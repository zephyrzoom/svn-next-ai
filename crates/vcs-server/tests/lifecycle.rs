//! 服务端业务层集成测试。
//!
//! 覆盖 M1 已经落地、且**最容易悄悄失效**的几条约束：
//!   - 认证：bootstrap 只能一次、仅 ACTIVE 可登录、被篡改的 token 不可用；
//!   - 成员门：非成员一律 `REPO_NOT_MEMBER`，reader 不能提交，member 管理按角色矩阵；
//!   - 配额：超出配额上限时在分配版本号之前就拒绝；
//!   - 内容寻址：相同内容只落盘一次，`import` 的差异识别正确。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use vcs_core::{ErrorCode, ObjectStore, RepoMemberRole, Role};
use vcs_server::ServerStore;

const PASSWORD: &str = "Abcdef1!";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "vcs-srv-{tag}-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn store(&self) -> ServerStore {
        ServerStore::open(self.root.join("store")).unwrap()
    }

    fn dir(&self, name: &str) -> PathBuf {
        let p = self.root.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_file(dir: &Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn admin(store: &ServerStore) -> vcs_server::Session {
    store.bootstrap_admin("alice", "张爱丽", PASSWORD).unwrap();
    let (token, _) = store.login("alice", PASSWORD).unwrap();
    store.authenticate(&token).unwrap()
}

// ---------------- 认证 ----------------

#[test]
fn bootstrap_is_allowed_only_once() {
    let sb = Sandbox::new("bootstrap");
    let store = sb.store();
    assert!(!store.has_any_user().unwrap());
    let user = store.bootstrap_admin("alice", "张爱丽", PASSWORD).unwrap();
    assert_eq!(user.role, Role::Admin);
    assert!(store.has_any_user().unwrap());

    // 第二次必须被拒绝，否则任何人都能把自己加成管理员。
    let err = store
        .bootstrap_admin("mallory", "攻击者", PASSWORD)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

#[test]
fn bootstrap_enforces_password_policy() {
    let sb = Sandbox::new("policy");
    let store = sb.store();
    let err = store
        .bootstrap_admin("alice", "张爱丽", "abcdefgh")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PasswordPolicyViolation);
    // 弱口令被拒后仍可正常引导，不会把账号建出来一半。
    assert!(!store.has_any_user().unwrap());
}

#[test]
fn login_rejects_wrong_password_and_tampered_token() {
    let sb = Sandbox::new("login");
    let store = sb.store();
    let session = admin(&store);
    assert_eq!(session.username, "alice");

    let err = store.login("alice", "Abcdef2!").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidCredentials);

    let (token, _) = store.login("alice", PASSWORD).unwrap();
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    parts[1] = "eyJzdWIiOiJmYWtlIn0".to_string();
    assert_eq!(
        store.authenticate(&parts.join(".")).unwrap_err().code,
        ErrorCode::Unauthenticated
    );
}

#[test]
fn only_admins_can_create_users_or_change_global_quota() {
    let sb = Sandbox::new("admin-only");
    let store = sb.store();
    let alice = admin(&store);

    store
        .create_user(&alice, "bob", "李明", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("bob", PASSWORD).unwrap();
    let bob = store.authenticate(&token).unwrap();

    assert_eq!(
        store
            .create_user(&bob, "eve", "夏娃", PASSWORD, Role::Admin)
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        store.set_default_quota(&bob, 1).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
}

// ---------------- 成员门 ----------------

#[test]
fn non_members_cannot_touch_the_repo() {
    let sb = Sandbox::new("membership");
    let store = sb.store();
    let alice = admin(&store);
    store
        .create_user(&alice, "bob", "李明", PASSWORD, Role::User)
        .unwrap();
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();

    let (token, _) = store.login("bob", PASSWORD).unwrap();
    let bob = store.authenticate(&token).unwrap();

    // 即使是 writer 全局角色，没有成员资格也不能读。
    assert_eq!(
        store.files_at(&bob, &repo.repo_id, None).unwrap_err().code,
        ErrorCode::RepoNotMember
    );
    assert_eq!(
        store.log(&bob, &repo.repo_id, 10).unwrap_err().code,
        ErrorCode::RepoNotMember
    );
    assert_eq!(
        store.list_members(&bob, &repo.repo_id).unwrap_err().code,
        ErrorCode::RepoNotMember
    );

    // 加入后才能读。
    store
        .add_member(&alice, &repo.repo_id, "bob", RepoMemberRole::Reader)
        .unwrap();
    assert!(store.files_at(&bob, &repo.repo_id, None).is_ok());
}

#[test]
fn reader_cannot_commit_but_writer_can() {
    let sb = Sandbox::new("roles");
    let store = sb.store();
    let alice = admin(&store);
    store
        .create_user(&alice, "bob", "李明", PASSWORD, Role::User)
        .unwrap();
    store
        .create_user(&alice, "carol", "王芳", PASSWORD, Role::User)
        .unwrap();
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    store
        .add_member(&alice, &repo.repo_id, "bob", RepoMemberRole::Reader)
        .unwrap();
    store
        .add_member(&alice, &repo.repo_id, "carol", RepoMemberRole::Writer)
        .unwrap();

    let src = sb.dir("src");
    write_file(&src, "202610/a.txt", b"v1\n");

    let (token, _) = store.login("bob", PASSWORD).unwrap();
    let bob = store.authenticate(&token).unwrap();
    assert_eq!(
        store
            .import_dir(&bob, &repo.repo_id, &src, "reader 尝试提交")
            .unwrap_err()
            .code,
        ErrorCode::RepoMemberForbidden
    );

    let (token, _) = store.login("carol", PASSWORD).unwrap();
    let carol = store.authenticate(&token).unwrap();
    let outcome = store
        .import_dir(&carol, &repo.repo_id, &src, "writer 提交")
        .unwrap();
    assert_eq!(outcome.rev, 1);
    assert_eq!(outcome.added, 1);
}

#[test]
fn member_assignment_follows_the_role_matrix() {
    let sb = Sandbox::new("matrix");
    let store = sb.store();
    let alice = admin(&store);
    for (name, display) in [("bob", "李明"), ("carol", "王芳")] {
        store
            .create_user(&alice, name, display, PASSWORD, Role::User)
            .unwrap();
    }
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    store
        .add_member(&alice, &repo.repo_id, "bob", RepoMemberRole::Maintainer)
        .unwrap();

    let (token, _) = store.login("bob", PASSWORD).unwrap();
    let bob = store.authenticate(&token).unwrap();

    // maintainer 只能授予 writer / reader
    store
        .add_member(&bob, &repo.repo_id, "carol", RepoMemberRole::Reader)
        .unwrap();
    let (token, _) = store.login("carol", PASSWORD).unwrap();
    let carol = store.authenticate(&token).unwrap();
    assert_eq!(
        store
            .add_member(&bob, &repo.repo_id, "carol", RepoMemberRole::Writer)
            .unwrap_err()
            .code,
        ErrorCode::RepoMemberExists
    );
    // owner 唯一且不可授予：连 owner 自己也不行
    assert_eq!(
        store
            .add_member(&alice, &repo.repo_id, "bob", RepoMemberRole::Owner)
            .unwrap_err()
            .code,
        ErrorCode::RepoMemberForbidden
    );
    // writer 不能管理成员
    assert_eq!(
        store
            .add_member(&carol, &repo.repo_id, "carol", RepoMemberRole::Reader)
            .unwrap_err()
            .code,
        ErrorCode::RepoMemberForbidden
    );

    assert_eq!(store.list_members(&alice, &repo.repo_id).unwrap().len(), 3);
}

// ---------------- 提交与内容寻址 ----------------

#[test]
fn import_is_deduplicated_and_diffed_correctly() {
    let sb = Sandbox::new("import");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let src = sb.dir("src");

    // 两个路径同样内容：只应落盘一个 blob。
    write_file(&src, "202610/a.txt", b"same-content\n");
    write_file(&src, "202611/a.txt", b"same-content\n");
    write_file(&src, "README.md", b"readme\n");

    let first = store.import_dir(&alice, &repo.repo_id, &src, "r1").unwrap();
    assert_eq!(first.rev, 1);
    assert_eq!(first.added, 3);

    let objects_after_first = store.objects().list_hashes().unwrap().len();

    // 无改动时不产生新版本
    let again = store
        .import_dir(&alice, &repo.repo_id, &src, "noop")
        .unwrap();
    assert!(again.no_changes, "内容一致却分配了新版本号");
    assert_eq!(again.rev, 1);

    // 改一个、删一个、加一个
    write_file(&src, "202610/a.txt", b"changed\n");
    fs::remove_file(src.join("README.md")).unwrap();
    write_file(&src, "202612/c.txt", b"new\n");

    let second = store.import_dir(&alice, &repo.repo_id, &src, "r2").unwrap();
    assert_eq!(second.rev, 2);
    assert_eq!((second.added, second.modified, second.deleted), (1, 1, 1));
    assert_eq!(second.unchanged, 1);

    assert!(store.objects().list_hashes().unwrap().len() > objects_after_first);
}

#[test]
fn log_and_read_file_reflect_each_revision() {
    let sb = Sandbox::new("log");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let src = sb.dir("src");
    write_file(&src, "202610/summary.txt", b"v1\n");
    store
        .import_dir(&alice, &repo.repo_id, &src, "初始导入")
        .unwrap();
    write_file(&src, "202610/summary.txt", b"v2\n");
    store
        .import_dir(&alice, &repo.repo_id, &src, "修订")
        .unwrap();

    let entries = store.log(&alice, &repo.repo_id, 10).unwrap();
    assert_eq!(entries.len(), 3); // r0 empty root + r1 + r2
    assert_eq!(entries[0].rev, 2);
    assert_eq!(entries[0].author, "alice");

    assert_eq!(
        store
            .read_file(&alice, &repo.repo_id, Some(1), "202610/summary.txt")
            .unwrap(),
        b"v1\n"
    );
    assert_eq!(
        store
            .read_file(&alice, &repo.repo_id, Some(2), "202610/summary.txt")
            .unwrap(),
        b"v2\n"
    );

    let changes = store.revision_changes(&repo.repo_id, 2).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "202610/summary.txt");
}

// ---------------- 配额 ----------------

#[test]
fn quota_is_enforced_before_allocating_a_revision() {
    let sb = Sandbox::new("quota");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();

    // 全局默认配额压到 1 KiB，仓库继承它。
    store.set_default_quota(&alice, 1024).unwrap();
    let effective = store.effective_quota(&repo.repo_id).unwrap();
    assert_eq!(effective.bytes, 1024);

    let src = sb.dir("src");
    write_file(&src, "202610/big.bin", &vec![b'x'; 8 * 1024]);

    let err = store
        .import_dir(&alice, &repo.repo_id, &src, "超大文件")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::QuotaExceeded);
    // 被拒绝后 head 保持不变，用户清理后可以重试。
    assert_eq!(store.find_repo(&repo.repo_id).unwrap().head_rev, 0);
}

#[test]
fn rejected_import_writes_nothing_to_the_object_pool() {
    // 回归用例：曾经「先写对象再查配额」，被拒绝的提交会在盘上留下几十 KB 孤儿对象，
    // 用户反复重试就能把磁盘填满 —— 拒绝本身成了攻击面。
    let sb = Sandbox::new("quota-nowrite");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 1024).unwrap();
    // 建库本身会写入一个空树对象，这是基线，不算提交产生的占用。
    let before = store.objects().list_hashes().unwrap().len();
    let bytes_before = store.objects().total_stored_bytes().unwrap();

    let src = sb.dir("src");
    write_file(&src, "big.bin", &vec![b'z'; 16 * 1024]);

    assert_eq!(
        store
            .import_dir(&alice, &repo.repo_id, &src, "超大")
            .unwrap_err()
            .code,
        ErrorCode::QuotaExceeded
    );

    let after = store.objects().list_hashes().unwrap().len();
    assert_eq!(
        before, after,
        "被配额拒绝的提交不应写入任何对象（{} → {}）",
        before, after
    );
    assert_eq!(
        store.objects().total_stored_bytes().unwrap(),
        bytes_before,
        "被拒绝的提交不应增加任何对象字节"
    );
}

#[test]
fn repo_level_quota_overrides_global_default() {
    let sb = Sandbox::new("quota-override");
    let store = sb.store();
    let alice = admin(&store);
    store.set_default_quota(&alice, 1024).unwrap();
    let repo = store
        .create_repo_direct(&alice, "reports", 8 * 1024 * 1024)
        .unwrap();

    let effective = store.effective_quota(&repo.repo_id).unwrap();
    assert_eq!(effective.bytes, 8 * 1024 * 1024);
    assert_eq!(effective.source, vcs_core::QuotaSource::RepoOverride);

    let src = sb.dir("src");
    write_file(&src, "big.bin", &vec![b'x'; 8 * 1024]);
    // 继承全局默认会失败，这里应该成功。
    assert!(store.import_dir(&alice, &repo.repo_id, &src, "ok").is_ok());
}

// ---------------- 仓库命名与状态 ----------------

#[test]
fn repo_names_are_validated_and_unique() {
    let sb = Sandbox::new("names");
    let store = sb.store();
    let alice = admin(&store);
    store.create_repo_direct(&alice, "reports", 0).unwrap();

    assert_eq!(
        store
            .create_repo_direct(&alice, "reports", 0)
            .unwrap_err()
            .code,
        ErrorCode::RepoNameInvalid
    );
    for bad in ["", "A", "1", "with space", "中文仓库", "-leading"] {
        assert_eq!(
            store.create_repo_direct(&alice, bad, 0).unwrap_err().code,
            ErrorCode::RepoNameInvalid,
            "应当拒绝非法仓库名 {bad:?}"
        );
    }
}

// ---------------- 两阶段提交会话 ----------------

fn seed_blob(store: &ServerStore, content: &[u8]) -> vcs_core::Hash {
    let hash = vcs_core::hash_bytes(content);
    store.objects().put(&hash, content).unwrap();
    hash
}

#[test]
fn begin_commit_rejects_out_of_date_base() {
    let sb = Sandbox::new("ood");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let src = sb.dir("src");
    write_file(&src, "a.txt", b"a\n");
    store.import_dir(&alice, &repo.repo_id, &src, "r1").unwrap();

    let hash = seed_blob(&store, b"b\n");
    let change = vcs_server::PendingChange {
        path: "b.txt".into(),
        kind: vcs_core::ChangeKind::Add,
        entry_kind: vcs_core::EntryKind::File,
        hash: Some(hash),
        size: 2,
    };
    // 基于 r1 提交前 head 已到 r1，这里用 r0 就应被拒
    let err = store
        .begin_commit(&alice, &repo.repo_id, 0, &[change], "落后提交", 2)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::OutOfDate);
}

#[test]
fn finalize_requires_pushed_objects() {
    let sb = Sandbox::new("nopush");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();

    // 只算哈希不推送：finalize 必须拒绝，否则会写出悬空引用
    let content = b"never pushed\n";
    let hash = vcs_core::hash_bytes(content);
    let change = vcs_server::PendingChange {
        path: "a.txt".into(),
        kind: vcs_core::ChangeKind::Add,
        entry_kind: vcs_core::EntryKind::File,
        hash: Some(hash),
        size: content.len() as u64,
    };
    let commit = store
        .begin_commit(&alice, &repo.repo_id, 0, &[change], "没推送就提交", 14)
        .unwrap();

    let err = store
        .finalize_commit(&alice, &commit.session_id)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ObjectMissing);
    assert_eq!(store.find_repo(&repo.repo_id).unwrap().head_rev, 0);
}

#[test]
fn push_rejects_content_that_does_not_match_its_address() {
    let sb = Sandbox::new("badpush");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let hash = seed_blob(&store, b"good\n");
    let change = vcs_server::PendingChange {
        path: "a.txt".into(),
        kind: vcs_core::ChangeKind::Add,
        entry_kind: vcs_core::EntryKind::File,
        hash: Some(hash),
        size: 5,
    };
    let commit = store
        .begin_commit(&alice, &repo.repo_id, 0, &[change], "错内容", 5)
        .unwrap();

    let err = store
        .push_objects(&alice, &commit.session_id, &[(hash, b"evil\n".to_vec())])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ObjectMissing);
}

#[test]
fn second_session_finalize_is_rejected_as_out_of_date() {
    // 严格 out-of-date 的核心竞态：两个会话都基于 r0，先完成的赢，后完成的被拒。
    let sb = Sandbox::new("race");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();

    let a = seed_blob(&store, b"A\n");
    let b = seed_blob(&store, b"B\n");
    let ca = store
        .begin_commit(
            &alice,
            &repo.repo_id,
            0,
            &[vcs_server::PendingChange {
                path: "a.txt".into(),
                kind: vcs_core::ChangeKind::Add,
                entry_kind: vcs_core::EntryKind::File,
                hash: Some(a),
                size: 2,
            }],
            "A",
            2,
        )
        .unwrap();
    let cb = store
        .begin_commit(
            &alice,
            &repo.repo_id,
            0,
            &[vcs_server::PendingChange {
                path: "b.txt".into(),
                kind: vcs_core::ChangeKind::Add,
                entry_kind: vcs_core::EntryKind::File,
                hash: Some(b),
                size: 2,
            }],
            "B",
            2,
        )
        .unwrap();

    store.finalize_commit(&alice, &ca.session_id).unwrap();
    let err = store.finalize_commit(&alice, &cb.session_id).unwrap_err();
    assert_eq!(err.code, ErrorCode::OutOfDate);
    assert_eq!(store.find_repo(&repo.repo_id).unwrap().head_rev, 1);
    // 被拒的会话已作废，不能再次提交
    assert_eq!(
        store
            .finalize_commit(&alice, &cb.session_id)
            .unwrap_err()
            .code,
        ErrorCode::InvalidSession
    );
}

#[test]
fn aborted_session_cannot_push() {
    let sb = Sandbox::new("abort");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let hash = seed_blob(&store, b"x\n");
    let commit = store
        .begin_commit(
            &alice,
            &repo.repo_id,
            0,
            &[vcs_server::PendingChange {
                path: "x.txt".into(),
                kind: vcs_core::ChangeKind::Add,
                entry_kind: vcs_core::EntryKind::File,
                hash: Some(hash),
                size: 2,
            }],
            "放弃",
            2,
        )
        .unwrap();
    store.abort_commit(&alice, &commit.session_id).unwrap();
    assert_eq!(
        store
            .push_objects(&alice, &commit.session_id, &[(hash, b"x\n".to_vec())])
            .unwrap_err()
            .code,
        ErrorCode::InvalidSession
    );
}

#[test]
fn commit_is_copy_on_write_for_untouched_directories() {
    // 真正的收益验证：改 202611 不应重写 202610 的子树。
    let sb = Sandbox::new("cow");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let src = sb.dir("src");
    write_file(&src, "202610/a.txt", b"a\n");
    write_file(&src, "202611/b.txt", b"b\n");
    store.import_dir(&alice, &repo.repo_id, &src, "r1").unwrap();

    let before_root = store.revision_root_tree(&repo.repo_id, 1).unwrap();

    write_file(&src, "202611/b.txt", b"b2\n");
    store.import_dir(&alice, &repo.repo_id, &src, "r2").unwrap();
    let after_root = store.revision_root_tree(&repo.repo_id, 2).unwrap();
    assert_ne!(before_root, after_root);

    let flat_before = vcs_core::flatten_tree(store.objects(), &before_root).unwrap();
    let flat_after = vcs_core::flatten_tree(store.objects(), &after_root).unwrap();
    let nov_before = flat_before
        .iter()
        .find(|e| e.path == "202610/a.txt")
        .unwrap();
    let nov_after = flat_after
        .iter()
        .find(|e| e.path == "202610/a.txt")
        .unwrap();
    assert_eq!(nov_before.hash, nov_after.hash, "未改动文件的内容被重写了");
}

#[test]
fn get_changes_diffs_two_revisions() {
    let sb = Sandbox::new("getchanges");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let src = sb.dir("src");
    write_file(&src, "keep.txt", b"k\n");
    write_file(&src, "gone.txt", b"g\n");
    store.import_dir(&alice, &repo.repo_id, &src, "r1").unwrap();

    fs::remove_file(src.join("gone.txt")).unwrap();
    write_file(&src, "new.txt", b"n\n");
    write_file(&src, "keep.txt", b"k2\n");
    store.import_dir(&alice, &repo.repo_id, &src, "r2").unwrap();

    let changes = store.get_changes(&alice, &repo.repo_id, 1, 2).unwrap();
    let mut summary: Vec<(String, vcs_core::ChangeKind)> =
        changes.into_iter().map(|c| (c.path, c.kind)).collect();
    summary.sort();
    assert_eq!(
        summary,
        vec![
            ("gone.txt".to_string(), vcs_core::ChangeKind::Delete),
            ("keep.txt".to_string(), vcs_core::ChangeKind::Modify),
            ("new.txt".to_string(), vcs_core::ChangeKind::Add),
        ]
    );
}

// ---------------- 容量治理：清理与 GC ----------------

/// 造一个典型按月归档的仓库：202610 与 202611 各有一份独有内容，
/// 另有一个两个月份都用的共享内容（共享 CAS 下只存一份）。
fn seed_monthly_repo(
    sb: &Sandbox,
    store: &ServerStore,
) -> (vcs_server::Session, String, vcs_core::Hash, vcs_core::Hash) {
    let alice = admin(store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let src = sb.dir("src");
    write_file(&src, "202610/only-oct.txt", b"oct only\n");
    write_file(&src, "202611/only-nov.txt", b"nov only\n");
    write_file(&src, "202610/shared.txt", b"shared content\n");
    write_file(&src, "202611/shared.txt", b"shared content\n");
    store.import_dir(&alice, &repo.repo_id, &src, "r1").unwrap();

    let oct_only = vcs_core::hash_bytes(b"oct only\n");
    let shared = vcs_core::hash_bytes(b"shared content\n");
    (alice, repo.repo_id, oct_only, shared)
}

#[test]
fn estimate_purge_separates_unique_and_shared_bytes() {
    let sb = Sandbox::new("estimate");
    let store = sb.store();
    let (alice, repo_id, oct_only, shared) = seed_monthly_repo(&sb, &store);

    let estimate = store
        .estimate_purge(&alice, &repo_id, &[String::from("202610")])
        .unwrap();

    let shared_bytes = store.objects().stored_size(&shared).unwrap();
    assert_eq!(
        estimate.shared_bytes, shared_bytes,
        "共享内容必须算在共享里"
    );
    assert!(
        estimate.reclaimable_bytes >= store.objects().stored_size(&oct_only).unwrap(),
        "独占内容必须算在可回收里"
    );
    // 逻辑大小包含共享内容的完整字节；可回收 + 共享 才是全量。
    assert_eq!(estimate.logical_bytes, 24, "202610 的两个文件共 24 字节");
    assert!(estimate.affected_revisions >= 1);
    assert!(estimate.shared_paths.iter().any(|p| p.contains("202611")));
    // dry-run 不能改任何东西。
    assert!(store.objects().exists(&oct_only));
}

#[test]
fn immediate_purge_reclaims_only_uniquely_owned_content() {
    let sb = Sandbox::new("purge-immediate");
    let store = sb.store();
    let (alice, repo_id, oct_only, shared) = seed_monthly_repo(&sb, &store);
    let oct_bytes = store.objects().stored_size(&oct_only).unwrap();

    let outcome = store
        .purge(
            &alice,
            &repo_id,
            &[String::from("202610")],
            true,
            "磁盘不足",
        )
        .unwrap();

    assert_eq!(outcome.deleted, 2, "202610 下两个文件都该从 HEAD 移除");
    assert!(outcome.rev >= 2);
    let status = store
        .gc_status(&alice, &repo_id, &outcome.gc_job_id)
        .unwrap();
    assert_eq!(status.state, "done");
    assert!(status.reclaimed_bytes > 0, "立即释放必须真的腾出空间");

    // HEAD 只剩 202611。
    let (_, _, files) = store.files_at(&alice, &repo_id, None).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.iter().all(|p| p.starts_with("202611/")));

    assert!(!store.objects().exists(&oct_only), "独占内容应被物理删除");
    assert!(
        store.objects().exists(&shared),
        "被 202611 共享的内容不能删"
    );
    // 释放量至少要覆盖那份独占内容（tree 对象按设计不回收，会带来少量新增）。
    assert!(status.reclaimed_bytes >= oct_bytes);
}

#[test]
fn purged_content_reports_content_purged_instead_of_missing() {
    let sb = Sandbox::new("purged-read");
    let store = sb.store();
    let (alice, repo_id, oct_only, _) = seed_monthly_repo(&sb, &store);
    store
        .purge(&alice, &repo_id, &[String::from("202610")], true, "清理")
        .unwrap();

    // r1 的树里仍留着这个路径（元数据永久保留），但内容已经没了。
    let err = store
        .read_file(&alice, &repo_id, Some(1), "202610/only-oct.txt")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ContentPurged);
    assert!(!store.objects().exists(&oct_only));

    // 未被清理的路径照常可读。
    let data = store
        .read_file(&alice, &repo_id, None, "202611/only-nov.txt")
        .unwrap();
    assert_eq!(data, b"nov only\n");
}

#[test]
fn grace_period_keeps_content_until_gc_promotes_the_purge() {
    let sb = Sandbox::new("purge-grace");
    let store = sb.store();
    let (alice, repo_id, oct_only, _) = seed_monthly_repo(&sb, &store);

    let outcome = store
        .purge(
            &alice,
            &repo_id,
            &[String::from("202610")],
            false,
            "误删保护",
        )
        .unwrap();
    // 未立即释放：任务只是排队，内容一个字节都没动。
    let queued = store
        .gc_status(&alice, &repo_id, &outcome.gc_job_id)
        .unwrap();
    assert_eq!(queued.state, "queued");
    assert!(store.objects().exists(&oct_only));

    // 手动 GC 时宽限期未到，PENDING 不该被当成 ACTIVE。
    let status = store.run_gc(&alice, &repo_id).unwrap();
    assert_eq!(status.state, "done");
    assert_eq!(status.reclaimed_bytes, 0);
    assert!(store.objects().exists(&oct_only), "宽限期内不能回收");
}

#[test]
fn gc_never_reclaims_content_used_by_another_repository() {
    let sb = Sandbox::new("gc-crossrepo");
    let store = sb.store();
    let (alice, repo_id, _, shared) = seed_monthly_repo(&sb, &store);

    // 另一个仓库引用同一份内容（共享 CAS 下天然去重）。
    let other = store.create_repo_direct(&alice, "archive", 0).unwrap();
    let other_src = sb.dir("other");
    write_file(&other_src, "copy.txt", b"shared content\n");
    store
        .import_dir(&alice, &other.repo_id, &other_src, "r1")
        .unwrap();

    store
        .purge(&alice, &repo_id, &[String::from("202610")], true, "清理")
        .unwrap();
    // 再跑一次全库 GC 也不能动别人的内容。
    store.run_gc(&alice, &other.repo_id).unwrap();

    assert!(store.objects().exists(&shared));
    let data = store
        .read_file(&alice, &other.repo_id, None, "copy.txt")
        .unwrap();
    assert_eq!(data, b"shared content\n");
}

#[test]
fn purge_requires_write_role() {
    let sb = Sandbox::new("purge-role");
    let store = sb.store();
    let (alice, repo_id, _, _) = seed_monthly_repo(&sb, &store);
    store
        .create_user(&alice, "reader1", "只读者", PASSWORD, Role::User)
        .unwrap();
    store
        .add_member(&alice, &repo_id, "reader1", RepoMemberRole::Reader)
        .unwrap();
    let (token, _) = store.login("reader1", PASSWORD).unwrap();
    let reader = store.authenticate(&token).unwrap();

    let err = store
        .purge(&reader, &repo_id, &[String::from("202610")], true, "试试")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoMemberForbidden);
}

#[test]
fn list_purge_candidates_ranks_by_unique_bytes() {
    let sb = Sandbox::new("candidates");
    let store = sb.store();
    let (alice, repo_id, _, shared) = seed_monthly_repo(&sb, &store);

    let candidates = store.list_purge_candidates(&alice, &repo_id, 10).unwrap();
    let paths: Vec<&str> = candidates.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, vec!["202610", "202611"]);

    let shared_bytes = store.objects().stored_size(&shared).unwrap();
    for c in &candidates {
        // 每个月份都独占自己那份 only-*.txt，共享内容不该进 unique_bytes。
        assert!(c.unique_bytes > 0);
        assert!(
            c.logical_bytes > c.unique_bytes - 1,
            "逻辑大小应不小于独占大小"
        );
        let _ = shared_bytes;
    }
}

// ---------------- 用户体系：注册、审批、仓库申请、PAT ----------------

#[test]
fn registration_takes_password_at_signup_and_still_needs_approval() {
    let sb = Sandbox::new("register");
    let store = sb.store();
    let alice = admin(&store);

    // 密码在注册时就校验：不合规直接拒掉，不留下任何账号或申请单。
    let err = store
        .register("bob", "李波", "short", "需要一个账号归档月度报表")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PasswordPolicyViolation);
    assert!(err.message.contains("至少 8 位"), "{}", err.message);
    let err = store.login("bob", PASSWORD).unwrap_err();
    assert_eq!(
        err.code,
        ErrorCode::InvalidCredentials,
        "被拒的申请不该建号"
    );

    let reg = store
        .register("bob", "李波", PASSWORD, "需要一个账号归档月度报表")
        .unwrap();
    assert_eq!(reg.info.username, "bob");
    assert!(!reg.first_user, "库里已有管理员，不应再自动批准");
    assert!(matches!(
        reg.info.state,
        vcs_core::RegistrationStatus::Pending
    ));

    // 未审批：密码即使正确也不能登录，且错误码要能让客户端显示「等待审批」。
    let err = store.login("bob", PASSWORD).unwrap_err();
    assert_eq!(err.code, ErrorCode::AccountPending);

    // 审批只放行，不再收集密码：审批后直接用注册时设置的密码登录。
    store
        .approve_registration(&alice, &reg.info.request_id)
        .unwrap();
    let err = store.login("bob", "Other1!x").unwrap_err();
    assert_eq!(
        err.code,
        ErrorCode::InvalidCredentials,
        "密码不该被审批改写"
    );
    let (token, user) = store.login("bob", PASSWORD).unwrap();
    assert_eq!(user.username, "bob");
    // 审批只放行、不选类型：通过即普通用户，读写权限按仓库授予。
    assert_eq!(user.role, Role::User);

    // token 版本在审批时递增：审批前签发过的凭证一律失效。
    let session = store.authenticate(&token).unwrap();
    assert_eq!(session.username, "bob");
    assert!(session.scopes.is_none());
}

#[test]
fn registration_rejects_duplicates_and_empty_reasons() {
    let sb = Sandbox::new("register-dup");
    let store = sb.store();
    let alice = admin(&store);

    store
        .register("bob", "李波", PASSWORD, "第一次申请")
        .unwrap();
    // 同一个用户名重复申请：明确拒绝，避免刷出一堆待审批单。
    let err = store
        .register("bob", "李波", PASSWORD, "再申请一次")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RegistrationExists);
    // 与已有账号重名。
    let err = store
        .register("alice", "张爱丽", PASSWORD, "冒名")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RegistrationExists);

    let reg = store.register("carol", "王佳", PASSWORD, "申请").unwrap();
    let err = store
        .reject_registration(&alice, &reg.info.request_id, "   ")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument, "驳回必须写原因");

    store
        .reject_registration(&alice, &reg.info.request_id, "用途不明确")
        .unwrap();
    let err = store.login("carol", PASSWORD).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidCredentials);
    // 已处理的申请不能重复处理。
    let err = store
        .reject_registration(&alice, &reg.info.request_id, "再拒一次")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalNotFound);
}

#[test]
fn only_admins_can_see_or_approve_registrations() {
    let sb = Sandbox::new("register-role");
    let store = sb.store();
    let alice = admin(&store);
    store
        .create_user(&alice, "dave", "赵大", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("dave", PASSWORD).unwrap();
    let dave = store.authenticate(&token).unwrap();

    let reg = store.register("eve", "钱一", PASSWORD, "申请").unwrap();
    let err = store.list_registrations(&dave, None, 10).unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    let err = store
        .approve_registration(&dave, &reg.info.request_id)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);

    let list = store.list_registrations(&alice, None, 10).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].username, "eve");
}

#[test]
fn registration_password_policy_is_enforced_server_side() {
    let sb = Sandbox::new("register-policy");
    let store = sb.store();
    let alice = admin(&store);

    // 服务端是安全边界：不合规的密码必须被拒，并列出缺什么。
    let err = store
        .register("frank", "孙飞", "short", "申请")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PasswordPolicyViolation);
    assert!(err.message.contains("至少 8 位"), "{}", err.message);

    // 拒掉之后可以用合规密码重来：失败的申请什么都没留下。
    let reg = store.register("frank", "孙飞", PASSWORD, "申请").unwrap();
    assert_eq!(reg.info.username, "frank");
    assert_eq!(store.list_registrations(&alice, None, 10).unwrap().len(), 1);
}

#[test]
fn repo_request_stays_unusable_until_approved() {
    let sb = Sandbox::new("repo-request");
    let store = sb.store();
    let alice = admin(&store);

    let req = store
        .request_repo(&alice, "monthly", "按月归档报表")
        .unwrap();
    assert_eq!(req.repo_name, "monthly");
    // 容量不用填：服务端固定按初始配额落库，审批人也不会再看到配额输入框。
    assert_eq!(
        req.requested_quota_bytes,
        vcs_core::REPO_INITIAL_QUOTA_BYTES
    );
    // 未审批：仓库不存在，任何 RepoService 调用都拿不到它。
    let err = store.find_repo("monthly").unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoNotFound);

    let repo = store
        .approve_repo_request(&alice, &req.request_id, "同意")
        .unwrap();
    assert_eq!(repo.name, "monthly");
    assert_eq!(repo.owner_user_id, alice.user_id);
    assert_eq!(
        store.effective_quota(&repo.repo_id).unwrap().bytes,
        vcs_core::REPO_INITIAL_QUOTA_BYTES
    );

    // 申请人自动成为 owner，且仓库从 r0 空版本开始可用。
    let found = store.find_repo("monthly").unwrap();
    assert_eq!(found.repo_id, repo.repo_id);
    assert_eq!(
        store.member_role(&repo.repo_id, &alice.user_id).unwrap(),
        RepoMemberRole::Owner
    );
    let (_, rev, files) = store.files_at(&alice, &repo.repo_id, None).unwrap();
    assert_eq!(rev, 0);
    assert!(files.is_empty());

    // 重复审批必须被拒，否则会建出第二个同名仓库。
    let err = store
        .approve_repo_request(&alice, &req.request_id, "再批一次")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoApprovalNotFound);
}

#[test]
fn quota_expansion_adds_one_fixed_step() {
    let sb = Sandbox::new("quota-expand");
    let store = sb.store();
    let alice = admin(&store);
    store
        .create_user(&alice, "heidi", "何迪", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("heidi", PASSWORD).unwrap();
    let heidi = store.authenticate(&token).unwrap();

    let req = store.request_repo(&heidi, "assets", "素材库").unwrap();
    let repo = store
        .approve_repo_request(&alice, &req.request_id, "同意")
        .unwrap();
    let before = store.effective_quota(&repo.repo_id).unwrap().bytes;
    assert_eq!(before, vcs_core::REPO_INITIAL_QUOTA_BYTES);

    // 成员申请扩容：不填容量，增量由服务端固定。
    let expansion = store
        .request_repo_expansion(&heidi, &repo.repo_id, "素材变多了")
        .unwrap();
    assert_eq!(
        expansion.increment_bytes,
        vcs_core::REPO_QUOTA_EXPANSION_STEP_BYTES
    );
    assert_eq!(expansion.quota_before_bytes, before);
    assert_eq!(expansion.target_bytes(), before + expansion.increment_bytes);

    // 同一仓库同时只允许一条待审批申请。
    let err = store
        .request_repo_expansion(&heidi, &repo.repo_id, "再来一次")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoCreationExists);

    // 审批前上限不变；审批后才增加一个固定步长。
    assert_eq!(store.effective_quota(&repo.repo_id).unwrap().bytes, before);
    let effective = store
        .approve_repo_quota_request(&alice, &expansion.request_id, "同意")
        .unwrap();
    assert_eq!(
        effective.bytes,
        before + vcs_core::REPO_QUOTA_EXPANSION_STEP_BYTES
    );
    assert_eq!(
        store.effective_quota(&repo.repo_id).unwrap().bytes,
        effective.bytes
    );

    // 重复审批必须被拒。
    let err = store
        .approve_repo_quota_request(&alice, &expansion.request_id, "再批一次")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoApprovalNotFound);

    // 非成员不能替别人的仓库要空间。
    let err = store
        .request_repo_expansion(&alice, &repo.repo_id, "我不是成员")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoNotMember);
}

#[test]
fn repo_request_rejects_duplicates_and_limits_visibility() {
    let sb = Sandbox::new("repo-request-vis");
    let store = sb.store();
    let alice = admin(&store);
    store
        .create_user(&alice, "grace", "周洁", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("grace", PASSWORD).unwrap();
    let grace = store.authenticate(&token).unwrap();

    let req = store
        .request_repo(&grace, "grace-repo", "个人归档")
        .unwrap();
    let err = store
        .request_repo(&grace, "grace-repo", "再申请")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoCreationExists);

    // 普通用户只看得到自己的申请。
    let mine = store.list_repo_requests(&grace, None, 10).unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].request_id, req.request_id);

    let all = store.list_repo_requests(&alice, None, 10).unwrap();
    assert_eq!(all.len(), 1);

    // 驳回必须写原因；驳回后状态是 rejected 且仍然没有仓库。
    let err = store
        .reject_repo_request(&alice, &req.request_id, "")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    store
        .reject_repo_request(&alice, &req.request_id, "暂不开放个人仓库")
        .unwrap();
    assert!(store.find_repo("grace-repo").is_err());

    // 已占用名字不能再申请。
    store.create_repo_direct(&alice, "taken", 0).unwrap();
    let err = store.request_repo(&grace, "taken", "重复").unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoCreationExists);
}

#[test]
fn pat_authenticates_and_scopes_cap_what_it_can_do() {
    let sb = Sandbox::new("pat");
    let store = sb.store();
    let alice = admin(&store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let src = sb.dir("pat-src");
    write_file(&src, "a.txt", b"a\n");
    store.import_dir(&alice, &repo.repo_id, &src, "r1").unwrap();

    // 只读令牌：能读，不能提交。
    let (read_token, read_id) = store
        .create_pat(&alice, "ci-readonly", &["read".to_string()], None)
        .unwrap();
    let read_session = store.authenticate(&read_token).unwrap();
    assert_eq!(read_session.username, "alice");
    assert!(read_session.permits("read"));
    assert!(!read_session.permits("write"));
    store.files_at(&read_session, &repo.repo_id, None).unwrap();
    let pending = vec![vcs_server::PendingChange::file(
        "a.txt",
        vcs_core::ChangeKind::Modify,
        Some(vcs_core::hash_bytes(b"a2\n")),
        3,
    )];
    let err = store
        .begin_commit(&read_session, &repo.repo_id, 1, &pending, "越权提交", 0)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);

    // 带写入能力的令牌才能提交。
    let (write_token, _) = store
        .create_pat(
            &alice,
            "ci-writer",
            &["read".to_string(), "write".to_string()],
            None,
        )
        .unwrap();
    let write_session = store.authenticate(&write_token).unwrap();
    let commit = store
        .begin_commit(&write_session, &repo.repo_id, 1, &pending, "令牌提交", 0)
        .unwrap();
    store
        .push_objects(
            &write_session,
            &commit.session_id,
            &[(vcs_core::hash_bytes(b"a2\n"), b"a2\n".to_vec())],
        )
        .unwrap();
    store
        .finalize_commit(&write_session, &commit.session_id)
        .unwrap();

    // 撤销后立即失效。
    store.revoke_pat(&alice, &read_id).unwrap();
    let err = store.authenticate(&read_token).unwrap_err();
    assert_eq!(err.code, ErrorCode::PatRevoked);

    // 明文不落库：列表里只有元信息。
    let tokens = store.list_pats(&alice).unwrap();
    assert_eq!(tokens.len(), 2);
    assert!(tokens.iter().all(|t| t.token_id != write_token));
}

#[test]
fn pat_cannot_exceed_the_owner_role() {
    let sb = Sandbox::new("pat-scope");
    let store = sb.store();
    let alice = admin(&store);
    store
        .create_user(&alice, "henry", "吴涵", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("henry", PASSWORD).unwrap();
    let henry = store.authenticate(&token).unwrap();

    // 普通用户不能给自己签发 admin 令牌，否则提权一步到位。
    let err = store
        .create_pat(&henry, "提权", &["admin".to_string()], None)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    // 未知 scope 与非法过期时间都是入参错误。
    let err = store
        .create_pat(&henry, "怪 scope", &["superuser".to_string()], None)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    let err = store
        .create_pat(&henry, "过去时间", &["read".to_string()], Some(1))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);

    // 别人的令牌不能撤销。
    let (_, alice_pat) = store
        .create_pat(&alice, "alice-ci", &["read".to_string()], None)
        .unwrap();
    let err = store.revoke_pat(&henry, &alice_pat).unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

// ---------------- 成员管理、配额覆盖与仓库状态 ----------------

/// 建一个仓库并加几个不同角色的成员，返回 (owner, repo_id, {name: session})。
fn repo_with_team(
    _sb: &Sandbox,
    store: &ServerStore,
) -> (
    vcs_server::Session,
    String,
    Vec<(String, vcs_server::Session)>,
) {
    let alice = admin(store);
    let repo = store.create_repo_direct(&alice, "reports", 0).unwrap();
    let mut team = Vec::new();
    for (name, role) in [
        ("dave", RepoMemberRole::Maintainer),
        ("bob", RepoMemberRole::Writer),
        ("carol", RepoMemberRole::Reader),
    ] {
        store
            .create_user(&alice, name, "成员", PASSWORD, Role::User)
            .unwrap();
        store.add_member(&alice, &repo.repo_id, name, role).unwrap();
        let (token, _) = store.login(name, PASSWORD).unwrap();
        team.push((name.to_string(), store.authenticate(&token).unwrap()));
    }
    (alice, repo.repo_id, team)
}

fn member(team: &[(String, vcs_server::Session)], name: &str) -> vcs_server::Session {
    team.iter().find(|(n, _)| n == name).unwrap().1.clone()
}

#[test]
fn member_removal_follows_the_role_matrix_and_aborts_sessions() {
    let sb = Sandbox::new("member-remove");
    let store = sb.store();
    let (alice, repo_id, team) = repo_with_team(&sb, &store);
    let bob = member(&team, "bob");
    let carol = member(&team, "carol");
    let dave = member(&team, "dave");

    // bob 有一个已经 Begin 但没 Finalize 的会话
    let pending = vec![vcs_server::PendingChange::file(
        "a.txt",
        vcs_core::ChangeKind::Add,
        Some(vcs_core::hash_bytes(b"a\n")),
        2,
    )];
    let commit = store
        .begin_commit(&bob, &repo_id, 0, &pending, "半截提交", 2)
        .unwrap();

    // owner 移人：会话必须立刻作废，而不是等对方点提交才失败。
    store.remove_member(&alice, &repo_id, "bob").unwrap();
    assert_eq!(
        store.member_role(&repo_id, &bob.user_id).unwrap_err().code,
        ErrorCode::RepoNotMember
    );
    let err = store.finalize_commit(&bob, &commit.session_id).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidSession);

    // owner 受保护：谁都不能移除或降级 owner
    let err = store.remove_member(&alice, &repo_id, "alice").unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoOwnerProtected);
    let err = store.remove_member(&dave, &repo_id, "alice").unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoOwnerProtected);

    // maintainer 可以移除 reader，但不能移除同级
    store.remove_member(&dave, &repo_id, "carol").unwrap();
    let err = store.remove_member(&dave, &repo_id, "dave").unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoMemberForbidden, "不能移除自己");

    // reader 不能管理成员；移除不存在的成员报明确的错
    let err = store.remove_member(&carol, &repo_id, "dave").unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoNotMember);
    let err = store.remove_member(&alice, &repo_id, "nobody").unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoMemberNotFound);
}

#[test]
fn member_role_change_downgrades_and_aborts_sessions() {
    let sb = Sandbox::new("member-role");
    let store = sb.store();
    let (alice, repo_id, team) = repo_with_team(&sb, &store);
    let bob = member(&team, "bob");
    let dave = member(&team, "dave");

    let pending = vec![vcs_server::PendingChange::file(
        "a.txt",
        vcs_core::ChangeKind::Add,
        Some(vcs_core::hash_bytes(b"a\n")),
        2,
    )];
    let commit = store
        .begin_commit(&bob, &repo_id, 0, &pending, "半截提交", 2)
        .unwrap();

    // writer → reader：会话立刻作废，之后的提交直接被成员角色挡住
    let updated = store
        .set_member_role(&alice, &repo_id, "bob", RepoMemberRole::Reader)
        .unwrap();
    assert_eq!(updated.role, RepoMemberRole::Reader);
    let err = store.finalize_commit(&bob, &commit.session_id).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidSession);
    let err = store
        .begin_commit(&bob, &repo_id, 0, &pending, "只读还想提", 2)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoMemberForbidden);

    // owner 不可降级；maintainer 不能造 maintainer
    let err = store
        .set_member_role(&alice, &repo_id, "alice", RepoMemberRole::Writer)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoOwnerProtected);
    let err = store
        .set_member_role(&dave, &repo_id, "bob", RepoMemberRole::Maintainer)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoMemberForbidden);
}

#[test]
fn repo_quota_override_can_be_set_and_inherited() {
    let sb = Sandbox::new("repo-quota");
    let store = sb.store();
    let (alice, repo_id, _) = repo_with_team(&sb, &store);
    store
        .create_user(&alice, "bob2", "普通用户", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("bob2", PASSWORD).unwrap();
    let bob2 = store.authenticate(&token).unwrap();

    // 管理员核定 1 MiB 上限
    let effective = store
        .set_repo_quota(&alice, &repo_id, 1024 * 1024, false)
        .unwrap();
    assert_eq!(effective.bytes, 1024 * 1024);
    assert_eq!(effective.source, vcs_core::QuotaSource::RepoOverride);

    // 非管理员不能改
    let err = store
        .set_repo_quota(&bob2, &repo_id, 2048, false)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);

    // 清零即恢复继承
    let inherited = store.set_repo_quota(&alice, &repo_id, 0, true).unwrap();
    assert_ne!(inherited.source, vcs_core::QuotaSource::RepoOverride);
    assert_eq!(store.repo_quota_override(&repo_id).unwrap(), 0);
}

#[test]
fn disabling_a_repo_blocks_writes_and_aborts_sessions() {
    let sb = Sandbox::new("repo-status");
    let store = sb.store();
    let (alice, repo_id, team) = repo_with_team(&sb, &store);
    let bob = member(&team, "bob");

    let pending = vec![vcs_server::PendingChange::file(
        "a.txt",
        vcs_core::ChangeKind::Add,
        Some(vcs_core::hash_bytes(b"a\n")),
        2,
    )];
    let commit = store
        .begin_commit(&bob, &repo_id, 0, &pending, "半截提交", 2)
        .unwrap();

    let disabled = store
        .set_repo_status(&alice, &repo_id, vcs_core::RepoStatus::Disabled)
        .unwrap();
    assert_eq!(disabled.status, vcs_core::RepoStatus::Disabled);

    // 在途会话作废 + 后续读写全被拒
    let err = store.finalize_commit(&bob, &commit.session_id).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidSession);
    let err = store
        .begin_commit(&bob, &repo_id, 1, &pending, "禁用后提交", 2)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoDisabled);
    let err = store.files_at(&bob, &repo_id, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoDisabled);

    // 非管理员不能改状态；恢复 active 后可继续用
    let err = store
        .set_repo_status(&bob, &repo_id, vcs_core::RepoStatus::Active)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    store
        .set_repo_status(&alice, &repo_id, vcs_core::RepoStatus::Active)
        .unwrap();
    store.files_at(&bob, &repo_id, None).unwrap();
}

// ---------------- 回收站：撤销清理 ----------------

#[test]
fn revert_purge_restores_content_that_still_exists() {
    let sb = Sandbox::new("purge-revert");
    let store = sb.store();
    let (alice, repo_id, oct_only, _) = seed_monthly_repo(&sb, &store);

    // 进入回收站（不走立即释放），内容还在盘上。
    let outcome = store
        .purge(
            &alice,
            &repo_id,
            &[String::from("202610")],
            false,
            "先放回收站",
        )
        .unwrap();
    let purged = store.list_purged_paths(&alice, &repo_id).unwrap();
    assert_eq!(purged.len(), 1);
    assert_eq!(purged[0].path, "202610");
    assert_eq!(purged[0].state, "pending");
    assert!(purged[0].grace_until.is_some());

    let new_rev = store
        .revert_purge(&alice, &repo_id, &purged[0].purge_id)
        .unwrap();
    assert!(new_rev > outcome.rev, "撤销本身也是一次提交");

    // HEAD 里又有了，内容与当初一致。
    let (_, _, files) = store.files_at(&alice, &repo_id, None).unwrap();
    let restored = files
        .iter()
        .find(|entry| entry.path == "202610/only-oct.txt")
        .expect("恢复后应回到 HEAD");
    assert_eq!(restored.hash, oct_only);
    let data = store
        .read_file(&alice, &repo_id, None, "202610/only-oct.txt")
        .unwrap();
    assert_eq!(data, b"oct only\n");

    // 标记变 reverted：GC 不再跳过这些路径，内容因此得以保留。
    let after = store.list_purged_paths(&alice, &repo_id).unwrap();
    assert_eq!(after[0].state, "reverted");
    let gc = store.run_gc(&alice, &repo_id).unwrap();
    assert_eq!(gc.reclaimed_bytes, 0);
    assert!(store.objects().exists(&oct_only));

    // 同一记录不能撤销两次。
    let err = store
        .revert_purge(&alice, &repo_id, &after[0].purge_id)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

#[test]
fn revert_purge_reports_content_purged_after_sweep() {
    let sb = Sandbox::new("purge-revert-gone");
    let store = sb.store();
    let (alice, repo_id, oct_only, _) = seed_monthly_repo(&sb, &store);

    // 立即释放：GC 已把独占内容物理删除。
    store
        .purge(
            &alice,
            &repo_id,
            &[String::from("202610")],
            true,
            "立即释放",
        )
        .unwrap();
    assert!(!store.objects().exists(&oct_only));
    let purged = store.list_purged_paths(&alice, &repo_id).unwrap();

    // 恢复必须诚实地失败，而不是造一个指向空内容的版本。
    let err = store
        .revert_purge(&alice, &repo_id, &purged[0].purge_id)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ContentPurged);
    // 标记没有被改成 reverted，用户还能看到这条记录。
    let after = store.list_purged_paths(&alice, &repo_id).unwrap();
    assert_eq!(after[0].state, "active");
}

#[test]
fn purged_paths_are_member_only() {
    let sb = Sandbox::new("purge-list-gate");
    let store = sb.store();
    let (alice, repo_id, _, _) = seed_monthly_repo(&sb, &store);
    store
        .create_user(&alice, "outsider", "路人", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("outsider", PASSWORD).unwrap();
    let outsider = store.authenticate(&token).unwrap();

    let err = store.list_purged_paths(&outsider, &repo_id).unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoNotMember);
    store.list_purged_paths(&alice, &repo_id).unwrap();
}

// ---------------- 保留策略 ----------------

/// 造三个按月归档的目录，每个一份独有内容。
fn seed_three_months(store: &ServerStore, sb: &Sandbox) -> (vcs_server::Session, String) {
    let alice = admin(store);
    let repo = store.create_repo_direct(&alice, "monthly", 0).unwrap();
    let src = sb.dir("monthly-src");
    for month in ["202609", "202610", "202611"] {
        write_file(&src, &format!("{month}/report.txt"), month.as_bytes());
    }
    store.import_dir(&alice, &repo.repo_id, &src, "r1").unwrap();
    (alice, repo.repo_id)
}

#[test]
fn retention_policy_keeps_only_recent_months() {
    let sb = Sandbox::new("retention");
    let store = sb.store();
    let (alice, repo_id) = seed_three_months(&store, &sb);

    let policy = store
        .upsert_retention_policy(
            &alice,
            &repo_id,
            vcs_server::RetentionPolicyInput {
                policy_id: None,
                path_pattern: r"^20\d{4}$".into(),
                keep_months: 2,
                max_bytes: 0,
                max_age_days: 0,
                enabled: true,
                dry_run: false,
            },
        )
        .unwrap();
    assert_eq!(policy.keep_months, 2);
    assert_eq!(
        store
            .list_retention_policies(&alice, &repo_id)
            .unwrap()
            .len(),
        1
    );

    // dry-run：报告候选但不产生提交。
    let preview = store
        .apply_retention(&alice, &repo_id, true, false)
        .unwrap();
    assert_eq!(preview.candidates, vec!["202609".to_string()]);
    assert!(preview.reclaimable_bytes > 0);
    assert_eq!(preview.new_rev, 0);
    let (_, rev, _) = store.files_at(&alice, &repo_id, None).unwrap();
    assert_eq!(rev, 1, "dry-run 不该改动任何数据");

    // 真正执行：只剩最近两个月。
    let applied = store
        .apply_retention(&alice, &repo_id, false, true)
        .unwrap();
    assert_eq!(applied.candidates, vec!["202609".to_string()]);
    assert!(applied.new_rev > 1);
    let (_, _, files) = store.files_at(&alice, &repo_id, None).unwrap();
    let dirs: std::collections::BTreeSet<&str> = files
        .iter()
        .map(|f| f.path.split('/').next().unwrap())
        .collect();
    assert_eq!(
        dirs,
        ["202610", "202611"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );

    // 再跑一次：已经满足策略，不再产生提交。
    let again = store
        .apply_retention(&alice, &repo_id, false, true)
        .unwrap();
    assert!(again.candidates.is_empty());
    assert_eq!(again.new_rev, 0);
}

#[test]
fn retention_policy_validates_pattern_and_requires_a_limit() {
    let sb = Sandbox::new("retention-validate");
    let store = sb.store();
    let (alice, repo_id) = seed_three_months(&store, &sb);

    let invalid_regex = store
        .upsert_retention_policy(
            &alice,
            &repo_id,
            vcs_server::RetentionPolicyInput {
                policy_id: None,
                path_pattern: "(unclosed".into(),
                keep_months: 1,
                max_bytes: 0,
                max_age_days: 0,
                enabled: true,
                dry_run: true,
            },
        )
        .unwrap_err();
    assert_eq!(invalid_regex.code, ErrorCode::InvalidArgument);

    let no_limit = store
        .upsert_retention_policy(
            &alice,
            &repo_id,
            vcs_server::RetentionPolicyInput {
                policy_id: None,
                path_pattern: "^20".into(),
                keep_months: 0,
                max_bytes: 0,
                max_age_days: 0,
                enabled: true,
                dry_run: true,
            },
        )
        .unwrap_err();
    assert_eq!(no_limit.code, ErrorCode::InvalidArgument);

    // 只有 owner / maintainer 能配策略。
    store
        .create_user(&alice, "carol2", "王佳", PASSWORD, Role::User)
        .unwrap();
    store
        .add_member(&alice, &repo_id, "carol2", RepoMemberRole::Writer)
        .unwrap();
    let (token, _) = store.login("carol2", PASSWORD).unwrap();
    let carol = store.authenticate(&token).unwrap();
    let err = store
        .upsert_retention_policy(
            &carol,
            &repo_id,
            vcs_server::RetentionPolicyInput {
                policy_id: None,
                path_pattern: "^20".into(),
                keep_months: 1,
                max_bytes: 0,
                max_age_days: 0,
                enabled: true,
                dry_run: true,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoMemberForbidden);
}

#[test]
fn retention_policy_dry_run_flag_blocks_execution() {
    let sb = Sandbox::new("retention-dry");
    let store = sb.store();
    let (alice, repo_id) = seed_three_months(&store, &sb);

    store
        .upsert_retention_policy(
            &alice,
            &repo_id,
            vcs_server::RetentionPolicyInput {
                policy_id: None,
                path_pattern: r"^20\d{4}$".into(),
                keep_months: 1,
                max_bytes: 0,
                max_age_days: 0,
                enabled: true,
                dry_run: true,
            },
        )
        .unwrap();

    // 策略自己标了 dry_run：不带 --dry-run 的执行也必须被拒，不能悄悄删数据。
    let err = store
        .apply_retention(&alice, &repo_id, false, true)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    // 显式 dry-run 仍可预览。
    let preview = store
        .apply_retention(&alice, &repo_id, true, false)
        .unwrap();
    assert_eq!(preview.candidates.len(), 2);
    assert_eq!(preview.new_rev, 0);

    // 删除策略后一切归零。
    let policies = store.list_retention_policies(&alice, &repo_id).unwrap();
    store
        .delete_retention_policy(&alice, &repo_id, &policies[0].policy_id)
        .unwrap();
    assert!(store
        .list_retention_policies(&alice, &repo_id)
        .unwrap()
        .is_empty());
    let none = store
        .apply_retention(&alice, &repo_id, false, true)
        .unwrap();
    assert!(none.candidates.is_empty());
}

// ---------------- 后台维护与宽限期 ----------------

#[test]
fn maintenance_promotes_expired_graces_and_reclaims() {
    let sb = Sandbox::new("maintenance");
    let store = sb.store();
    let (alice, repo_id, oct_only, _) = seed_monthly_repo(&sb, &store);

    // 宽限期设为 0：清理记录创建时就到期，下一次维护会把它转正并回收。
    store.set_repo_grace_days(&alice, &repo_id, 0).unwrap();
    store
        .purge(
            &alice,
            &repo_id,
            &[String::from("202610")],
            false,
            "宽限期为 0",
        )
        .unwrap();
    let before = store.list_purged_paths(&alice, &repo_id).unwrap();
    assert_eq!(before[0].state, "pending");
    assert!(store.objects().exists(&oct_only));

    let report = store.run_maintenance().unwrap();
    assert_eq!(report.promoted_purges, 1);
    assert!(report.swept_objects >= 1);
    assert!(report.reclaimed_bytes > 0);
    assert!(report.repo_count >= 1);

    let after = store.list_purged_paths(&alice, &repo_id).unwrap();
    assert_eq!(after[0].state, "active");
    assert!(!store.objects().exists(&oct_only), "维护后应真正释放");
}

#[test]
fn maintenance_does_not_touch_purges_still_in_grace() {
    let sb = Sandbox::new("maintenance-grace");
    let store = sb.store();
    let (alice, repo_id, oct_only, _) = seed_monthly_repo(&sb, &store);

    // 默认宽限期 7 天：维护不该把它转正，更不该回收内容。
    store
        .purge(
            &alice,
            &repo_id,
            &[String::from("202610")],
            false,
            "还在宽限期",
        )
        .unwrap();
    let report = store.run_maintenance().unwrap();
    assert_eq!(report.promoted_purges, 0);
    assert_eq!(report.reclaimed_bytes, 0);
    let after = store.list_purged_paths(&alice, &repo_id).unwrap();
    assert_eq!(after[0].state, "pending");
    assert!(store.objects().exists(&oct_only));
}

#[test]
fn grace_days_must_be_set_by_a_writer() {
    let sb = Sandbox::new("grace-role");
    let store = sb.store();
    let (alice, repo_id, _, _) = seed_monthly_repo(&sb, &store);
    store
        .create_user(&alice, "reader2", "只读", PASSWORD, Role::User)
        .unwrap();
    store
        .add_member(&alice, &repo_id, "reader2", RepoMemberRole::Reader)
        .unwrap();
    let (token, _) = store.login("reader2", PASSWORD).unwrap();
    let reader = store.authenticate(&token).unwrap();

    let err = store.set_repo_grace_days(&reader, &repo_id, 1).unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoMemberForbidden);
    store.set_repo_grace_days(&alice, &repo_id, 3).unwrap();
}

// ---------------- 系统身份：保留策略的自动执行 ----------------

#[test]
fn maintenance_executes_non_dry_run_retention() {
    let sb = Sandbox::new("maintenance-retention");
    let store = sb.store();
    let (alice, repo_id) = seed_three_months(&store, &sb);

    // 宽限期设 0：本轮 purge 产生的记录立刻可转正，同一轮 GC 就能回收。
    store.set_repo_grace_days(&alice, &repo_id, 0).unwrap();
    store
        .upsert_retention_policy(
            &alice,
            &repo_id,
            vcs_server::RetentionPolicyInput {
                policy_id: None,
                path_pattern: r"^20\d{4}$".into(),
                keep_months: 1,
                max_bytes: 0,
                max_age_days: 0,
                enabled: true,
                dry_run: false,
            },
        )
        .unwrap();

    let report = store.run_maintenance().unwrap();
    assert_eq!(report.retention_repos, 1, "应有一个仓库被策略清理");
    assert_eq!(report.retention_paths, 2, "保留最近 1 个，另外两个应被清理");
    assert_eq!(report.promoted_purges, 2, "两条清理记录应在同一轮转正");
    assert!(report.reclaimed_bytes > 0);

    let (_, _, files) = store.files_at(&alice, &repo_id, None).unwrap();
    let dirs: std::collections::BTreeSet<&str> = files
        .iter()
        .map(|f| f.path.split('/').next().unwrap())
        .collect();
    assert_eq!(
        dirs,
        ["202611"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );

    // 再跑一轮：策略已满足，不再产生提交。
    let again = store.run_maintenance().unwrap();
    assert_eq!(again.retention_paths, 0);
}

#[test]
fn maintenance_ignores_dry_run_policies() {
    let sb = Sandbox::new("maintenance-dry");
    let store = sb.store();
    let (alice, repo_id) = seed_three_months(&store, &sb);

    store
        .upsert_retention_policy(
            &alice,
            &repo_id,
            vcs_server::RetentionPolicyInput {
                policy_id: None,
                path_pattern: r"^20\d{4}$".into(),
                keep_months: 1,
                max_bytes: 0,
                max_age_days: 0,
                enabled: true,
                dry_run: true,
            },
        )
        .unwrap();

    let report = store.run_maintenance().unwrap();
    assert_eq!(report.retention_paths, 0, "仅预览的策略不该在后台被执行");

    let (_, rev, files) = store.files_at(&alice, &repo_id, None).unwrap();
    assert_eq!(rev, 1, "不该产生删除提交");
    assert_eq!(files.len(), 3);
}

#[test]
fn system_session_cannot_be_authenticated() {
    // 系统身份只由服务端内部构造：任何 token 都不可能换成它。
    let sb = Sandbox::new("system-actor");
    let store = sb.store();
    let alice = admin(&store);
    let (token, _) = store.login("alice", PASSWORD).unwrap();
    let session = store.authenticate(&token).unwrap();
    assert!(!session.is_system());
    assert!(vcs_server::Session::system().is_system());
    // 系统身份仍然要过仓库状态校验：禁用的仓库不该被自动清理。
    let repo = store.create_repo_direct(&alice, "frozen", 0).unwrap();
    store
        .set_repo_status(&alice, &repo.repo_id, vcs_core::RepoStatus::Disabled)
        .unwrap();
    let err = store
        .files_at(&vcs_server::Session::system(), &repo.repo_id, None)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RepoDisabled);
}

// ---------------- 首位用户引导（注册 / LDAP 共用名额） ----------------

fn dir_user(username: &str, groups: &[&str]) -> vcs_server::DirectoryUser {
    vcs_server::DirectoryUser {
        username: username.into(),
        display_name: format!("{username} 姓名"),
        dn: format!("uid={username},dc=example,dc=com"),
        groups: groups.iter().map(|s| (*s).to_string()).collect(),
    }
}

fn ldap_fixture() -> vcs_server::LdapSettings {
    vcs_server::LdapSettings {
        host: "ldap.example.com".into(),
        base_dn: "dc=example,dc=com".into(),
        bind_dn: "cn=readonly,dc=example,dc=com".into(),
        bind_password: "BindPass1!".into(),
        enabled: true,
        ..Default::default()
    }
}

#[test]
fn first_registration_is_automatically_an_admin() {
    let sb = Sandbox::new("first-register");
    let store = sb.store();

    let reg = store
        .register("alice", "张爱丽", PASSWORD, "我是第一个用户")
        .unwrap();
    assert!(matches!(
        reg.info.state,
        vcs_core::RegistrationStatus::Approved
    ));
    assert!(reg.first_user, "首个注册应自动批准");

    // 注册时设置的密码立刻可用，不必再走任何激活链接。
    let (token, user) = store.login("alice", PASSWORD).unwrap();
    assert_eq!(user.role, Role::Admin);
    let session = store.authenticate(&token).unwrap();
    assert_eq!(session.role, Role::Admin);

    // 自动批准过的申请不能被重复审批。
    let err = store
        .approve_registration(&session, &reg.info.request_id)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalNotFound);

    // 第二个人仍是普通待审批申请，绝不会自动拿到管理员。
    let second = store.register("bob", "李波", PASSWORD, "申请").unwrap();
    assert!(matches!(
        second.info.state,
        vcs_core::RegistrationStatus::Pending
    ));
    assert!(!second.first_user);
    assert_eq!(
        store.login("bob", PASSWORD).unwrap_err().code,
        ErrorCode::AccountPending
    );
}

#[test]
fn bootstrap_is_closed_once_the_first_user_can_log_in() {
    let sb = Sandbox::new("first-rescue");
    let store = sb.store();
    let reg = store.register("alice", "张爱丽", PASSWORD, "").unwrap();
    assert!(reg.first_user);

    // 首位注册者已经有自己的密码，本机 init-admin 不该再开一个后门。
    let err = store
        .bootstrap_admin("mallory", "攻击者", PASSWORD)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    // 也不能借同名重置把提权通道打开。
    let err = store
        .bootstrap_admin("alice", "张爱丽", "Other1!x")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

#[test]
fn first_ldap_login_is_admin_and_group_mapping_only_applies_to_new_users() {
    let sb = Sandbox::new("first-ldap");
    let store = sb.store();
    let mut cfg = ldap_fixture();
    // 全局身份只有两种，组映射能给的只有管理员。
    cfg.group_roles = vec![("cn=admins,dc=example,dc=com".into(), Role::Admin)];
    store.set_ldap_config(None, &cfg).unwrap();

    // 目录里第一个登录的人：无论属于哪个组，都是管理员。
    let (token, first) = store.login_ldap(&dir_user("alice", &[])).unwrap();
    assert_eq!(first.role, Role::Admin);
    assert_eq!(store.authenticate(&token).unwrap().role, Role::Admin);

    // 之后 JIT 建号按组映射给初始身份；没命中的一律普通用户。
    let (_, mapped) = store
        .login_ldap(&dir_user("bob", &["cn=admins,dc=example,dc=com"]))
        .unwrap();
    assert_eq!(mapped.role, Role::Admin);
    let (_, plain) = store.login_ldap(&dir_user("carol", &[])).unwrap();
    assert_eq!(plain.role, Role::User);

    // 已有账号的再次登录只刷新姓名 / DN：先建号后加组也不会被提权。
    let (_, again) = store
        .login_ldap(&dir_user("carol", &["cn=admins,dc=example,dc=com"]))
        .unwrap();
    assert_eq!(again.role, Role::User);

    // 目录账号不存本地口令：本地登录会明确提示走目录登录。
    let err = store.login("alice", PASSWORD).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidCredentials);
}

#[test]
fn registration_and_ldap_share_a_single_first_user_slot() {
    // 注册先占了名额：之后第一个 LDAP 用户必须按普通规则（普通用户）建号。
    let sb = Sandbox::new("first-slot-register");
    let store = sb.store();
    store.register("alice", "张爱丽", PASSWORD, "").unwrap();
    let (_, via_ldap) = store.login_ldap(&dir_user("bob", &[])).unwrap();
    assert_eq!(via_ldap.role, Role::User);

    // 反过来：LDAP 先登录成管理员，之后注册的人只能等审批。
    let sb = Sandbox::new("first-slot-ldap");
    let store = sb.store();
    let (_, admin) = store.login_ldap(&dir_user("alice", &[])).unwrap();
    assert_eq!(admin.role, Role::Admin);
    let reg = store.register("bob", "李波", PASSWORD, "申请").unwrap();
    assert!(!reg.first_user);
    assert!(matches!(
        reg.info.state,
        vcs_core::RegistrationStatus::Pending
    ));
}

#[test]
fn ldap_bind_password_never_touches_disk_or_the_public_config_in_cleartext() {
    let sb = Sandbox::new("ldap-secret");
    let store = sb.store();
    let cfg = ldap_fixture();
    store.set_ldap_config(None, &cfg).unwrap();

    let db = fs::read(sb.root.join("store/server.db")).unwrap();
    let needle = cfg.bind_password.as_bytes();
    assert!(
        !db.windows(needle.len()).any(|w| w == needle),
        "server.db 里出现了明文 bind 密码"
    );

    let public = store.ldap_config_public(None).unwrap().unwrap();
    assert!(public.settings.bind_password.is_empty());
    assert!(public.bind_password_set);
    assert_eq!(public.settings.host, cfg.host);
    assert_eq!(
        store.ldap_config().unwrap().unwrap().bind_password,
        cfg.bind_password,
        "内部读取应能解密还原"
    );
}

#[test]
fn ldap_config_requires_admin_when_called_over_the_network_path() {
    let sb = Sandbox::new("ldap-authz");
    let store = sb.store();
    let alice = admin(&store);
    store
        .create_user(&alice, "dave", "赵大", PASSWORD, Role::User)
        .unwrap();
    let (token, _) = store.login("dave", PASSWORD).unwrap();
    let dave = store.authenticate(&token).unwrap();

    let cfg = ldap_fixture();
    assert_eq!(
        store.set_ldap_config(Some(&dave), &cfg).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        store.ldap_config_public(Some(&dave)).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    // 本机运维通道（actor=None）不受影响。
    store.set_ldap_config(None, &cfg).unwrap();
    assert!(store.ldap_config_public(Some(&alice)).unwrap().is_some());
}
