//! svn-next gRPC 服务层（M3）。
//!
//! 这一层只做三件事：**解析参数、转换类型、映射错误**。所有业务语义都在
//! `vcs-server::ServerStore` 里，CLI 与 gRPC 调的是同一套方法——不存在
//! 「命令行能提交、网络客户端提交不了」这种分叉。
//!
//! ## 错误分层
//!
//! - **业务拒绝**（配额超限、out-of-date、非成员……）：放在响应的 `Status` 字段里正常返回。
//!   proto 里每个响应都带 `Status`，客户端按 `ErrorCode` 分支即可，不需要解析 gRPC 错误串。
//! - **传输层错误**：只有三种情况用 `tonic::Status`——没带凭证 / 凭证无效（`UNAUTHENTICATED`）、
//!   方法尚未实现（`UNIMPLEMENTED`）、流式接口中途失败（流里没有 `Status` 字段可放）。
//!
//! ## 并发模型
//!
//! `rusqlite::Connection` 是 `Send` 但不是 `Sync`，所以整层用一把 `Mutex` 把 `ServerStore`
//! 串行化。SQLite 本身只有单写入者，M3 的单机形态下这是够用的；真要并发得换连接池 +
//! `spawn_blocking`，那是 M4 之后的事（见设计 §14 风险表）。

// tonic 的 handler 必须返回 `Result<_, tonic::Status>`，而 Status 有 176 字节。
// clippy 建议装箱，但那会与生成的 trait 签名打架，逐处转换只会更难读，因此整体放行。
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};
use vcs_core::{
    ChangeKind, EntryKind, Error, ErrorCode, ObjectStore, RepoMemberRole, RepoStatus, Role,
};
use vcs_proto::vcs::repo::v1 as pb;
use vcs_proto::vcs::repo::v1::auth_service_server::{AuthService, AuthServiceServer};
use vcs_proto::vcs::repo::v1::repo_admin_service_server::{
    RepoAdminService, RepoAdminServiceServer,
};
use vcs_proto::vcs::repo::v1::repo_service_server::{RepoService, RepoServiceServer};
use vcs_server::{authenticate_ldap, test_ldap_connection, LdapSettings, ServerStore, TlsMode};

mod admin;

pub type Store = Arc<Mutex<ServerStore>>;

/// 单批对象大小：与 CLI 的推送分片保持一致，避免两套「批量」定义。
const BATCH_OBJECTS: usize = 32;
const BATCH_BYTES: usize = 4 * 1024 * 1024;

/// 把 `ServerStore` 包成三个 gRPC service。
#[derive(Clone)]
pub struct Api {
    store: Store,
}

impl Api {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, ServerStore> {
        // 中毒说明有人 panic 在持锁期间；继续用可能读到半截状态，因此直接恢复不变量。
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 从 metadata 取凭证并校验。`authorization: Bearer <token>`，也接受裸 token。
    fn session<T>(&self, req: &Request<T>) -> Result<vcs_server::Session, Status> {
        let raw = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Status::unauthenticated("缺少 authorization metadata"))?;
        let token = raw
            .strip_prefix("Bearer ")
            .or_else(|| raw.strip_prefix("bearer "))
            .unwrap_or(raw)
            .trim();
        if token.is_empty() {
            return Err(Status::unauthenticated("凭证为空"));
        }
        self.store()
            .authenticate(token)
            .map_err(|e| Status::unauthenticated(format!("{:?}: {}", e.code, e.message)))
    }
}

// ---------------- 状态与类型转换 ----------------

fn status_ok() -> pb::Status {
    pb::Status {
        code: pb::ErrorCode::Ok as i32,
        message: String::new(),
        quota: None,
        candidates: Vec::new(),
    }
}

/// `ErrorCode` → proto 枚举编号。
///
/// **不能写 `code as i32`**：Rust 侧没有显式判别值，`Ok` 是 0，而 proto 的 0 是
/// `UNSPECIFIED`、`OK` 是 1——直接转换会让所有错误码整体错一位，客户端把
/// 「配额超限」当成「入参非法」。这个 match 是穷尽的，新增变体会编译失败，
/// 从而强制作者回来对齐编号。
fn error_number(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::Ok => pb::ErrorCode::Ok as i32,
        ErrorCode::OutOfDate => pb::ErrorCode::OutOfDate as i32,
        ErrorCode::PathConflict => pb::ErrorCode::PathConflict as i32,
        ErrorCode::TreeConflict => pb::ErrorCode::TreeConflict as i32,
        ErrorCode::PermissionDenied => pb::ErrorCode::PermissionDenied as i32,
        ErrorCode::QuotaExceeded => pb::ErrorCode::QuotaExceeded as i32,
        ErrorCode::ContentPurged => pb::ErrorCode::ContentPurged as i32,
        ErrorCode::Locked => pb::ErrorCode::Locked as i32,
        ErrorCode::ObjectMissing => pb::ErrorCode::ObjectMissing as i32,
        ErrorCode::InvalidSession => pb::ErrorCode::InvalidSession as i32,
        ErrorCode::Unauthenticated => pb::ErrorCode::Unauthenticated as i32,
        ErrorCode::InvalidCredentials => pb::ErrorCode::InvalidCredentials as i32,
        ErrorCode::AccountPending => pb::ErrorCode::AccountPending as i32,
        ErrorCode::AccountDisabled => pb::ErrorCode::AccountDisabled as i32,
        ErrorCode::RegistrationExists => pb::ErrorCode::RegistrationExists as i32,
        ErrorCode::ApprovalNotFound => pb::ErrorCode::ApprovalNotFound as i32,
        ErrorCode::SetupTokenInvalid => pb::ErrorCode::SetupTokenInvalid as i32,
        ErrorCode::LdapUnavailable => pb::ErrorCode::LdapUnavailable as i32,
        ErrorCode::TokenExpired => pb::ErrorCode::TokenExpired as i32,
        ErrorCode::PatRevoked => pb::ErrorCode::PatRevoked as i32,
        ErrorCode::RepoNotFound => pb::ErrorCode::RepoNotFound as i32,
        ErrorCode::RepoDisabled => pb::ErrorCode::RepoDisabled as i32,
        ErrorCode::RepoArchived => pb::ErrorCode::RepoArchived as i32,
        ErrorCode::RepoNameInvalid => pb::ErrorCode::RepoNameInvalid as i32,
        ErrorCode::RepoCreationExists => pb::ErrorCode::RepoCreationExists as i32,
        ErrorCode::RepoApprovalNotFound => pb::ErrorCode::RepoApprovalNotFound as i32,
        ErrorCode::PasswordPolicyViolation => pb::ErrorCode::PasswordPolicyViolation as i32,
        ErrorCode::RepoNotMember => pb::ErrorCode::RepoNotMember as i32,
        ErrorCode::RepoMemberExists => pb::ErrorCode::RepoMemberExists as i32,
        ErrorCode::RepoMemberNotFound => pb::ErrorCode::RepoMemberNotFound as i32,
        ErrorCode::RepoOwnerProtected => pb::ErrorCode::RepoOwnerProtected as i32,
        ErrorCode::RepoMemberForbidden => pb::ErrorCode::RepoMemberForbidden as i32,
        ErrorCode::UserNotFound => pb::ErrorCode::UserNotFound as i32,
        ErrorCode::QuotaConfigInvalid => pb::ErrorCode::QuotaConfigInvalid as i32,
        ErrorCode::IoError => pb::ErrorCode::IoError as i32,
        ErrorCode::InvalidArgument => pb::ErrorCode::InvalidArgument as i32,
    }
}

fn status_err(err: &vcs_core::Error) -> pb::Status {
    pb::Status {
        code: error_number(err.code),
        message: err.message.clone(),
        quota: None,
        candidates: Vec::new(),
    }
}

fn unimplemented<T>(what: &str) -> Result<Response<T>, Status> {
    Err(Status::unimplemented(format!(
        "{what} 尚未实现（见设计 §12 M3 剩余项）"
    )))
}

fn role_to_pb(role: Role) -> i32 {
    match role {
        Role::Readonly => pb::Role::Readonly as i32,
        Role::Writer => pb::Role::Writer as i32,
        Role::Admin => pb::Role::Admin as i32,
    }
}

fn parse_role(value: i32) -> Result<Role, Status> {
    match pb::Role::try_from(value).unwrap_or(pb::Role::Unspecified) {
        pb::Role::Admin => Ok(Role::Admin),
        pb::Role::Writer => Ok(Role::Writer),
        pb::Role::Readonly => Ok(Role::Readonly),
        _ => Err(Status::invalid_argument("未知的全局角色")),
    }
}

fn member_role_to_pb(role: RepoMemberRole) -> i32 {
    match role {
        RepoMemberRole::Owner => pb::RepoMemberRole::Owner as i32,
        RepoMemberRole::Maintainer => pb::RepoMemberRole::Maintainer as i32,
        RepoMemberRole::Writer => pb::RepoMemberRole::Writer as i32,
        RepoMemberRole::Reader => pb::RepoMemberRole::Reader as i32,
    }
}

fn parse_member_role(value: i32) -> Result<RepoMemberRole, Status> {
    match pb::RepoMemberRole::try_from(value).unwrap_or(pb::RepoMemberRole::Unspecified) {
        pb::RepoMemberRole::Owner => Ok(RepoMemberRole::Owner),
        pb::RepoMemberRole::Maintainer => Ok(RepoMemberRole::Maintainer),
        pb::RepoMemberRole::Writer => Ok(RepoMemberRole::Writer),
        pb::RepoMemberRole::Reader => Ok(RepoMemberRole::Reader),
        _ => Err(Status::invalid_argument("未知的成员角色")),
    }
}

fn repo_status_to_pb(status: RepoStatus) -> i32 {
    match status {
        RepoStatus::Active => pb::RepoStatus::Active as i32,
        RepoStatus::Disabled => pb::RepoStatus::Disabled as i32,
        RepoStatus::Archived => pb::RepoStatus::Archived as i32,
    }
}

fn parse_repo_status(value: i32) -> Option<RepoStatus> {
    match pb::RepoStatus::try_from(value).unwrap_or(pb::RepoStatus::Unspecified) {
        pb::RepoStatus::Active => Some(RepoStatus::Active),
        pb::RepoStatus::Disabled => Some(RepoStatus::Disabled),
        pb::RepoStatus::Archived => Some(RepoStatus::Archived),
        _ => None,
    }
}

fn account_status_to_pb(status: vcs_core::AccountStatus) -> i32 {
    match status {
        vcs_core::AccountStatus::Pending => pb::AccountStatus::Pending as i32,
        vcs_core::AccountStatus::Active => pb::AccountStatus::Active as i32,
        vcs_core::AccountStatus::Disabled => pb::AccountStatus::Disabled as i32,
        vcs_core::AccountStatus::Rejected => pb::AccountStatus::Rejected as i32,
        vcs_core::AccountStatus::Expired => pb::AccountStatus::Expired as i32,
    }
}

fn reg_status_to_pb(status: vcs_core::RegistrationStatus) -> i32 {
    match status {
        vcs_core::RegistrationStatus::Pending => pb::RegistrationStatus::Pending as i32,
        vcs_core::RegistrationStatus::Approved => pb::RegistrationStatus::Approved as i32,
        vcs_core::RegistrationStatus::Rejected => pb::RegistrationStatus::Rejected as i32,
        vcs_core::RegistrationStatus::Expired => pb::RegistrationStatus::Expired as i32,
    }
}

fn parse_reg_status(value: i32) -> Option<vcs_core::RegistrationStatus> {
    match pb::RegistrationStatus::try_from(value).unwrap_or(pb::RegistrationStatus::Unspecified) {
        pb::RegistrationStatus::Pending => Some(vcs_core::RegistrationStatus::Pending),
        pb::RegistrationStatus::Approved => Some(vcs_core::RegistrationStatus::Approved),
        pb::RegistrationStatus::Rejected => Some(vcs_core::RegistrationStatus::Rejected),
        pb::RegistrationStatus::Expired => Some(vcs_core::RegistrationStatus::Expired),
        _ => None,
    }
}

fn creation_status_to_pb(status: vcs_core::RepoCreationStatus) -> i32 {
    match status {
        vcs_core::RepoCreationStatus::Pending => pb::RepoCreationStatus::Pending as i32,
        vcs_core::RepoCreationStatus::Approved => pb::RepoCreationStatus::Approved as i32,
        vcs_core::RepoCreationStatus::Rejected => pb::RepoCreationStatus::Rejected as i32,
        vcs_core::RepoCreationStatus::Expired => pb::RepoCreationStatus::Expired as i32,
    }
}

fn parse_creation_status(value: i32) -> Option<vcs_core::RepoCreationStatus> {
    match pb::RepoCreationStatus::try_from(value).unwrap_or(pb::RepoCreationStatus::Unspecified) {
        pb::RepoCreationStatus::Pending => Some(vcs_core::RepoCreationStatus::Pending),
        pb::RepoCreationStatus::Approved => Some(vcs_core::RepoCreationStatus::Approved),
        pb::RepoCreationStatus::Rejected => Some(vcs_core::RepoCreationStatus::Rejected),
        pb::RepoCreationStatus::Expired => Some(vcs_core::RepoCreationStatus::Expired),
        _ => None,
    }
}

fn change_kind_to_pb(kind: ChangeKind) -> i32 {
    match kind {
        ChangeKind::Add => pb::ChangeKind::Add as i32,
        ChangeKind::Modify => pb::ChangeKind::Modify as i32,
        ChangeKind::Delete => pb::ChangeKind::Delete as i32,
        ChangeKind::Rename => pb::ChangeKind::Rename as i32,
        ChangeKind::Copy => pb::ChangeKind::Copy as i32,
    }
}

fn entry_kind_to_pb(kind: EntryKind) -> i32 {
    match kind {
        EntryKind::File => pb::EntryKind::File as i32,
        EntryKind::Dir => pb::EntryKind::Dir as i32,
        EntryKind::Symlink => pb::EntryKind::Symlink as i32,
    }
}

/// 变更的条目类型通过 `props` 承载。
///
/// proto 的 `Change` 没有 entry_kind 字段（M0 定稿时只有文件），但符号链接必须原样带过去，
/// 否则提交后会退化成普通文件。约定：`props == b"symlink"` 表示符号链接，其余按普通文件处理。
/// 真正新增字段属于破坏契约的改动，留给有明确需求时再做。
fn change_entry_kind(props: &[u8]) -> EntryKind {
    if props == b"symlink" {
        EntryKind::Symlink
    } else {
        EntryKind::File
    }
}

fn entry_kind_props(kind: EntryKind) -> Vec<u8> {
    match kind {
        EntryKind::Symlink => b"symlink".to_vec(),
        _ => Vec::new(),
    }
}

fn revision_to_pb(rev: &vcs_server::RevisionInfo) -> pb::RevisionInfo {
    pb::RevisionInfo {
        rev: rev.rev,
        root_tree: vcs_core::to_hex(&rev.root_tree),
        parent_rev: rev.parent_rev,
        author: rev.author.clone(),
        message: rev.message.clone(),
        committed_at: rev.committed_at,
        change_count: rev.change_count,
    }
}

fn repo_to_pb(repo: &vcs_server::RepoInfo) -> pb::RepoInfo {
    pb::RepoInfo {
        repo_id: repo.repo_id.clone(),
        name: repo.name.clone(),
        status: repo_status_to_pb(repo.status),
        owner_user_id: repo.owner_user_id.clone(),
        head_rev: repo.head_rev,
        quota_bytes: 0,
        created_at: 0,
    }
}

fn user_to_pb(user: &vcs_server::UserInfo) -> pb::UserInfo {
    pb::UserInfo {
        user_id: user.user_id.clone(),
        username: user.username.clone(),
        display_name: user.display_name.clone(),
        source: pb::AuthSource::Local as i32,
        status: account_status_to_pb(user.status),
        role: role_to_pb(user.role),
        last_login_at: 0,
    }
}

fn registration_to_pb(info: &vcs_server::RegistrationInfo) -> pb::RegistrationInfo {
    pb::RegistrationInfo {
        request_id: info.request_id.clone(),
        username: info.username.clone(),
        display_name: info.display_name.clone(),
        reason: info.reason.clone().unwrap_or_default(),
        state: reg_status_to_pb(info.state),
        submitted_at: info.submitted_at,
        decided_at: info.decided_at.unwrap_or(0),
        decided_by: info.decided_by.clone().unwrap_or_default(),
        decision_note: info.decision_note.clone().unwrap_or_default(),
    }
}

fn request_to_pb(info: &vcs_server::RepoRequestInfo) -> pb::RepoCreationInfo {
    pb::RepoCreationInfo {
        request_id: info.request_id.clone(),
        requester_user_id: info.requester_user_id.clone(),
        repo_name: info.repo_name.clone(),
        repo_id_hint: String::new(),
        reason: info.reason.clone().unwrap_or_default(),
        requested_quota_bytes: info.requested_quota_bytes,
        state: creation_status_to_pb(info.state),
        submitted_at: info.submitted_at,
        decided_at: info.decided_at.unwrap_or(0),
        decided_by: info.decided_by.clone().unwrap_or_default(),
        decision_note: info.decision_note.clone().unwrap_or_default(),
        expires_at: info.expires_at,
        approved_repo_id: info.approved_repo_id.clone().unwrap_or_default(),
    }
}

fn member_to_pb(member: &vcs_server::MemberInfo) -> pb::RepoMemberInfo {
    pb::RepoMemberInfo {
        repo_id: String::new(),
        user_id: member.user_id.clone(),
        username: member.username.clone(),
        display_name: member.display_name.clone(),
        role: member_role_to_pb(member.role),
        added_by: String::new(),
        added_at: 0,
    }
}

fn quota_source_to_pb(source: vcs_core::QuotaSource) -> i32 {
    match source {
        vcs_core::QuotaSource::RepoOverride => pb::QuotaSource::RepoOverride as i32,
        vcs_core::QuotaSource::GlobalDefault => pb::QuotaSource::GlobalDefault as i32,
        vcs_core::QuotaSource::Fallback => pb::QuotaSource::Fallback as i32,
    }
}

/// 把对象列表切成批次：单批 32 个 / 4 MiB，与 CLI 推送节奏一致。
fn batches(objects: Vec<pb::Object>) -> Vec<pb::ObjectBatch> {
    let mut out = Vec::new();
    let mut current: Vec<pb::Object> = Vec::new();
    let mut bytes = 0usize;
    for object in objects {
        bytes += object.data.len();
        current.push(object);
        if current.len() >= BATCH_OBJECTS || bytes >= BATCH_BYTES {
            out.push(pb::ObjectBatch {
                session_id: String::new(),
                objects: std::mem::take(&mut current),
            });
            bytes = 0;
        }
    }
    if !current.is_empty() {
        out.push(pb::ObjectBatch {
            session_id: String::new(),
            objects: current,
        });
    }
    out
}

// ---------------- AuthService ----------------

/// 内部配置 → proto。bind 密码刻意不在 `LdapConfig` 里，回传永远不含明文。
fn ldap_settings_to_pb(cfg: &LdapSettings) -> pb::LdapConfig {
    pb::LdapConfig {
        host: cfg.host.clone(),
        port: u32::from(cfg.port),
        tls_mode: cfg.tls_mode.as_str().into(),
        base_dn: cfg.base_dn.clone(),
        bind_dn: cfg.bind_dn.clone(),
        user_filter: cfg.user_filter.clone(),
        group_filter: cfg.group_filter.clone(),
        attr_username: cfg.attr_username.clone(),
        attr_display_name: cfg.attr_display_name.clone(),
        enabled: cfg.enabled,
        group_roles: cfg
            .group_roles
            .iter()
            .map(|(dn, role)| pb::GroupRole {
                group_dn: dn.clone(),
                role: role_to_pb(*role),
            })
            .collect(),
    }
}

fn pb_to_ldap_settings(
    cfg: &pb::LdapConfig,
    bind_password: String,
) -> std::result::Result<LdapSettings, Error> {
    let mut group_roles = Vec::with_capacity(cfg.group_roles.len());
    for entry in &cfg.group_roles {
        let role = match pb::Role::try_from(entry.role).unwrap_or(pb::Role::Unspecified) {
            pb::Role::Admin => Role::Admin,
            pb::Role::Writer => Role::Writer,
            pb::Role::Readonly => Role::Readonly,
            _ => {
                return Err(Error::new(
                    ErrorCode::InvalidArgument,
                    format!("组 {} 映射了未知角色", entry.group_dn),
                ))
            }
        };
        group_roles.push((entry.group_dn.clone(), role));
    }
    Ok(LdapSettings {
        host: cfg.host.clone(),
        port: cfg.port as u16,
        tls_mode: TlsMode::parse(&cfg.tls_mode)?,
        base_dn: cfg.base_dn.clone(),
        bind_dn: cfg.bind_dn.clone(),
        bind_password,
        user_filter: if cfg.user_filter.is_empty() {
            "(uid={username})".into()
        } else {
            cfg.user_filter.clone()
        },
        group_filter: cfg.group_filter.clone(),
        attr_username: if cfg.attr_username.is_empty() {
            "uid".into()
        } else {
            cfg.attr_username.clone()
        },
        attr_display_name: if cfg.attr_display_name.is_empty() {
            "cn".into()
        } else {
            cfg.attr_display_name.clone()
        },
        enabled: cfg.enabled,
        group_roles,
    })
}

/// bind 密码留空表示「沿用已保存的」，避免前端为了改一个开关重新明文传一遍口令。
fn resolve_bind_password(
    store: &ServerStore,
    supplied: &str,
) -> std::result::Result<String, Error> {
    if !supplied.is_empty() {
        return Ok(supplied.to_string());
    }
    Ok(store
        .ldap_config()?
        .map(|cfg| cfg.bind_password)
        .unwrap_or_default())
}

pub struct AuthApi {
    api: Api,
}

impl AuthApi {
    pub fn new(store: Store) -> Self {
        Self {
            api: Api::new(store),
        }
    }

    /// 目录登录：配置读取 → 阻塞线程里做 bind → 回到存储层 JIT 建号并签发 JWT。
    ///
    /// 目录服务不可用时只影响这一条请求，本地账号登录路径完全不受影响。
    async fn login_with_ldap(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Response<pb::LoginResponse>, Status> {
        let cfg = {
            let store = self.api.store();
            match store.ldap_config() {
                Ok(cfg) => cfg,
                Err(err) => {
                    return Ok(Response::new(pb::LoginResponse {
                        status: Some(status_err(&err)),
                        ..Default::default()
                    }))
                }
            }
        };
        let Some(cfg) = cfg.filter(|c| c.enabled) else {
            return Ok(Response::new(pb::LoginResponse {
                status: Some(pb::Status {
                    code: pb::ErrorCode::LdapUnavailable as i32,
                    message: "LDAP 登录尚未配置或未启用，请先让管理员配置目录服务".into(),
                    quota: None,
                    candidates: Vec::new(),
                }),
                ..Default::default()
            }));
        };

        let user = username.to_string();
        let pass = password.to_string();
        // ldap3 的同步接口会自建 tokio runtime，绝不能在 async 线程里直接调用。
        let profile = match tokio::task::spawn_blocking(move || {
            authenticate_ldap(&cfg, &user, &pass)
        })
        .await
        {
            Ok(Ok(profile)) => profile,
            Ok(Err(err)) => {
                return Ok(Response::new(pb::LoginResponse {
                    status: Some(status_err(&err)),
                    ..Default::default()
                }))
            }
            Err(join) => {
                return Ok(Response::new(pb::LoginResponse {
                    status: Some(pb::Status {
                        code: pb::ErrorCode::LdapUnavailable as i32,
                        message: format!("目录认证任务异常：{join}"),
                        quota: None,
                        candidates: Vec::new(),
                    }),
                    ..Default::default()
                }))
            }
        };

        let store = self.api.store();
        match store.login_ldap(&profile) {
            Ok((token, user)) => Ok(Response::new(pb::LoginResponse {
                status: Some(status_ok()),
                access_token: token,
                refresh_token: String::new(),
                expires_at: now_secs() + vcs_core::auth::DEFAULT_TOKEN_TTL_SECONDS,
                user: Some(user_to_pb(&user)),
            })),
            Err(err) => Ok(Response::new(pb::LoginResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }
}

#[tonic::async_trait]
impl AuthService for AuthApi {
    async fn login(
        &self,
        request: Request<pb::LoginRequest>,
    ) -> Result<Response<pb::LoginResponse>, Status> {
        let req = request.into_inner();
        if req.source == pb::AuthSource::Ldap as i32 {
            return self.login_with_ldap(&req.username, &req.password).await;
        }
        let now = now_secs();
        match self.api.store().login(&req.username, &req.password) {
            Ok((token, user)) => Ok(Response::new(pb::LoginResponse {
                status: Some(status_ok()),
                access_token: token,
                refresh_token: String::new(),
                expires_at: now + vcs_core::auth::DEFAULT_TOKEN_TTL_SECONDS,
                user: Some(user_to_pb(&user)),
            })),
            Err(err) => Ok(Response::new(pb::LoginResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn register(
        &self,
        request: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        let req = request.into_inner();
        match self.api.store().register(
            &req.username,
            &req.display_name,
            &req.password,
            &req.reason,
        ) {
            Ok(outcome) => {
                // 首位用户：自动批准为管理员，注册时设置的密码立即可用。
                let first = outcome.first_user;
                Ok(Response::new(pb::RegisterResponse {
                    status: Some(status_ok()),
                    request_id: outcome.info.request_id,
                    state: reg_status_to_pb(outcome.info.state),
                    auto_approved: first,
                    granted_role: if first {
                        pb::Role::Admin as i32
                    } else {
                        pb::Role::Unspecified as i32
                    },
                }))
            }
            Err(err) => Ok(Response::new(pb::RegisterResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn get_registration(
        &self,
        request: Request<pb::GetRegistrationRequest>,
    ) -> Result<Response<pb::GetRegistrationResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let store = self.api.store();
        // 复用列表接口按用户名过滤即可：注册申请与用户名一一对应（同一个用户名只能有一张待审批单）。
        let result = store
            .list_registrations(&session, None, 200)
            .map(|list| list.into_iter().find(|r| r.request_id == req.request_id));
        match result {
            Ok(Some(info)) => Ok(Response::new(pb::GetRegistrationResponse {
                status: Some(status_ok()),
                request: Some(registration_to_pb(&info)),
            })),
            Ok(None) => Ok(Response::new(pb::GetRegistrationResponse {
                status: Some(pb::Status {
                    code: pb::ErrorCode::ApprovalNotFound as i32,
                    message: "注册申请不存在".into(),
                    quota: None,
                    candidates: Vec::new(),
                }),
                request: None,
            })),
            Err(err) => Ok(Response::new(pb::GetRegistrationResponse {
                status: Some(status_err(&err)),
                request: None,
            })),
        }
    }

    async fn list_registrations(
        &self,
        request: Request<pb::ListRegistrationsRequest>,
    ) -> Result<Response<pb::ListRegistrationsResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let filter = parse_reg_status(req.filter);
        match self
            .api
            .store()
            .list_registrations(&session, filter, req.limit as usize)
        {
            Ok(list) => Ok(Response::new(pb::ListRegistrationsResponse {
                status: Some(status_ok()),
                requests: list.iter().map(registration_to_pb).collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListRegistrationsResponse {
                status: Some(status_err(&err)),
                requests: Vec::new(),
            })),
        }
    }

    async fn approve_registration(
        &self,
        request: Request<pb::ApproveRegistrationRequest>,
    ) -> Result<Response<pb::ApproveRegistrationResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let role = match parse_role(req.role) {
            Ok(role) => role,
            Err(status) => return Err(status),
        };
        match self
            .api
            .store()
            .approve_registration(&session, &req.request_id, role)
        {
            Ok(approved) => Ok(Response::new(pb::ApproveRegistrationResponse {
                status: Some(status_ok()),
                user_id: approved.user_id,
            })),
            Err(err) => Ok(Response::new(pb::ApproveRegistrationResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn reject_registration(
        &self,
        request: Request<pb::RejectRegistrationRequest>,
    ) -> Result<Response<pb::RejectRegistrationResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self
            .api
            .store()
            .reject_registration(&session, &req.request_id, &req.reason)
        {
            Ok(()) => Ok(Response::new(pb::RejectRegistrationResponse {
                status: Some(status_ok()),
            })),
            Err(err) => Ok(Response::new(pb::RejectRegistrationResponse {
                status: Some(status_err(&err)),
            })),
        }
    }

    async fn refresh_token(
        &self,
        _request: Request<pb::RefreshTokenRequest>,
    ) -> Result<Response<pb::RefreshTokenResponse>, Status> {
        unimplemented("RefreshToken（当前 JWT 12 小时有效，过期重新登录；轮换见 M3 剩余项）")
    }

    async fn logout(
        &self,
        _request: Request<pb::LogoutRequest>,
    ) -> Result<Response<pb::LogoutResponse>, Status> {
        unimplemented("Logout（JWT 无状态，客户端丢弃即可；服务端吊销走 token_version）")
    }

    async fn who_am_i(
        &self,
        request: Request<pb::WhoAmIRequest>,
    ) -> Result<Response<pb::WhoAmIResponse>, Status> {
        let session = self.api.session(&request)?;
        Ok(Response::new(pb::WhoAmIResponse {
            status: Some(status_ok()),
            user: Some(pb::UserInfo {
                user_id: session.user_id,
                username: session.username,
                display_name: String::new(),
                source: pb::AuthSource::Local as i32,
                status: pb::AccountStatus::Active as i32,
                role: role_to_pb(session.role),
                last_login_at: 0,
            }),
        }))
    }

    async fn create_pat(
        &self,
        request: Request<pb::CreatePatRequest>,
    ) -> Result<Response<pb::CreatePatResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let expires_at = if req.expires_at > 0 {
            Some(req.expires_at)
        } else {
            None
        };
        match self
            .api
            .store()
            .create_pat(&session, &req.name, &req.scopes, expires_at)
        {
            Ok((token, token_id)) => Ok(Response::new(pb::CreatePatResponse {
                status: Some(status_ok()),
                token,
                token_id,
            })),
            Err(err) => Ok(Response::new(pb::CreatePatResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn list_pats(
        &self,
        request: Request<pb::ListPatsRequest>,
    ) -> Result<Response<pb::ListPatsResponse>, Status> {
        let session = self.api.session(&request)?;
        match self.api.store().list_pats(&session) {
            Ok(tokens) => Ok(Response::new(pb::ListPatsResponse {
                status: Some(status_ok()),
                tokens: tokens
                    .iter()
                    .map(|t| pb::PatInfo {
                        token_id: t.token_id.clone(),
                        name: t.name.clone(),
                        scopes: t.scopes.clone(),
                        created_at: t.created_at,
                        expires_at: t.expires_at.unwrap_or(0),
                        revoked_at: t.revoked_at.unwrap_or(0),
                        last_used_at: t.last_used_at.unwrap_or(0),
                    })
                    .collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListPatsResponse {
                status: Some(status_err(&err)),
                tokens: Vec::new(),
            })),
        }
    }

    async fn revoke_pat(
        &self,
        request: Request<pb::RevokePatRequest>,
    ) -> Result<Response<pb::RevokePatResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().revoke_pat(&session, &req.token_id) {
            Ok(()) => Ok(Response::new(pb::RevokePatResponse {
                status: Some(status_ok()),
            })),
            Err(err) => Ok(Response::new(pb::RevokePatResponse {
                status: Some(status_err(&err)),
            })),
        }
    }

    async fn get_ldap_config(
        &self,
        request: Request<pb::GetLdapConfigRequest>,
    ) -> Result<Response<pb::GetLdapConfigResponse>, Status> {
        let session = self.api.session(&request)?;
        let result = self.api.store().ldap_config_public(Some(&session));
        match result {
            Ok(public) => {
                let (config, bind_password_set) = match public {
                    Some(p) => (ldap_settings_to_pb(&p.settings), p.bind_password_set),
                    None => (ldap_settings_to_pb(&LdapSettings::default()), false),
                };
                Ok(Response::new(pb::GetLdapConfigResponse {
                    status: Some(status_ok()),
                    config: Some(config),
                    bind_password_set,
                }))
            }
            Err(err) => Ok(Response::new(pb::GetLdapConfigResponse {
                status: Some(status_err(&err)),
                config: None,
                bind_password_set: false,
            })),
        }
    }

    async fn update_ldap_config(
        &self,
        request: Request<pb::UpdateLdapConfigRequest>,
    ) -> Result<Response<pb::UpdateLdapConfigResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let cfg = req
            .config
            .ok_or_else(|| Status::invalid_argument("缺少 LDAP 配置"))?;
        let password = {
            let store = self.api.store();
            match resolve_bind_password(&store, &req.bind_password) {
                Ok(p) => p,
                Err(err) => {
                    return Ok(Response::new(pb::UpdateLdapConfigResponse {
                        status: Some(status_err(&err)),
                    }))
                }
            }
        };
        let settings = match pb_to_ldap_settings(&cfg, password) {
            Ok(s) => s,
            Err(err) => {
                return Ok(Response::new(pb::UpdateLdapConfigResponse {
                    status: Some(status_err(&err)),
                }))
            }
        };
        match self.api.store().set_ldap_config(Some(&session), &settings) {
            Ok(()) => Ok(Response::new(pb::UpdateLdapConfigResponse {
                status: Some(status_ok()),
            })),
            Err(err) => Ok(Response::new(pb::UpdateLdapConfigResponse {
                status: Some(status_err(&err)),
            })),
        }
    }

    async fn test_ldap_connection(
        &self,
        request: Request<pb::TestLdapConnectionRequest>,
    ) -> Result<Response<pb::TestLdapConnectionResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let cfg = req
            .config
            .ok_or_else(|| Status::invalid_argument("缺少 LDAP 配置"))?;
        let (password, authz) = {
            let store = self.api.store();
            let authz = store.require_admin_action(&session, "测试 LDAP 连接");
            (resolve_bind_password(&store, &req.bind_password), authz)
        };
        if let Err(err) = authz {
            return Ok(Response::new(pb::TestLdapConnectionResponse {
                status: Some(status_err(&err)),
                bound_dn: String::new(),
                user_count_sample: 0,
            }));
        }
        let password = match password {
            Ok(p) => p,
            Err(err) => {
                return Ok(Response::new(pb::TestLdapConnectionResponse {
                    status: Some(status_err(&err)),
                    bound_dn: String::new(),
                    user_count_sample: 0,
                }))
            }
        };
        let settings = match pb_to_ldap_settings(&cfg, password) {
            Ok(s) => s,
            Err(err) => {
                return Ok(Response::new(pb::TestLdapConnectionResponse {
                    status: Some(status_err(&err)),
                    bound_dn: String::new(),
                    user_count_sample: 0,
                }))
            }
        };
        let outcome = tokio::task::spawn_blocking(move || test_ldap_connection(&settings)).await;
        match outcome {
            Ok(Ok(out)) => Ok(Response::new(pb::TestLdapConnectionResponse {
                status: Some(status_ok()),
                bound_dn: out.bound_dn,
                user_count_sample: out.user_count_sample,
            })),
            Ok(Err(err)) => Ok(Response::new(pb::TestLdapConnectionResponse {
                status: Some(status_err(&err)),
                bound_dn: String::new(),
                user_count_sample: 0,
            })),
            Err(join) => Ok(Response::new(pb::TestLdapConnectionResponse {
                status: Some(pb::Status {
                    code: pb::ErrorCode::LdapUnavailable as i32,
                    message: format!("目录测试任务异常：{join}"),
                    quota: None,
                    candidates: Vec::new(),
                }),
                bound_dn: String::new(),
                user_count_sample: 0,
            })),
        }
    }

    async fn set_user_status(
        &self,
        _request: Request<pb::SetUserStatusRequest>,
    ) -> Result<Response<pb::SetUserStatusResponse>, Status> {
        unimplemented("SetUserStatus（禁用 / 启用账号的 API 尚未落地）")
    }

    async fn set_user_role(
        &self,
        _request: Request<pb::SetUserRoleRequest>,
    ) -> Result<Response<pb::SetUserRoleResponse>, Status> {
        unimplemented("SetUserRole（改全局角色的 API 尚未落地）")
    }
}

// ---------------- RepoService ----------------

pub struct RepoApi {
    api: Api,
}

impl RepoApi {
    pub fn new(store: Store) -> Self {
        Self {
            api: Api::new(store),
        }
    }
}

#[tonic::async_trait]
impl RepoService for RepoApi {
    async fn get_head(
        &self,
        request: Request<pb::GetHeadRequest>,
    ) -> Result<Response<pb::GetHeadResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let result = {
            let store = self.api.store();
            store.head_info(&session, &req.repo_id)
        };
        match result {
            Ok((rev, root)) => Ok(Response::new(pb::GetHeadResponse {
                status: Some(status_ok()),
                rev,
                root_tree: vcs_core::to_hex(&root),
            })),
            Err(err) => Ok(Response::new(pb::GetHeadResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn get_revision(
        &self,
        request: Request<pb::GetRevisionRequest>,
    ) -> Result<Response<pb::GetRevisionResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self
            .api
            .store()
            .get_revision(&session, &req.repo_id, req.rev)
        {
            Ok(info) => Ok(Response::new(pb::GetRevisionResponse {
                status: Some(status_ok()),
                revision: Some(revision_to_pb(&info)),
            })),
            Err(err) => Ok(Response::new(pb::GetRevisionResponse {
                status: Some(status_err(&err)),
                revision: None,
            })),
        }
    }

    async fn get_tree(
        &self,
        request: Request<pb::GetTreeRequest>,
    ) -> Result<Response<pb::GetTreeResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let depth = pb::Depth::try_from(req.depth).unwrap_or(pb::Depth::Unspecified);
        let rev = if req.rev == 0 { None } else { Some(req.rev) };
        let store = self.api.store();
        let result = store.files_at(&session, &req.repo_id, rev);
        match result {
            Ok((repo, rev, files)) => {
                let root = match store.revision_root_tree(&repo.repo_id, rev) {
                    Ok(root) => root,
                    Err(err) => {
                        return Ok(Response::new(pb::GetTreeResponse {
                            status: Some(status_err(&err)),
                            ..Default::default()
                        }))
                    }
                };
                // M3 只实现 DEPTH_INFINITY（全量扁平）。稀疏检出/立即子项留给 M4 的检出向导。
                if !matches!(depth, pb::Depth::Unspecified | pb::Depth::Infinity) {
                    return Ok(Response::new(pb::GetTreeResponse {
                        status: Some(pb::Status {
                            code: pb::ErrorCode::InvalidArgument as i32,
                            message: "当前只支持 DEPTH_INFINITY；稀疏检出见 M4 检出向导".into(),
                            quota: None,
                            candidates: Vec::new(),
                        }),
                        root_tree: vcs_core::to_hex(&root),
                        entries: Vec::new(),
                    }));
                }
                let entries = files
                    .iter()
                    .filter(|e| req.paths.is_empty() || under_any(&e.path, &req.paths))
                    .map(|e| pb::TreeEntry {
                        // 深度语义未实现前，`name` 承载完整相对路径；这与 Checkout 的对象流配合即够用。
                        name: e.path.clone(),
                        kind: entry_kind_to_pb(e.kind),
                        hash: vcs_core::to_hex(&e.hash),
                        size: e.size,
                        props: Vec::new(),
                    })
                    .collect();
                Ok(Response::new(pb::GetTreeResponse {
                    status: Some(status_ok()),
                    root_tree: vcs_core::to_hex(&root),
                    entries,
                }))
            }
            Err(err) => Ok(Response::new(pb::GetTreeResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn get_log(
        &self,
        request: Request<pb::GetLogRequest>,
    ) -> Result<Response<pb::GetLogResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let limit = if req.limit == 0 {
            50
        } else {
            req.limit as usize
        };
        let store = self.api.store();
        match store.log(&session, &req.repo_id, limit) {
            Ok(mut revisions) => {
                let repo = match store.find_repo(&req.repo_id) {
                    Ok(repo) => repo,
                    Err(err) => {
                        return Ok(Response::new(pb::GetLogResponse {
                            status: Some(status_err(&err)),
                            revisions: Vec::new(),
                        }))
                    }
                };
                // 变更清单按需补齐：log 本身只查 revisions 表。
                for rev in revisions.iter_mut() {
                    rev.changes = store
                        .revision_changes(&repo.repo_id, rev.rev)
                        .unwrap_or_default();
                }
                Ok(Response::new(pb::GetLogResponse {
                    status: Some(status_ok()),
                    revisions: revisions.iter().map(revision_to_pb).collect(),
                }))
            }
            Err(err) => Ok(Response::new(pb::GetLogResponse {
                status: Some(status_err(&err)),
                revisions: Vec::new(),
            })),
        }
    }

    type CheckoutStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::ObjectBatch, Status>> + Send>>;

    async fn checkout(
        &self,
        request: Request<pb::CheckoutRequest>,
    ) -> Result<Response<Self::CheckoutStream>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let rev = if req.rev == 0 { None } else { Some(req.rev) };
        // 先在锁内把对象读进内存再返回流：MutexGuard 不能跨 await，也不能跨 yield。
        let objects = {
            let store = self.api.store();
            let (_, _, files) = store
                .files_at(&session, &req.repo_id, rev)
                .map_err(|e| Status::internal(format!("{:?}: {}", e.code, e.message)))?;
            let mut objects = Vec::new();
            for entry in files {
                if !req.paths.is_empty() && !under_any(&entry.path, &req.paths) {
                    continue;
                }
                let data = store
                    .objects()
                    .get(&entry.hash)
                    .map_err(|e| Status::internal(format!("{:?}: {}", e.code, e.message)))?;
                objects.push(pb::Object {
                    hash: vcs_core::to_hex(&entry.hash),
                    kind: pb::ObjectKind::Blob as i32,
                    data,
                    compressed: false,
                });
            }
            objects
        };
        let stream = tokio_stream::iter(
            batches(objects)
                .into_iter()
                .map(Ok)
                .collect::<Vec<Result<pb::ObjectBatch, Status>>>(),
        );
        Ok(Response::new(Box::pin(stream) as Self::CheckoutStream))
    }

    async fn update(
        &self,
        request: Request<pb::UpdateRequest>,
    ) -> Result<Response<pb::UpdateResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let store = self.api.store();
        let repo = match store.find_repo(&req.repo_id) {
            Ok(repo) => repo,
            Err(err) => {
                return Ok(Response::new(pb::UpdateResponse {
                    status: Some(status_err(&err)),
                    ..Default::default()
                }))
            }
        };
        let target = if req.target_rev == 0 {
            repo.head_rev
        } else {
            req.target_rev
        };
        // 客户端可能混合多个基线版本：从最旧的那个开始取差异，多算的条目在客户端是幂等的。
        let from = req
            .bases
            .iter()
            .map(|b| b.base_rev)
            .min()
            .unwrap_or(repo.head_rev)
            .min(target);
        match store.get_changes(&session, &repo.repo_id, from, target) {
            Ok(changes) => {
                let required: Vec<String> = changes
                    .iter()
                    .filter_map(|c| c.hash.map(|h| vcs_core::to_hex(&h)))
                    .collect();
                Ok(Response::new(pb::UpdateResponse {
                    status: Some(status_ok()),
                    rev: target,
                    changes: changes
                        .iter()
                        .map(|c| pb::Change {
                            kind: change_kind_to_pb(c.kind),
                            path: c.path.clone(),
                            old_hash: String::new(),
                            new_hash: c.hash.map(|h| vcs_core::to_hex(&h)).unwrap_or_default(),
                            from_path: String::new(),
                            props: entry_kind_props(c.entry_kind),
                        })
                        .collect(),
                    required_hashes: required,
                }))
            }
            Err(err) => Ok(Response::new(pb::UpdateResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    type FetchObjectsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::ObjectBatch, Status>> + Send>>;

    async fn fetch_objects(
        &self,
        request: Request<tonic::Streaming<pb::ObjectRequest>>,
    ) -> Result<Response<Self::FetchObjectsStream>, Status> {
        let _session = self.api.session(&request)?;
        let mut stream = request.into_inner();
        // 先把客户端要的哈希收齐：MutexGuard 不是 Send，不能跨 await 持有。
        let mut wanted: Vec<String> = Vec::new();
        while let Some(item) = stream.next().await {
            wanted.extend(item?.hashes);
        }
        let mut objects = Vec::new();
        {
            let store = self.api.store();
            for hex in wanted {
                let hash =
                    vcs_core::from_hex(&hex).map_err(|e| Status::invalid_argument(e.message))?;
                let data = store
                    .objects()
                    .get(&hash)
                    .map_err(|e| Status::internal(format!("{:?}: {}", e.code, e.message)))?;
                objects.push(pb::Object {
                    hash: hex,
                    kind: pb::ObjectKind::Blob as i32,
                    data,
                    compressed: false,
                });
            }
        }
        let out = tokio_stream::iter(
            batches(objects)
                .into_iter()
                .map(Ok)
                .collect::<Vec<Result<pb::ObjectBatch, Status>>>(),
        );
        Ok(Response::new(Box::pin(out) as Self::FetchObjectsStream))
    }

    async fn begin_commit(
        &self,
        request: Request<pb::BeginCommitRequest>,
    ) -> Result<Response<pb::BeginCommitResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let mut changes = Vec::with_capacity(req.changes.len());
        for change in req.changes {
            let kind = match pb::ChangeKind::try_from(change.kind)
                .unwrap_or(pb::ChangeKind::Unspecified)
            {
                pb::ChangeKind::Add => ChangeKind::Add,
                pb::ChangeKind::Modify => ChangeKind::Modify,
                pb::ChangeKind::Delete => ChangeKind::Delete,
                pb::ChangeKind::Rename => ChangeKind::Rename,
                pb::ChangeKind::Copy => ChangeKind::Copy,
                _ => return Err(Status::invalid_argument("未知的变更类型")),
            };
            let hash = if change.new_hash.is_empty() {
                None
            } else {
                Some(
                    vcs_core::from_hex(&change.new_hash)
                        .map_err(|e| Status::invalid_argument(e.message))?,
                )
            };
            changes.push(vcs_server::PendingChange {
                path: change.path,
                kind,
                entry_kind: change_entry_kind(&change.props),
                hash,
                size: 0,
            });
        }
        // 客户端声明的地址先记下来：提交会话建立后才能知道哪些对象服务端还没有。
        let declared: Vec<vcs_core::Hash> = changes.iter().filter_map(|c| c.hash).collect();
        let store = self.api.store();
        match store.begin_commit(
            &session,
            &req.repo_id,
            req.base_rev,
            &changes,
            &req.message,
            req.estimated_new_bytes,
        ) {
            Ok(outcome) => {
                let head = store
                    .find_repo(&req.repo_id)
                    .map(|r| r.head_rev)
                    .unwrap_or(0);
                // 服务端已有的对象不必重传：客户端据此只推缺的那一部分。
                let missing_hashes: Vec<String> = declared
                    .iter()
                    .filter(|hash| !store.objects().exists(hash))
                    .map(vcs_core::to_hex)
                    .collect();
                Ok(Response::new(pb::BeginCommitResponse {
                    status: Some(status_ok()),
                    session_id: outcome.session_id,
                    head_rev: head,
                    missing_hashes,
                }))
            }
            Err(err) => Ok(Response::new(pb::BeginCommitResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn push_objects(
        &self,
        request: Request<tonic::Streaming<pb::ObjectBatch>>,
    ) -> Result<Response<pb::PushAck>, Status> {
        let session = self.api.session(&request)?;
        let mut stream = request.into_inner();
        let mut session_id = String::new();
        let mut objects = Vec::new();
        while let Some(item) = stream.next().await {
            let batch = item?;
            if session_id.is_empty() {
                session_id = batch.session_id.clone();
            }
            for object in batch.objects {
                let hash = vcs_core::from_hex(&object.hash)
                    .map_err(|e| Status::invalid_argument(e.message))?;
                objects.push((hash, object.data));
            }
        }
        if session_id.is_empty() {
            return Err(Status::invalid_argument("缺少 session_id"));
        }
        let received = objects.len() as u64;
        let bytes = objects.iter().map(|(_, data)| data.len() as u64).sum();
        match self
            .api
            .store()
            .push_objects(&session, &session_id, &objects)
        {
            Ok(_) => Ok(Response::new(pb::PushAck {
                status: Some(status_ok()),
                received_objects: received,
                received_bytes: bytes,
            })),
            Err(err) => Ok(Response::new(pb::PushAck {
                status: Some(status_err(&err)),
                received_objects: 0,
                received_bytes: 0,
            })),
        }
    }

    async fn finalize_commit(
        &self,
        request: Request<pb::FinalizeCommitRequest>,
    ) -> Result<Response<pb::FinalizeCommitResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let store = self.api.store();
        match store.finalize_commit(&session, &req.session_id) {
            Ok(outcome) => {
                // 响应里的 root_tree 供客户端校验本地树：拿会话所属仓库 + 新版本号取一次。
                let root = store
                    .session_repo_id(&session, &req.session_id)
                    .and_then(|repo_id| store.revision_root_tree(&repo_id, outcome.rev))
                    .map(|h| vcs_core::to_hex(&h))
                    .unwrap_or_default();
                Ok(Response::new(pb::FinalizeCommitResponse {
                    status: Some(status_ok()),
                    rev: outcome.rev,
                    root_tree: root,
                    updated_paths: Vec::new(),
                }))
            }
            Err(err) => Ok(Response::new(pb::FinalizeCommitResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn abort_commit(
        &self,
        request: Request<pb::AbortCommitRequest>,
    ) -> Result<Response<pb::AbortCommitResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().abort_commit(&session, &req.session_id) {
            Ok(()) => Ok(Response::new(pb::AbortCommitResponse {
                status: Some(status_ok()),
            })),
            Err(err) => Ok(Response::new(pb::AbortCommitResponse {
                status: Some(status_err(&err)),
            })),
        }
    }

    async fn lock(
        &self,
        _request: Request<pb::LockRequest>,
    ) -> Result<Response<pb::LockResponse>, Status> {
        unimplemented("Lock（锁服务属于 M3 剩余项）")
    }

    async fn unlock(
        &self,
        _request: Request<pb::UnlockRequest>,
    ) -> Result<Response<pb::UnlockResponse>, Status> {
        unimplemented("Unlock（锁服务属于 M3 剩余项）")
    }

    async fn get_quota(
        &self,
        request: Request<pb::GetQuotaRequest>,
    ) -> Result<Response<pb::GetQuotaResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let store = self.api.store();
        let repo = match store.find_repo(&req.repo_id) {
            Ok(repo) => repo,
            Err(err) => {
                return Ok(Response::new(pb::GetQuotaResponse {
                    status: Some(status_err(&err)),
                    ..Default::default()
                }))
            }
        };
        if let Err(err) = store.member_role(&repo.repo_id, &session.user_id) {
            return Ok(Response::new(pb::GetQuotaResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            }));
        }
        match store.usage(&repo.repo_id) {
            Ok(usage) => Ok(Response::new(pb::GetQuotaResponse {
                status: Some(status_ok()),
                used_bytes: usage.used_bytes,
                limit_bytes: usage.quota.bytes,
                // 精确的可回收量要走 EstimatePurge（要遍历全库引用），这里不假装知道。
                reclaimable_bytes: 0,
                by_path: Vec::new(),
                effective: Some(pb::QuotaEffective {
                    bytes: usage.quota.bytes,
                    source: quota_source_to_pb(usage.quota.source),
                }),
            })),
            Err(err) => Ok(Response::new(pb::GetQuotaResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn list_purge_candidates(
        &self,
        request: Request<pb::ListPurgeCandidatesRequest>,
    ) -> Result<Response<pb::ListPurgeCandidatesResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let limit = if req.limit == 0 {
            10
        } else {
            req.limit as usize
        };
        match self
            .api
            .store()
            .list_purge_candidates(&session, &req.repo_id, limit)
        {
            Ok(list) => Ok(Response::new(pb::ListPurgeCandidatesResponse {
                status: Some(status_ok()),
                candidates: list
                    .iter()
                    .map(|c| pb::PurgeCandidate {
                        path: c.path.clone(),
                        logical_bytes: c.logical_bytes,
                        unique_bytes: c.unique_bytes,
                        first_seen_rev: c.first_seen_rev,
                        last_modified_rev: c.last_modified_rev,
                    })
                    .collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListPurgeCandidatesResponse {
                status: Some(status_err(&err)),
                candidates: Vec::new(),
            })),
        }
    }

    async fn estimate_purge(
        &self,
        request: Request<pb::EstimatePurgeRequest>,
    ) -> Result<Response<pb::EstimatePurgeResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self
            .api
            .store()
            .estimate_purge(&session, &req.repo_id, &req.paths)
        {
            Ok(estimate) => Ok(Response::new(pb::EstimatePurgeResponse {
                status: Some(status_ok()),
                reclaimable_bytes: estimate.reclaimable_bytes,
                affected_revisions: estimate.affected_revisions,
                shared_paths: estimate.shared_paths,
            })),
            Err(err) => Ok(Response::new(pb::EstimatePurgeResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn purge(
        &self,
        request: Request<pb::PurgeRequest>,
    ) -> Result<Response<pb::PurgeResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().purge(
            &session,
            &req.repo_id,
            &req.paths,
            req.immediate,
            &req.reason,
        ) {
            Ok(outcome) => Ok(Response::new(pb::PurgeResponse {
                status: Some(status_ok()),
                purge_id: 0,
                new_rev: outcome.rev,
                gc_job_id: outcome.gc_job_id,
            })),
            Err(err) => Ok(Response::new(pb::PurgeResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn list_purged_paths(
        &self,
        request: Request<pb::ListPurgedPathsRequest>,
    ) -> Result<Response<pb::ListPurgedPathsResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let result = {
            let store = self.api.store();
            store.list_purged_paths(&session, &req.repo_id)
        };
        match result {
            Ok(list) => Ok(Response::new(pb::ListPurgedPathsResponse {
                status: Some(status_ok()),
                entries: list
                    .iter()
                    .map(|item| pb::PurgedPathInfo {
                        purge_id: item.purge_id.clone(),
                        path: item.path.clone(),
                        state: item.state.clone(),
                        reason: item.reason.clone().unwrap_or_default(),
                        purged_by: item.purged_by.clone(),
                        created_at: item.created_at,
                        grace_until: item.grace_until.unwrap_or(0),
                        activated_at: item.activated_at.unwrap_or(0),
                    })
                    .collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListPurgedPathsResponse {
                status: Some(status_err(&err)),
                entries: Vec::new(),
            })),
        }
    }

    async fn revert_purge(
        &self,
        request: Request<pb::RevertPurgeRequest>,
    ) -> Result<Response<pb::RevertPurgeResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let result = {
            let store = self.api.store();
            store.revert_purge(&session, &req.repo_id, &req.purge_id)
        };
        match result {
            Ok(rev) => Ok(Response::new(pb::RevertPurgeResponse {
                status: Some(status_ok()),
                new_rev: rev,
            })),
            Err(err) => Ok(Response::new(pb::RevertPurgeResponse {
                status: Some(status_err(&err)),
                new_rev: 0,
            })),
        }
    }

    async fn list_retention_policies(
        &self,
        request: Request<pb::ListRetentionPoliciesRequest>,
    ) -> Result<Response<pb::ListRetentionPoliciesResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let result = {
            let store = self.api.store();
            store.list_retention_policies(&session, &req.repo_id)
        };
        match result {
            Ok(list) => Ok(Response::new(pb::ListRetentionPoliciesResponse {
                status: Some(status_ok()),
                policies: list.iter().map(policy_to_pb).collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListRetentionPoliciesResponse {
                status: Some(status_err(&err)),
                policies: Vec::new(),
            })),
        }
    }

    async fn upsert_retention_policy(
        &self,
        request: Request<pb::UpsertRetentionPolicyRequest>,
    ) -> Result<Response<pb::UpsertRetentionPolicyResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let input = vcs_server::RetentionPolicyInput {
            policy_id: if req.policy_id.is_empty() {
                None
            } else {
                Some(req.policy_id)
            },
            path_pattern: req.path_pattern,
            keep_months: req.keep_months,
            max_bytes: req.max_bytes,
            max_age_days: req.max_age_days,
            enabled: req.enabled,
            dry_run: req.dry_run,
        };
        let result = {
            let store = self.api.store();
            store.upsert_retention_policy(&session, &req.repo_id, input)
        };
        match result {
            Ok(policy) => Ok(Response::new(pb::UpsertRetentionPolicyResponse {
                status: Some(status_ok()),
                policy: Some(policy_to_pb(&policy)),
            })),
            Err(err) => Ok(Response::new(pb::UpsertRetentionPolicyResponse {
                status: Some(status_err(&err)),
                policy: None,
            })),
        }
    }

    async fn delete_retention_policy(
        &self,
        request: Request<pb::DeleteRetentionPolicyRequest>,
    ) -> Result<Response<pb::DeleteRetentionPolicyResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let result = {
            let store = self.api.store();
            store.delete_retention_policy(&session, &req.repo_id, &req.policy_id)
        };
        Ok(Response::new(pb::DeleteRetentionPolicyResponse {
            status: Some(match result {
                Ok(()) => status_ok(),
                Err(err) => status_err(&err),
            }),
        }))
    }

    async fn apply_retention(
        &self,
        request: Request<pb::ApplyRetentionRequest>,
    ) -> Result<Response<pb::ApplyRetentionResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let result = {
            let store = self.api.store();
            store.apply_retention(&session, &req.repo_id, req.dry_run, req.immediate)
        };
        match result {
            Ok(outcome) => Ok(Response::new(pb::ApplyRetentionResponse {
                status: Some(status_ok()),
                candidates: outcome.candidates,
                reclaimable_bytes: outcome.reclaimable_bytes,
                new_rev: outcome.new_rev,
            })),
            Err(err) => Ok(Response::new(pb::ApplyRetentionResponse {
                status: Some(status_err(&err)),
                candidates: Vec::new(),
                reclaimable_bytes: 0,
                new_rev: 0,
            })),
        }
    }

    async fn get_gc_status(
        &self,
        request: Request<pb::GetGcStatusRequest>,
    ) -> Result<Response<pb::GetGcStatusResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self
            .api
            .store()
            .gc_status(&session, &req.repo_id, &req.job_id)
        {
            Ok(status) => Ok(Response::new(pb::GetGcStatusResponse {
                status: Some(status_ok()),
                state: gc_state_to_pb(&status.state),
                progress_percent: status.progress_percent,
                marked_objects: status.marked_objects,
                swept_objects: status.swept_objects,
                reclaimed_bytes: status.reclaimed_bytes,
                error: status.error.unwrap_or_default(),
            })),
            Err(err) => Ok(Response::new(pb::GetGcStatusResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }
}

fn policy_to_pb(policy: &vcs_server::RetentionPolicyInfo) -> pb::RetentionPolicyInfo {
    pb::RetentionPolicyInfo {
        policy_id: policy.policy_id.clone(),
        path_pattern: policy.path_pattern.clone(),
        keep_months: policy.keep_months,
        max_bytes: policy.max_bytes,
        max_age_days: policy.max_age_days,
        enabled: policy.enabled,
        dry_run: policy.dry_run,
        created_at: policy.created_at,
    }
}

fn gc_state_to_pb(state: &str) -> i32 {
    match state {
        "queued" => pb::GcState::Queued as i32,
        "marking" => pb::GcState::Marking as i32,
        "sweeping" => pb::GcState::Sweeping as i32,
        "done" => pb::GcState::Done as i32,
        "failed" => pb::GcState::Failed as i32,
        _ => pb::GcState::Unspecified as i32,
    }
}

/// 路径是否落在任一前缀之下。与 `vcs-server` 的清理前缀判定同一语义。
fn under_any(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        path == p.as_str()
            || (path.len() > p.len()
                && path.starts_with(p.as_str())
                && path.as_bytes()[p.len()] == b'/')
    })
}

// ---------------- RepoAdminService ----------------

pub struct AdminApi {
    api: Api,
}

impl AdminApi {
    pub fn new(store: Store) -> Self {
        Self {
            api: Api::new(store),
        }
    }
}

#[tonic::async_trait]
impl RepoAdminService for AdminApi {
    async fn request_repo(
        &self,
        request: Request<pb::RequestRepoRequest>,
    ) -> Result<Response<pb::RequestRepoResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().request_repo(
            &session,
            &req.repo_name,
            &req.reason,
            req.requested_quota_bytes,
        ) {
            Ok(info) => Ok(Response::new(pb::RequestRepoResponse {
                status: Some(status_ok()),
                request_id: info.request_id,
                state: creation_status_to_pb(info.state),
            })),
            Err(err) => Ok(Response::new(pb::RequestRepoResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn get_repo_creation(
        &self,
        request: Request<pb::GetRepoCreationRequest>,
    ) -> Result<Response<pb::GetRepoCreationResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().get_repo_request(&session, &req.request_id) {
            Ok(info) => Ok(Response::new(pb::GetRepoCreationResponse {
                status: Some(status_ok()),
                request: Some(request_to_pb(&info)),
            })),
            Err(err) => Ok(Response::new(pb::GetRepoCreationResponse {
                status: Some(status_err(&err)),
                request: None,
            })),
        }
    }

    async fn list_repo_creations(
        &self,
        request: Request<pb::ListRepoCreationsRequest>,
    ) -> Result<Response<pb::ListRepoCreationsResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let filter = parse_creation_status(req.filter);
        match self
            .api
            .store()
            .list_repo_requests(&session, filter, req.limit as usize)
        {
            Ok(list) => Ok(Response::new(pb::ListRepoCreationsResponse {
                status: Some(status_ok()),
                requests: list.iter().map(request_to_pb).collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListRepoCreationsResponse {
                status: Some(status_err(&err)),
                requests: Vec::new(),
            })),
        }
    }

    async fn approve_repo_creation(
        &self,
        request: Request<pb::ApproveRepoCreationRequest>,
    ) -> Result<Response<pb::ApproveRepoCreationResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().approve_repo_request(
            &session,
            &req.request_id,
            req.quota_bytes,
            &req.decision_note,
        ) {
            Ok(repo) => Ok(Response::new(pb::ApproveRepoCreationResponse {
                status: Some(status_ok()),
                repo: Some(repo_to_pb(&repo)),
            })),
            Err(err) => Ok(Response::new(pb::ApproveRepoCreationResponse {
                status: Some(status_err(&err)),
                repo: None,
            })),
        }
    }

    async fn reject_repo_creation(
        &self,
        request: Request<pb::RejectRepoCreationRequest>,
    ) -> Result<Response<pb::RejectRepoCreationResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self
            .api
            .store()
            .reject_repo_request(&session, &req.request_id, &req.reason)
        {
            Ok(()) => Ok(Response::new(pb::RejectRepoCreationResponse {
                status: Some(status_ok()),
            })),
            Err(err) => Ok(Response::new(pb::RejectRepoCreationResponse {
                status: Some(status_err(&err)),
            })),
        }
    }

    async fn create_repo_direct(
        &self,
        request: Request<pb::CreateRepoDirectRequest>,
    ) -> Result<Response<pb::CreateRepoDirectResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self
            .api
            .store()
            .create_repo_direct(&session, &req.repo_name, req.quota_bytes)
        {
            Ok(repo) => Ok(Response::new(pb::CreateRepoDirectResponse {
                status: Some(status_ok()),
                repo: Some(repo_to_pb(&repo)),
            })),
            Err(err) => Ok(Response::new(pb::CreateRepoDirectResponse {
                status: Some(status_err(&err)),
                repo: None,
            })),
        }
    }

    async fn list_repos(
        &self,
        request: Request<pb::ListReposRequest>,
    ) -> Result<Response<pb::ListReposResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let filter = parse_repo_status(req.filter);
        match self.api.store().list_repos(&session, filter) {
            Ok(repos) => Ok(Response::new(pb::ListReposResponse {
                status: Some(status_ok()),
                repos: repos.iter().map(repo_to_pb).collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListReposResponse {
                status: Some(status_err(&err)),
                repos: Vec::new(),
            })),
        }
    }

    async fn get_repo(
        &self,
        request: Request<pb::GetRepoQuery>,
    ) -> Result<Response<pb::GetRepoResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let store = self.api.store();
        match store.find_repo(&req.repo_id) {
            Ok(repo) => {
                // 非成员不返回仓库信息，与 ListRepos 的口径一致。
                if let Err(err) = store.member_role(&repo.repo_id, &session.user_id) {
                    return Ok(Response::new(pb::GetRepoResponse {
                        status: Some(status_err(&err)),
                        repo: None,
                    }));
                }
                Ok(Response::new(pb::GetRepoResponse {
                    status: Some(status_ok()),
                    repo: Some(repo_to_pb(&repo)),
                }))
            }
            Err(err) => Ok(Response::new(pb::GetRepoResponse {
                status: Some(status_err(&err)),
                repo: None,
            })),
        }
    }

    async fn set_repo_status(
        &self,
        request: Request<pb::SetRepoStatusRequest>,
    ) -> Result<Response<pb::SetRepoStatusResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let status = match parse_repo_status(req.status) {
            Some(status) => status,
            None => return Err(Status::invalid_argument("未知的仓库状态")),
        };
        let result = {
            let store = self.api.store();
            store.set_repo_status(&session, &req.repo_id, status)
        };
        Ok(Response::new(pb::SetRepoStatusResponse {
            status: Some(match result {
                Ok(_) => status_ok(),
                Err(err) => status_err(&err),
            }),
        }))
    }

    async fn list_members(
        &self,
        request: Request<pb::ListMembersRequest>,
    ) -> Result<Response<pb::ListMembersResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().list_members(&session, &req.repo_id) {
            Ok(members) => Ok(Response::new(pb::ListMembersResponse {
                status: Some(status_ok()),
                members: members.iter().map(member_to_pb).collect(),
            })),
            Err(err) => Ok(Response::new(pb::ListMembersResponse {
                status: Some(status_err(&err)),
                members: Vec::new(),
            })),
        }
    }

    async fn add_member(
        &self,
        request: Request<pb::AddMemberRequest>,
    ) -> Result<Response<pb::AddMemberResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let role = match parse_member_role(req.role) {
            Ok(role) => role,
            Err(status) => return Err(status),
        };
        // proto 用 user_id 定位；服务端 API 按用户名，这里先翻译一次。
        let username = {
            let store = self.api.store();
            match store.username_by_id(&req.user_id) {
                Ok(name) => name,
                Err(err) => return Err(Status::not_found(err.message)),
            }
        };
        // 先把返回值收进变量再 match：`match self.api.store().xxx()` 的临时 guard 会活到
        // 整个 match 结束，在 arm 里再锁一次就是自己把自己锁死（std::sync::Mutex 不可重入）。
        let result = {
            let store = self.api.store();
            store.add_member(&session, &req.repo_id, &username, role)
        };
        match result {
            Ok(()) => {
                // add_member 只返回 ()，这里把刚写入的成员行读回来，客户端才好直接刷新列表。
                let member = {
                    let store = self.api.store();
                    store
                        .list_members(&session, &req.repo_id)
                        .unwrap_or_default()
                        .into_iter()
                        .find(|m| m.user_id == req.user_id)
                        .map(|m| member_to_pb(&m))
                };
                Ok(Response::new(pb::AddMemberResponse {
                    status: Some(status_ok()),
                    member,
                }))
            }
            Err(err) => Ok(Response::new(pb::AddMemberResponse {
                status: Some(status_err(&err)),
                member: None,
            })),
        }
    }

    async fn remove_member(
        &self,
        request: Request<pb::RemoveMemberRequest>,
    ) -> Result<Response<pb::RemoveMemberResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let username = {
            let store = self.api.store();
            match store.username_by_id(&req.user_id) {
                Ok(name) => name,
                Err(err) => {
                    return Ok(Response::new(pb::RemoveMemberResponse {
                        status: Some(status_err(&err)),
                    }))
                }
            }
        };
        let result = {
            let store = self.api.store();
            store.remove_member(&session, &req.repo_id, &username)
        };
        Ok(Response::new(pb::RemoveMemberResponse {
            status: Some(match result {
                Ok(()) => status_ok(),
                Err(err) => status_err(&err),
            }),
        }))
    }

    async fn set_member_role(
        &self,
        request: Request<pb::SetMemberRoleRequest>,
    ) -> Result<Response<pb::SetMemberRoleResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let role = match parse_member_role(req.role) {
            Ok(role) => role,
            Err(status) => return Err(status),
        };
        let username = {
            let store = self.api.store();
            match store.username_by_id(&req.user_id) {
                Ok(name) => name,
                Err(err) => {
                    return Ok(Response::new(pb::SetMemberRoleResponse {
                        status: Some(status_err(&err)),
                        member: None,
                    }))
                }
            }
        };
        let result = {
            let store = self.api.store();
            store.set_member_role(&session, &req.repo_id, &username, role)
        };
        match result {
            Ok(member) => Ok(Response::new(pb::SetMemberRoleResponse {
                status: Some(status_ok()),
                member: Some(member_to_pb(&member)),
            })),
            Err(err) => Ok(Response::new(pb::SetMemberRoleResponse {
                status: Some(status_err(&err)),
                member: None,
            })),
        }
    }

    async fn search_users(
        &self,
        request: Request<pb::SearchUsersRequest>,
    ) -> Result<Response<pb::SearchUsersResponse>, Status> {
        let _session = self.api.session(&request)?;
        let req = request.into_inner();
        let limit = if req.limit == 0 {
            20
        } else {
            req.limit as usize
        };
        match self.api.store().search_users(&req.query, limit) {
            Ok(users) => Ok(Response::new(pb::SearchUsersResponse {
                status: Some(status_ok()),
                users: users
                    .iter()
                    .map(|u| pb::UserSummary {
                        user_id: u.user_id.clone(),
                        username: u.username.clone(),
                        display_name: u.display_name.clone(),
                    })
                    .collect(),
            })),
            Err(err) => Ok(Response::new(pb::SearchUsersResponse {
                status: Some(status_err(&err)),
                users: Vec::new(),
            })),
        }
    }

    async fn get_quota_defaults(
        &self,
        request: Request<pb::GetQuotaDefaultsRequest>,
    ) -> Result<Response<pb::GetQuotaDefaultsResponse>, Status> {
        let session = self.api.session(&request)?;
        let store = self.api.store();
        // 全局默认对所有已登录用户可见（仓库设置页要展示「继承自全局」），只有管理员能改。
        let _ = session;
        let fallback = vcs_core::resolve_effective_quota(0, 0);
        match store.quota_defaults() {
            Ok(bytes) => Ok(Response::new(pb::GetQuotaDefaultsResponse {
                status: Some(status_ok()),
                defaults: Some(pb::QuotaDefaults {
                    bytes,
                    updated_at: 0,
                    updated_by: String::new(),
                }),
                fallback: Some(pb::QuotaEffective {
                    bytes: fallback.bytes,
                    source: quota_source_to_pb(fallback.source),
                }),
            })),
            Err(err) => Ok(Response::new(pb::GetQuotaDefaultsResponse {
                status: Some(status_err(&err)),
                defaults: None,
                fallback: None,
            })),
        }
    }

    async fn update_quota_defaults(
        &self,
        request: Request<pb::UpdateQuotaDefaultsRequest>,
    ) -> Result<Response<pb::UpdateQuotaDefaultsResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        match self.api.store().set_default_quota(&session, req.bytes) {
            Ok(effective) => Ok(Response::new(pb::UpdateQuotaDefaultsResponse {
                status: Some(status_ok()),
                defaults: Some(pb::QuotaDefaults {
                    bytes: effective.bytes,
                    updated_at: now_secs(),
                    updated_by: session.username.clone(),
                }),
            })),
            Err(err) => Ok(Response::new(pb::UpdateQuotaDefaultsResponse {
                status: Some(status_err(&err)),
                defaults: None,
            })),
        }
    }

    async fn get_repo_quota(
        &self,
        request: Request<pb::GetRepoQuotaRequest>,
    ) -> Result<Response<pb::GetRepoQuotaResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let store = self.api.store();
        let repo = match store.find_repo(&req.repo_id) {
            Ok(repo) => repo,
            Err(err) => {
                return Ok(Response::new(pb::GetRepoQuotaResponse {
                    status: Some(status_err(&err)),
                    ..Default::default()
                }))
            }
        };
        if let Err(err) = store.member_role(&repo.repo_id, &session.user_id) {
            return Ok(Response::new(pb::GetRepoQuotaResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            }));
        }
        let repo_bytes = match store.repo_quota_override(&repo.repo_id) {
            Ok(quota) => quota,
            Err(err) => {
                return Ok(Response::new(pb::GetRepoQuotaResponse {
                    status: Some(status_err(&err)),
                    ..Default::default()
                }))
            }
        };
        let default_bytes = match store.quota_defaults() {
            Ok(bytes) => bytes,
            Err(err) => {
                return Ok(Response::new(pb::GetRepoQuotaResponse {
                    status: Some(status_err(&err)),
                    ..Default::default()
                }))
            }
        };
        match store.effective_quota(&repo.repo_id) {
            Ok(effective) => Ok(Response::new(pb::GetRepoQuotaResponse {
                status: Some(status_ok()),
                repo_bytes,
                defaults: Some(pb::QuotaDefaults {
                    bytes: default_bytes,
                    updated_at: 0,
                    updated_by: String::new(),
                }),
                effective: Some(pb::QuotaEffective {
                    bytes: effective.bytes,
                    source: quota_source_to_pb(effective.source),
                }),
            })),
            Err(err) => Ok(Response::new(pb::GetRepoQuotaResponse {
                status: Some(status_err(&err)),
                ..Default::default()
            })),
        }
    }

    async fn set_repo_quota(
        &self,
        request: Request<pb::SetRepoQuotaRequest>,
    ) -> Result<Response<pb::SetRepoQuotaResponse>, Status> {
        let session = self.api.session(&request)?;
        let req = request.into_inner();
        let result = {
            let store = self.api.store();
            store.set_repo_quota(&session, &req.repo_id, req.bytes, req.inherit)
        };
        match result {
            Ok(effective) => Ok(Response::new(pb::SetRepoQuotaResponse {
                status: Some(status_ok()),
                effective: Some(pb::QuotaEffective {
                    bytes: effective.bytes,
                    source: quota_source_to_pb(effective.source),
                }),
            })),
            Err(err) => Ok(Response::new(pb::SetRepoQuotaResponse {
                status: Some(status_err(&err)),
                effective: None,
            })),
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------- 装配与启动 ----------------

/// 把三个 service 装进一个 tonic router。
///
/// 单独暴露 `router` 是为了测试能在进程内起服务：`serve_with_incoming` 可以用
/// 内核分配的端口（`:0`），不必在 CI 上抢固定端口。
pub fn router(store: Store) -> tonic::transport::server::Router {
    tonic::transport::Server::builder()
        .add_service(AuthServiceServer::new(AuthApi::new(store.clone())))
        .add_service(RepoServiceServer::new(RepoApi::new(store.clone())))
        .add_service(RepoAdminServiceServer::new(RepoAdminApi::new(store)))
}

type RepoAdminApi = AdminApi;

/// 服务端启动参数。
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub root: PathBuf,
    pub listen: SocketAddr,
    /// 内置 Web 管理端监听地址；`None` 表示不启动（`--no-admin`）。
    pub admin_listen: Option<SocketAddr>,
    /// 后台维护间隔（秒）。0 表示不启动定时任务，需要手工 `vcs gc`。
    pub maintenance_interval_secs: u64,
}

/// 启动 gRPC 服务与内置 Web 管理端，直到任一服务退出。
pub async fn serve(config: ServeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let store = ServerStore::open(&config.root)?;
    let store: Store = Arc::new(Mutex::new(store));
    // 先绑定管理端：端口占用要立刻失败，不能留下「gRPC 起来了但管理页面没起来」的半残状态。
    let admin_listener = match config.admin_listen {
        Some(addr) => Some(
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| format!("Web 管理端无法监听 {addr}：{e}"))?,
        ),
        None => None,
    };
    println!(
        "vcs serve 已启动：{}（存储根目录 {}）",
        config.listen,
        config.root.display()
    );
    match config.admin_listen {
        Some(addr) => println!("Web 管理端已启动：http://{addr}"),
        None => println!("Web 管理端：已关闭（--no-admin）"),
    }
    if config.maintenance_interval_secs > 0 {
        println!(
            "后台维护：每 {} 秒一次（转正到期清理 + 全库 GC）",
            config.maintenance_interval_secs
        );
        spawn_maintenance(store.clone(), config.maintenance_interval_secs);
    } else {
        println!("后台维护：已关闭（需要手工执行 vcs gc）");
    }
    let grpc = router(store.clone()).serve(config.listen);
    match admin_listener {
        Some(listener) => {
            let http = admin::serve_with_listener(store.clone(), listener);
            let mut grpc = std::pin::pin!(grpc);
            let mut http = std::pin::pin!(http);
            tokio::select! {
                result = &mut grpc => result?,
                result = &mut http => result.map_err(|e| e.to_string())?,
            }
        }
        None => grpc.await?,
    }
    Ok(())
}

/// 后台维护循环。
///
/// 两件事：把宽限期到点的清理记录转正，然后跑一次全库 GC。
/// **不做保留策略的自动执行**——那需要以「系统」身份提交版本，得先把提交链路的
/// 权限模型打通（见设计 §30.4），在那之前宁可少做一步，也不要绕过权限去删数据。
fn spawn_maintenance(store: Store, interval_secs: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        // 上一轮跑超时了不要补跑，直接顺延——维护是幂等的，堆积反而伤磁盘。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let store = store.clone();
            // ServerStore 全是阻塞的 SQLite / 文件操作，必须挪到 blocking 线程池，
            // 否则一次大 GC 会把整个 gRPC 运行时卡住。
            let outcome = tokio::task::spawn_blocking(move || {
                let guard = store.lock().unwrap_or_else(|err| err.into_inner());
                guard.run_maintenance()
            })
            .await;
            match outcome {
                Ok(Ok(report)) => {
                    let touched = report.promoted_purges > 0
                        || report.swept_objects > 0
                        || report.retention_paths > 0;
                    if touched {
                        println!(
                            "后台维护完成：保留策略清理 {} 个目录，转正 {} 条清理记录，\
                             清扫 {} 个对象，释放 {} 字节",
                            report.retention_paths,
                            report.promoted_purges,
                            report.swept_objects,
                            report.reclaimed_bytes
                        );
                    }
                }
                Ok(Err(err)) => eprintln!("后台维护失败：{:?}: {}", err.code, err.message),
                Err(join) => eprintln!("后台维护任务异常：{join}"),
            }
        }
    })
}

/// 供测试使用：在一个随机端口上起服务，返回监听地址与关闭句柄。
pub async fn serve_in_background(
    store: Store,
) -> Result<
    (
        SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ),
    Box<dyn std::error::Error>,
> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let handle = tokio::spawn(async move {
        let _ = router(store)
            .serve_with_incoming_shutdown(incoming, async {
                let _ = rx.await;
            })
            .await;
    });
    Ok((addr, tx, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_numbers_match_the_contract() {
        // 客户端按数值分支，proto 与 core 的编号必须逐一对齐。
        // 特别注意 OK=1 而不是 0：core 里 Rust 枚举没有显式判别值，直接 `as i32` 会错一位。
        let cases = [
            (ErrorCode::Ok, 1),
            (ErrorCode::OutOfDate, 2),
            (ErrorCode::PathConflict, 3),
            (ErrorCode::TreeConflict, 4),
            (ErrorCode::PermissionDenied, 5),
            (ErrorCode::QuotaExceeded, 6),
            (ErrorCode::ContentPurged, 7),
            (ErrorCode::Locked, 8),
            (ErrorCode::ObjectMissing, 9),
            (ErrorCode::InvalidSession, 10),
            (ErrorCode::Unauthenticated, 11),
            (ErrorCode::InvalidCredentials, 12),
            (ErrorCode::AccountPending, 13),
            (ErrorCode::AccountDisabled, 14),
            (ErrorCode::RegistrationExists, 15),
            (ErrorCode::ApprovalNotFound, 16),
            (ErrorCode::SetupTokenInvalid, 17),
            (ErrorCode::LdapUnavailable, 18),
            (ErrorCode::TokenExpired, 19),
            (ErrorCode::PatRevoked, 20),
            (ErrorCode::RepoNotFound, 21),
            (ErrorCode::RepoDisabled, 22),
            (ErrorCode::RepoArchived, 23),
            (ErrorCode::RepoNameInvalid, 24),
            (ErrorCode::RepoCreationExists, 25),
            (ErrorCode::RepoApprovalNotFound, 26),
            (ErrorCode::PasswordPolicyViolation, 27),
            (ErrorCode::RepoNotMember, 28),
            (ErrorCode::RepoMemberExists, 29),
            (ErrorCode::RepoMemberNotFound, 30),
            (ErrorCode::RepoOwnerProtected, 31),
            (ErrorCode::RepoMemberForbidden, 32),
            (ErrorCode::UserNotFound, 33),
            (ErrorCode::QuotaConfigInvalid, 34),
            (ErrorCode::IoError, 35),
            (ErrorCode::InvalidArgument, 36),
        ];
        for (code, number) in cases {
            assert_eq!(error_number(code), number, "错误码编号漂移：{code:?}");
        }
    }

    #[test]
    fn change_entry_kind_round_trips_through_props() {
        assert_eq!(
            change_entry_kind(&entry_kind_props(EntryKind::Symlink)),
            EntryKind::Symlink
        );
        assert_eq!(change_entry_kind(&[]), EntryKind::File);
    }

    #[test]
    fn batches_split_by_count_and_size() {
        let objects: Vec<pb::Object> = (0..70)
            .map(|i| pb::Object {
                hash: format!("{i:064x}"),
                kind: pb::ObjectKind::Blob as i32,
                data: vec![0u8; 8],
                compressed: false,
            })
            .collect();
        let batches = batches(objects);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].objects.len(), 32);
        assert_eq!(batches[2].objects.len(), 6);
    }

    #[test]
    fn under_any_matches_prefix_boundaries() {
        assert!(under_any("202610/a.txt", &["202610".to_string()]));
        assert!(!under_any("2026100/a.txt", &["202610".to_string()]));
        assert!(under_any("202610", &["202610".to_string()]));
    }
}
