//! gRPC 层的端到端测试：**真的起一个服务、真的走 TCP**。
//!
//! 只测 handler 逻辑没有意义——这一层唯一的价值就是「把 ServerStore 正确地暴露到网络上」，
//! 所以这里用内核分配的端口起真实服务，再用生成的客户端桩把一条完整链路跑一遍：
//! 登录 → 建库 → 两阶段提交 → 读日志/树/配额 → 清理与 GC → 审批。
//!
//! 重点验证四件事：
//!   1. 凭证从 metadata 走通，且没带凭证时是 `UNAUTHENTICATED`；
//!   2. 业务错误以响应内 `Status` + 正确的 `ErrorCode` 返回（不是 gRPC 错误串）；
//!   3. 两阶段提交的三段（Begin / Push 流 / Finalize）能跨网络拼起来；
//!   4. 成员门在网络上依然生效。

use std::sync::{Arc, Mutex};

use tonic::transport::Channel;
use tonic::Request;
use vcs_core::{hash_bytes, ChangeKind};
use vcs_grpc::{serve_in_background, Store};
use vcs_proto::vcs::repo::v1 as pb;
use vcs_proto::vcs::repo::v1::auth_service_client::AuthServiceClient;
use vcs_proto::vcs::repo::v1::repo_admin_service_client::RepoAdminServiceClient;
use vcs_proto::vcs::repo::v1::repo_service_client::RepoServiceClient;
use vcs_server::ServerStore;

const PASSWORD: &str = "Abcdef1!";
/// 与 `vcs_core::REPO_QUOTA_EXPANSION_STEP_BYTES` 对齐：一次扩容固定 10 GiB。
const EXPANSION_STEP: u64 = 10 * 1024 * 1024 * 1024;

struct Harness {
    root: std::path::PathBuf,
    addr: std::net::SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Harness {
    async fn start(tag: &str) -> Self {
        Self::start_inner(tag, true).await
    }

    /// 空库启动：用来验证「第一个注册 / 第一个 LDAP 登录自动成为管理员」的引导链路。
    async fn start_empty(tag: &str) -> Self {
        Self::start_inner(tag, false).await
    }

    async fn start_inner(tag: &str, with_admin: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "vcs-grpc-{tag}-{}-{}",
            std::process::id(),
            vcs_core::auth::random_token(4)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = ServerStore::open(root.join("store")).unwrap();
        // 服务端进程里通常由部署脚本做这一步；测试里直接建管理员。
        if with_admin {
            store.bootstrap_admin("alice", "张爱丽", PASSWORD).unwrap();
        }
        let store: Store = Arc::new(Mutex::new(store));
        let (addr, shutdown, _handle) = serve_in_background(store).await.unwrap();
        Self {
            root,
            addr,
            shutdown: Some(shutdown),
        }
    }

    async fn channel(&self) -> Channel {
        Channel::from_shared(format!("http://{}", self.addr))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    async fn auth(&self) -> AuthServiceClient<Channel> {
        AuthServiceClient::new(self.channel().await)
    }

    async fn repos(&self) -> RepoServiceClient<Channel> {
        RepoServiceClient::new(self.channel().await)
    }

    async fn admin(&self) -> RepoAdminServiceClient<Channel> {
        RepoAdminServiceClient::new(self.channel().await)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn bearer<T>(mut req: Request<T>, token: &str) -> Request<T> {
    req.metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    req
}

async fn login(h: &Harness, username: &str, password: &str) -> String {
    let resp = h
        .auth()
        .await
        .login(Request::new(pb::LoginRequest {
            username: username.into(),
            password: password.into(),
            source: pb::AuthSource::Local as i32,
            remember_me: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status.unwrap().code, pb::ErrorCode::Ok as i32);
    resp.access_token
}

#[tokio::test]
async fn login_requires_credentials_and_maps_business_errors() {
    let h = Harness::start("login").await;

    // 没带凭证访问需要鉴权的接口：必须是传输层 UNAUTHENTICATED，客户端据此跳登录页。
    let err = h
        .repos()
        .await
        .get_head(Request::new(pb::GetHeadRequest {
            repo_id: "repo_x".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 密码错属于业务错误：正常返回 200，错误码在 Status 里。
    let resp = h
        .auth()
        .await
        .login(Request::new(pb::LoginRequest {
            username: "alice".into(),
            password: "wrong".into(),
            source: pb::AuthSource::Local as i32,
            remember_me: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resp.status.unwrap().code,
        pb::ErrorCode::InvalidCredentials as i32,
        "错误码必须整体对齐 proto，不能因为 Rust 枚举从 0 开始就错一位"
    );

    // 令牌有效时 WhoAmI 能报出身份。
    let token = login(&h, "alice", PASSWORD).await;
    let me = h
        .auth()
        .await
        .who_am_i(bearer(Request::new(pb::WhoAmIRequest {}), &token))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.user.unwrap().username, "alice");
}

#[tokio::test]
async fn remember_me_signs_a_30_day_session() {
    let h = Harness::start("remember-me").await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // 不勾「记住我」：12 小时，CLI / Web 管理端 / 公共机器保持原来的安全边界。
    let short = h
        .auth()
        .await
        .login(Request::new(pb::LoginRequest {
            username: "alice".into(),
            password: PASSWORD.into(),
            source: pb::AuthSource::Local as i32,
            remember_me: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(short.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert!(
        (short.expires_at - now - vcs_core::auth::DEFAULT_TOKEN_TTL_SECONDS).abs() < 30,
        "默认会话应为 12 小时，实际 {}",
        short.expires_at - now
    );

    // 勾选后：30 天，且这张 token 立刻可用（不是仅仅把数字写大）。
    let long = h
        .auth()
        .await
        .login(Request::new(pb::LoginRequest {
            username: "alice".into(),
            password: PASSWORD.into(),
            source: pb::AuthSource::Local as i32,
            remember_me: true,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(long.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert!(
        (long.expires_at - now - vcs_core::auth::REMEMBER_ME_TTL_SECONDS).abs() < 30,
        "免登录会话应为 30 天，实际 {}",
        long.expires_at - now
    );
    let me = h
        .auth()
        .await
        .who_am_i(bearer(
            Request::new(pb::WhoAmIRequest {}),
            &long.access_token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.user.unwrap().username, "alice");
}

#[tokio::test]
async fn full_commit_round_trip_over_the_wire() {
    let h = Harness::start("commit").await;
    let token = login(&h, "alice", PASSWORD).await;

    // 建库（管理员直通）
    let repo = h
        .admin()
        .await
        .create_repo_direct(bearer(
            Request::new(pb::CreateRepoDirectRequest {
                repo_name: "reports".into(),
                repo_id_hint: String::new(),
                quota_bytes: 20 * 1024 * 1024,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(repo.status.unwrap().code, pb::ErrorCode::Ok as i32);
    let repo_id = repo.repo.unwrap().repo_id;

    // 两阶段提交：Begin 声明一个新增文件
    let content = b"october summary\n";
    let hash = hash_bytes(content);
    let begin = h
        .repos()
        .await
        .begin_commit(bearer(
            Request::new(pb::BeginCommitRequest {
                repo_id: repo_id.clone(),
                base_rev: 0,
                changes: vec![pb::Change {
                    kind: pb::ChangeKind::Add as i32,
                    path: "202610/summary.txt".into(),
                    old_hash: String::new(),
                    new_hash: vcs_core::to_hex(&hash),
                    from_path: String::new(),
                    props: Vec::new(),
                }],
                message: "首次提交".into(),
                estimated_new_bytes: content.len() as u64,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(begin.status.unwrap().code, pb::ErrorCode::Ok as i32);
    // 服务端还没有这个对象，必须让客户端推上来。
    assert_eq!(begin.missing_hashes, vec![vcs_core::to_hex(&hash)]);
    let session_id = begin.session_id;

    // Push 是客户端流：一批一条。
    let batch = pb::ObjectBatch {
        session_id: session_id.clone(),
        objects: vec![pb::Object {
            hash: vcs_core::to_hex(&hash),
            kind: pb::ObjectKind::Blob as i32,
            data: content.to_vec(),
            compressed: false,
        }],
    };
    let ack = h
        .repos()
        .await
        .push_objects(bearer(
            Request::new(tokio_stream::iter(vec![batch])),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ack.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(ack.received_objects, 1);
    assert_eq!(ack.received_bytes, content.len() as u64);

    // Finalize 分配版本号
    let finalize = h
        .repos()
        .await
        .finalize_commit(bearer(
            Request::new(pb::FinalizeCommitRequest {
                session_id: session_id.clone(),
                message: "首次提交".into(),
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(finalize.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(finalize.rev, 1);
    assert!(
        !finalize.root_tree.is_empty(),
        "响应必须带 root_tree 供客户端校验"
    );

    // 读侧：head / tree / log / 配额
    let head = h
        .repos()
        .await
        .get_head(bearer(
            Request::new(pb::GetHeadRequest {
                repo_id: repo_id.clone(),
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(head.rev, 1);

    let tree = h
        .repos()
        .await
        .get_tree(bearer(
            Request::new(pb::GetTreeRequest {
                repo_id: repo_id.clone(),
                rev: 0,
                paths: Vec::new(),
                depth: pb::Depth::Infinity as i32,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tree.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(tree.entries.len(), 1);
    assert_eq!(tree.entries[0].name, "202610/summary.txt");

    let log = h
        .repos()
        .await
        .get_log(bearer(
            Request::new(pb::GetLogRequest {
                repo_id: repo_id.clone(),
                from_rev: 0,
                to_rev: 0,
                path_prefix: String::new(),
                limit: 10,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(log.revisions.len(), 2, "r0 与 r1");
    assert_eq!(log.revisions[0].rev, 1);
    assert_eq!(log.revisions[0].message, "首次提交");

    let quota = h
        .repos()
        .await
        .get_quota(bearer(
            Request::new(pb::GetQuotaRequest {
                repo_id: repo_id.clone(),
                path_prefix: String::new(),
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(quota.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert!(quota.used_bytes > 0);
    assert_eq!(
        quota.effective.unwrap().source,
        pb::QuotaSource::RepoOverride as i32
    );

    // Checkout 是服务端流：把所有 blob 打回来
    let mut stream = h
        .repos()
        .await
        .checkout(bearer(
            Request::new(pb::CheckoutRequest {
                repo_id: repo_id.clone(),
                rev: 0,
                paths: Vec::new(),
                depth: pb::Depth::Infinity as i32,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    let first = tokio_stream::StreamExt::next(&mut stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.objects.len(), 1);
    assert_eq!(first.objects[0].data, content);
}

#[tokio::test]
async fn out_of_date_and_membership_are_reported_in_status() {
    let h = Harness::start("guard").await;
    let token = login(&h, "alice", PASSWORD).await;

    let repo = h
        .admin()
        .await
        .create_repo_direct(bearer(
            Request::new(pb::CreateRepoDirectRequest {
                repo_name: "guard".into(),
                repo_id_hint: String::new(),
                quota_bytes: 0,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner()
        .repo
        .unwrap();

    // 基于一个不存在的版本号提交：必须是 OUT_OF_DATE，客户端据此提示先 update。
    let resp = h
        .repos()
        .await
        .begin_commit(bearer(
            Request::new(pb::BeginCommitRequest {
                repo_id: repo.repo_id.clone(),
                base_rev: 99,
                changes: vec![pb::Change {
                    kind: pb::ChangeKind::Add as i32,
                    path: "a.txt".into(),
                    old_hash: String::new(),
                    new_hash: vcs_core::to_hex(&hash_bytes(b"a")),
                    from_path: String::new(),
                    props: Vec::new(),
                }],
                message: "过期提交".into(),
                estimated_new_bytes: 1,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status.unwrap().code, pb::ErrorCode::OutOfDate as i32);

    // 造一个非成员用户（管理员直接建号，绕过注册流程）
    {
        let store = ServerStore::open(h.root.join("store")).unwrap();
        let session = store.authenticate(&token).unwrap();
        store
            .create_user(&session, "bob", "李波", PASSWORD, vcs_core::Role::Writer)
            .unwrap();
    }
    let bob = login(&h, "bob", PASSWORD).await;
    let resp = h
        .repos()
        .await
        .get_head(bearer(
            Request::new(pb::GetHeadRequest {
                repo_id: repo.repo_id.clone(),
            }),
            &bob,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resp.status.unwrap().code,
        pb::ErrorCode::RepoNotMember as i32,
        "成员门必须在网络层同样生效"
    );

    // 成员管理：owner 加人之后就能读
    let users = h
        .admin()
        .await
        .search_users(bearer(
            Request::new(pb::SearchUsersRequest {
                query: "bob".into(),
                limit: 10,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(users.users.len(), 1);
    let bob_id = users.users[0].user_id.clone();
    let added = h
        .admin()
        .await
        .add_member(bearer(
            Request::new(pb::AddMemberRequest {
                repo_id: repo.repo_id.clone(),
                user_id: bob_id,
                role: pb::RepoMemberRole::Writer as i32,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(added.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(
        added.member.unwrap().role,
        pb::RepoMemberRole::Writer as i32
    );

    let resp = h
        .repos()
        .await
        .get_head(bearer(
            Request::new(pb::GetHeadRequest {
                repo_id: repo.repo_id.clone(),
            }),
            &bob,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.status.unwrap().code, pb::ErrorCode::Ok as i32);

    // ListRepos 只返回自己参与的仓库：bob 现在能看到，admin 也能。
    let bob_list = h
        .admin()
        .await
        .list_repos(bearer(
            Request::new(pb::ListReposRequest {
                filter: pb::RepoStatus::Active as i32,
            }),
            &bob,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(bob_list.repos.len(), 1);
}

#[tokio::test]
async fn admin_approval_and_purge_over_grpc() {
    let h = Harness::start("admin").await;
    let token = login(&h, "alice", PASSWORD).await;

    // 注册（自带密码）→ 审批 → 直接登录，全走网络接口
    let reg = h
        .auth()
        .await
        .register(Request::new(pb::RegisterRequest {
            username: "carol".into(),
            display_name: "王佳".into(),
            reason: "归档".into(),
            password: PASSWORD.into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reg.status.unwrap().code, pb::ErrorCode::Ok as i32);
    // 未审批前密码不管用。
    let early = h
        .auth()
        .await
        .login(Request::new(pb::LoginRequest {
            username: "carol".into(),
            password: PASSWORD.into(),
            source: pb::AuthSource::Local as i32,
            remember_me: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        early.status.unwrap().code,
        pb::ErrorCode::AccountPending as i32
    );

    // 弱密码必须在注册这一步就被服务端拒绝。
    let weak = h
        .auth()
        .await
        .register(Request::new(pb::RegisterRequest {
            username: "carol2".into(),
            display_name: "王佳2".into(),
            reason: "归档".into(),
            password: "short".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        weak.status.unwrap().code,
        pb::ErrorCode::PasswordPolicyViolation as i32
    );

    let approved = h
        .auth()
        .await
        .approve_registration(bearer(
            Request::new(pb::ApproveRegistrationRequest {
                request_id: reg.request_id.clone(),
                role: pb::Role::Writer as i32,
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(approved.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert!(!approved.user_id.is_empty());
    // 审批后直接能用注册时的密码登录，服务端不再下发任何凭证。
    let carol = login(&h, "carol", PASSWORD).await;

    // 仓库申请 → 审批 → 清理闭环。申请由 carol 本人提交，审批后她就是 owner。
    let req = h
        .admin()
        .await
        .request_repo(bearer(
            Request::new(pb::RequestRepoRequest {
                repo_name: "carol-repo".into(),
                repo_id_hint: String::new(),
                reason: "个人归档".into(),
            }),
            &carol,
        ))
        .await
        .unwrap()
        .into_inner();
    let repo = h
        .admin()
        .await
        .approve_repo_creation(bearer(
            Request::new(pb::ApproveRepoCreationRequest {
                request_id: req.request_id,
                decision_note: "同意".into(),
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(repo.status.unwrap().code, pb::ErrorCode::Ok as i32);
    let repo_id = repo.repo.unwrap().repo_id;

    // 扩容申请：容量不填，服务端固定 +10 GiB，由管理员按磁盘余量审批。
    let expansion = h
        .admin()
        .await
        .request_repo_expansion(bearer(
            Request::new(pb::RequestRepoExpansionRequest {
                repo_id: repo_id.clone(),
                reason: "素材变多".into(),
            }),
            &carol,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        expansion.status.unwrap().code,
        pb::ErrorCode::Ok as i32,
        "扩容申请应被接受"
    );

    let mine = h
        .admin()
        .await
        .list_repo_quota_requests(bearer(
            Request::new(pb::ListRepoQuotaRequestsRequest {
                filter: pb::RepoCreationStatus::Pending as i32,
                limit: 10,
            }),
            &carol,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mine.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(mine.requests.len(), 1);
    let pending = &mine.requests[0];
    assert_eq!(pending.increment_bytes, EXPANSION_STEP);
    assert_eq!(
        pending.target_quota_bytes,
        pending.quota_before_bytes + EXPANSION_STEP
    );

    // 磁盘容量是宿主机信息：普通用户看不到，管理员能看到。
    let denied = h
        .admin()
        .await
        .get_storage_capacity(bearer(
            Request::new(pb::GetStorageCapacityRequest {}),
            &carol,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        denied.status.unwrap().code,
        pb::ErrorCode::PermissionDenied as i32
    );

    let capacity = h
        .admin()
        .await
        .get_storage_capacity(bearer(
            Request::new(pb::GetStorageCapacityRequest {}),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(capacity.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert!(capacity.capacity.unwrap().total_bytes > 0);

    let approved_expansion = h
        .admin()
        .await
        .approve_repo_quota_request(bearer(
            Request::new(pb::ApproveRepoQuotaRequestRequest {
                request_id: pending.request_id.clone(),
                decision_note: "同意".into(),
            }),
            &token,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        approved_expansion.status.unwrap().code,
        pb::ErrorCode::Ok as i32
    );
    assert_eq!(
        approved_expansion.quota_bytes,
        pending.quota_before_bytes + EXPANSION_STEP
    );

    let estimate = h
        .repos()
        .await
        .estimate_purge(bearer(
            Request::new(pb::EstimatePurgeRequest {
                repo_id: repo_id.clone(),
                paths: vec!["202610".into()],
            }),
            &carol,
        ))
        .await
        .unwrap()
        .into_inner();
    // 空仓库里没有这个目录：dry-run 仍应成功返回（0 字节），而不是报错。
    assert_eq!(estimate.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(estimate.reclaimable_bytes, 0);

    // 尚未实现的方法明确返回 UNIMPLEMENTED，而不是悄悄成功。
    let err = h
        .repos()
        .await
        .lock(bearer(
            Request::new(pb::LockRequest {
                repo_id,
                paths: vec!["a.txt".into()],
                lease_seconds: 60,
            }),
            &token,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unimplemented);

    // ChangeKind 常量与服务器实现无关，但这里顺手钉住导入没被优化掉。
    let _ = ChangeKind::Add;
}

#[tokio::test]
async fn member_and_repo_admin_rpcs_work_over_the_wire() {
    let h = Harness::start("admin2").await;
    let alice = login(&h, "alice", PASSWORD).await;

    // 走完整注册流程造一个真实用户（没有「直接建号」的 RPC，管理员建号只在 CLI 侧）
    let reg = h
        .auth()
        .await
        .register(Request::new(pb::RegisterRequest {
            username: "dave".into(),
            display_name: "赵大".into(),
            reason: "协作".into(),
            password: PASSWORD.into(),
        }))
        .await
        .unwrap()
        .into_inner();
    let approved = h
        .auth()
        .await
        .approve_registration(bearer(
            Request::new(pb::ApproveRegistrationRequest {
                request_id: reg.request_id,
                role: pb::Role::Writer as i32,
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(approved.status.unwrap().code, pb::ErrorCode::Ok as i32);
    let dave = login(&h, "dave", PASSWORD).await;

    let repo = h
        .admin()
        .await
        .create_repo_direct(bearer(
            Request::new(pb::CreateRepoDirectRequest {
                repo_name: "team".into(),
                repo_id_hint: String::new(),
                quota_bytes: 0,
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner()
        .repo
        .unwrap();
    let repo_id = repo.repo_id;

    let dave_id = h
        .admin()
        .await
        .search_users(bearer(
            Request::new(pb::SearchUsersRequest {
                query: "dave".into(),
                limit: 10,
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner()
        .users[0]
        .user_id
        .clone();

    // 加成员 → 改角色 → 移除，三步都从网络接口走
    let added = h
        .admin()
        .await
        .add_member(bearer(
            Request::new(pb::AddMemberRequest {
                repo_id: repo_id.clone(),
                user_id: dave_id.clone(),
                role: pb::RepoMemberRole::Writer as i32,
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(added.status.unwrap().code, pb::ErrorCode::Ok as i32);

    let changed = h
        .admin()
        .await
        .set_member_role(bearer(
            Request::new(pb::SetMemberRoleRequest {
                repo_id: repo_id.clone(),
                user_id: dave_id.clone(),
                role: pb::RepoMemberRole::Reader as i32,
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(changed.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(
        changed.member.unwrap().role,
        pb::RepoMemberRole::Reader as i32
    );

    let removed = h
        .admin()
        .await
        .remove_member(bearer(
            Request::new(pb::RemoveMemberRequest {
                repo_id: repo_id.clone(),
                user_id: dave_id.clone(),
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(removed.status.unwrap().code, pb::ErrorCode::Ok as i32);

    // 被移除后立刻失去访问权
    let head = h
        .repos()
        .await
        .get_head(bearer(
            Request::new(pb::GetHeadRequest {
                repo_id: repo_id.clone(),
            }),
            &dave,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        head.status.unwrap().code,
        pb::ErrorCode::RepoNotMember as i32
    );

    // 配额覆盖：设置 → 读取 → 恢复继承
    let quota = h
        .admin()
        .await
        .set_repo_quota(bearer(
            Request::new(pb::SetRepoQuotaRequest {
                repo_id: repo_id.clone(),
                bytes: 1024 * 1024,
                inherit: false,
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(quota.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(
        quota.effective.unwrap().source,
        pb::QuotaSource::RepoOverride as i32
    );
    let read_back = h
        .admin()
        .await
        .get_repo_quota(bearer(
            Request::new(pb::GetRepoQuotaRequest {
                repo_id: repo_id.clone(),
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read_back.repo_bytes, 1024 * 1024);

    // 禁用仓库：读写立即被拒
    let disabled = h
        .admin()
        .await
        .set_repo_status(bearer(
            Request::new(pb::SetRepoStatusRequest {
                repo_id: repo_id.clone(),
                status: pb::RepoStatus::Disabled as i32,
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(disabled.status.unwrap().code, pb::ErrorCode::Ok as i32);
    let blocked = h
        .repos()
        .await
        .get_head(bearer(
            Request::new(pb::GetHeadRequest {
                repo_id: repo_id.clone(),
            }),
            &alice,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        blocked.status.unwrap().code,
        pb::ErrorCode::RepoDisabled as i32
    );

    // 恢复 active 后可继续访问
    h.admin()
        .await
        .set_repo_status(bearer(
            Request::new(pb::SetRepoStatusRequest {
                repo_id: repo_id.clone(),
                status: pb::RepoStatus::Active as i32,
            }),
            &alice,
        ))
        .await
        .unwrap();
    let ok = h
        .repos()
        .await
        .get_head(bearer(Request::new(pb::GetHeadRequest { repo_id }), &alice))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ok.status.unwrap().code, pb::ErrorCode::Ok as i32);
}

#[tokio::test]
async fn first_registration_over_grpc_auto_approves_the_admin() {
    let h = Harness::start_empty("first-user-grpc").await;

    // 目录未配置：LDAP 登录必须明确报「不可用」，绝不退化成「校验通过」。
    let unconfigured = h
        .auth()
        .await
        .login(Request::new(pb::LoginRequest {
            username: "alice".into(),
            password: PASSWORD.into(),
            source: pb::AuthSource::Ldap as i32,
            remember_me: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        unconfigured.status.unwrap().code,
        pb::ErrorCode::LdapUnavailable as i32
    );
    assert!(unconfigured.access_token.is_empty());

    // 空库第一个注册：自动批准为 admin，注册时设置的密码立即可用。
    let reg = h
        .auth()
        .await
        .register(Request::new(pb::RegisterRequest {
            username: "alice".into(),
            display_name: "张爱丽".into(),
            reason: "首位用户".into(),
            password: PASSWORD.into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reg.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert!(reg.auto_approved);
    assert_eq!(reg.state, pb::RegistrationStatus::Approved as i32);
    assert_eq!(reg.granted_role, pb::Role::Admin as i32);

    let logged = h
        .auth()
        .await
        .login(Request::new(pb::LoginRequest {
            username: "alice".into(),
            password: PASSWORD.into(),
            source: pb::AuthSource::Local as i32,
            remember_me: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(logged.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert_eq!(logged.user.unwrap().role, pb::Role::Admin as i32);

    // 第二个人仍是普通待审批申请，审批后才可用同一份密码登录。
    let second = h
        .auth()
        .await
        .register(Request::new(pb::RegisterRequest {
            username: "bob".into(),
            display_name: "李波".into(),
            reason: "申请".into(),
            password: PASSWORD.into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(second.status.unwrap().code, pb::ErrorCode::Ok as i32);
    assert!(!second.auto_approved);
    assert_eq!(second.state, pb::RegistrationStatus::Pending as i32);
}
