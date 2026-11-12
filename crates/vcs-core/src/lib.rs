//! svn-next 共享版本控制核心。
//!
//! M0 只定义领域模型与契约（零外部依赖，可离线 `cargo check`）；
//! M1 起接入 blake3 / zstd / rusqlite / getrandom，落地了五块可运行能力：
//!
//!   - [`hash`]：blake3 地址与 `objects/ab/cdef…` 分片路径；
//!   - [`tree`]：树对象的规范编码、自底向上组装、扁平化；
//!   - [`store`]：本地文件系统 CAS（tmp → 原子 rename、zstd、幂等写入）；
//!   - [`auth`]：Argon2id 口令、HS256 JWT、PAT 令牌；
//!   - [`wc`]：工作副本（检出、离线 status / diff、pristine 基线缓存）。
//!
//! 客户端经 napi-rs 复用本 crate，服务端 `vcs-server` 也直接依赖它，
//! 从根上避免「同一份语义两处实现、两处漂移」。
//!
//! 已确认的核心约束（见 docs/design.md 第 15 章）：
//!   - 服务端内容存储固定为本地文件系统 CAS，不引入 S3 / 外部对象存储；
//!   - 提交严格 out-of-date 拒绝，不做服务端自动合并；
//!   - 客户端绑定方式为 napi-rs 原生模块，不采用 sidecar；
//!   - 无分支与标签：单一线性主干，版本定位只用全局 rN。
//!   - 用户体系：LDAP 登录 + 本地自助注册（需管理员审批），不依赖外部 IdP。
//!   - 存储布局统一为共享 CAS（SHARED_CAS），不提供 PARTITIONED 分区隔离。
//!   - 仓库由用户申请创建，管理员审批通过后才分配 repo_id 并可用。
//!   - 用户字段仅用户名 / 密码 / 姓名；本地密码 >= 8 位且同时含数字、大写、小写、特殊字符。
//!   - 仓库成员资格是使用仓库的第一道门；owner 自行管理成员，非成员无权限。
//!   - 配额两级：全局默认 + 单仓库覆盖，未配置时内置兜底，BeginCommit 用生效值预检。

use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

pub mod auth;
pub mod hash;
pub mod store;
pub mod tree;
pub mod wc;

pub use hash::{from_hex, hash_bytes, object_rel_path, to_hex};
pub use store::FileObjectStore;
pub use tree::{
    all_tree_hashes, apply_changes, apply_changes_tracked, build_tree, decode_tree, encode_tree,
    flatten_tree, read_tree, write_tree, FlatEntry, TreeOp,
};
pub use wc::{
    ConflictChoice, ConflictRecord, LocalChange, RemoteChange, StatusEntry, UpdateOutcome, WcState,
    WorkingCopy,
};

pub const HASH_LEN: usize = 32;

/// blake3 摘要，作为 CAS 中对象的唯一地址。
pub type Hash = [u8; HASH_LEN];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Blob,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
    Rename,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    #[default]
    Infinity,
    Immediates,
    Files,
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    pub hash: Hash,
    pub kind: ObjectKind,
    pub size: u64,
    pub stored_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub hash: Hash,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: String,
    pub old_hash: Option<Hash>,
    pub new_hash: Option<Hash>,
    pub from_path: Option<String>,
}

/// 每个 revision 的 root_tree 是完整快照，不是增量 delta 链。
/// 无分支与标签：rev 是线性主干上唯一的版本定位方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub rev: u64,
    pub root_tree: Hash,
    pub parent_rev: u64,
    pub author: String,
    pub message: String,
    pub committed_at: i64,
}

// ---------------- 容量治理 ----------------

/// 全仓库唯一的对象存储布局：单一共享 CAS，所有目录与月份共用同一对象池。
/// 不提供 PARTITIONED 分区隔离；释放空间一律走 purged_paths 路径裁剪 + GC。
pub const CAS_LAYOUT: &str = "SHARED_CAS";

/// 两级配额都未配置时的内置兜底，保证任何情况下都存在一条上限。
///
/// 配额只有一条线（超过即拒绝写入），不再区分软 / 硬阈值：
/// 软阈值从未参与过任何判定，多一个输入框只会让审批人猜。
pub const FALLBACK_QUOTA_BYTES: u64 = 100 * 1024 * 1024 * 1024; // 100 GiB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSource {
    RepoOverride,
    GlobalDefault,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveQuota {
    /// 生效的配额上限（字节）。
    pub bytes: u64,
    pub source: QuotaSource,
}

/// 解析生效配额：单仓库显式值 > 全局默认 > 内置兜底。
/// `repo_bytes` / `default_bytes` 为 0 表示该级未配置。
pub fn resolve_effective_quota(repo_bytes: u64, default_bytes: u64) -> EffectiveQuota {
    if repo_bytes > 0 {
        return EffectiveQuota {
            bytes: repo_bytes,
            source: QuotaSource::RepoOverride,
        };
    }
    if default_bytes > 0 {
        return EffectiveQuota {
            bytes: default_bytes,
            source: QuotaSource::GlobalDefault,
        };
    }
    EffectiveQuota {
        bytes: FALLBACK_QUOTA_BYTES,
        source: QuotaSource::Fallback,
    }
}

/// dry-run 结果：reclaimable_bytes 才是删除后真正能释放的空间，
/// 与目录的逻辑大小不等价（内容可能被其它目录共享）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeEstimate {
    pub reclaimable_bytes: u64,
    pub affected_revisions: u64,
    pub shared_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeRequest {
    pub paths: Vec<String>,
    /// 跳过回收站宽限期，立即触发 GC（配额不足场景默认开启）。
    pub immediate: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcState {
    Queued,
    Marking,
    Sweeping,
    Done,
    Failed,
}

// ---------------- 提交策略 ----------------

/// 已确认：严格 out-of-date 拒绝。
/// FinalizeCommit 时若 head != base_rev 直接返回 OutOfDate，
/// 用户先 update 再提交；服务端不自动合并非重叠路径。
/// 单变体枚举用于显式锁定该约束，避免实现阶段出现第二种策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitPolicy {
    #[default]
    StrictOutOfDate,
}

impl CommitPolicy {
    /// 是否应因版本落后直接拒绝本次提交。
    pub fn rejects_out_of_date(&self, base_rev: u64, head_rev: u64) -> bool {
        match self {
            Self::StrictOutOfDate => base_rev != head_rev,
        }
    }
}

// ---------------- 密码策略 ----------------

/// 本地账号最小密码长度（按 Unicode 标量值计）。
pub const MIN_PASSWORD_LEN: usize = 8;

/// 特殊字符 = 可打印 ASCII 标点；空格、字母、数字与非 ASCII 字符均不计入。
pub fn is_special_char(c: char) -> bool {
    c.is_ascii_punctuation()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordPolicyError {
    TooShort { min: usize, actual: usize },
    MissingDigit,
    MissingUppercase,
    MissingLowercase,
    MissingSpecial,
}

impl fmt::Display for PasswordPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { min, actual } => {
                write!(f, "长度不足（当前 {actual} 位，至少需要 {min} 位）")
            }
            Self::MissingDigit => write!(f, "缺少数字"),
            Self::MissingUppercase => write!(f, "缺少大写字母"),
            Self::MissingLowercase => write!(f, "缺少小写字母"),
            Self::MissingSpecial => write!(f, "缺少特殊字符（ASCII 标点，如 ! @ # $ %）"),
        }
    }
}

/// 返回全部未满足项（空向量表示通过）。客户端与服务端共用同一份校验逻辑。
pub fn validate_password(password: &str) -> Vec<PasswordPolicyError> {
    let mut issues = Vec::new();
    let actual = password.chars().count();
    if actual < MIN_PASSWORD_LEN {
        issues.push(PasswordPolicyError::TooShort {
            min: MIN_PASSWORD_LEN,
            actual,
        });
    }
    if !password.chars().any(|c| c.is_ascii_digit()) {
        issues.push(PasswordPolicyError::MissingDigit);
    }
    if !password.chars().any(|c| c.is_ascii_uppercase()) {
        issues.push(PasswordPolicyError::MissingUppercase);
    }
    if !password.chars().any(|c| c.is_ascii_lowercase()) {
        issues.push(PasswordPolicyError::MissingLowercase);
    }
    if !password.chars().any(is_special_char) {
        issues.push(PasswordPolicyError::MissingSpecial);
    }
    issues
}

pub fn password_is_valid(password: &str) -> bool {
    validate_password(password).is_empty()
}

// ---------------- 用户体系 ----------------

/// 身份来源：LDAP 用户不存密码，本地账号走自助注册 + 管理员审批。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    Local,
    Ldap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Pending,
    Active,
    Disabled,
    Rejected,
    Expired,
}

impl AccountStatus {
    /// 仅 ACTIVE 账号可登录并签发 token。
    pub fn may_login(&self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Readonly,
    Writer,
    Admin,
}

impl Role {
    /// 注册审批与用户管理接口仅限管理员。
    pub fn can_approve_registrations(&self) -> bool {
        matches!(self, Self::Admin)
    }
}

// ---------------- 仓库生命周期 ----------------

/// 仓库一旦存在即为 ACTIVE；PENDING 的是创建申请，不是仓库实体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoStatus {
    #[default]
    Active,
    Disabled,
    Archived,
}

impl RepoStatus {
    /// 只有 ACTIVE 仓库允许执行 RepoService 的任何操作。
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoCreationStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl RepoCreationStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Approved | Self::Rejected | Self::Expired)
    }
}

// ---------------- 成员角色 ----------------

/// 仓库成员角色；非成员没有任何仓库使用权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoMemberRole {
    Owner,
    Maintainer,
    #[default]
    Writer,
    Reader,
}

impl RepoMemberRole {
    pub fn can_manage_members(&self) -> bool {
        matches!(self, Self::Owner | Self::Maintainer)
    }

    pub fn can_write(&self) -> bool {
        matches!(self, Self::Owner | Self::Maintainer | Self::Writer)
    }

    /// owner 唯一且受保护：不可移除、不可降级。
    pub fn is_protected(&self) -> bool {
        matches!(self, Self::Owner)
    }

    /// owner 可指定除 owner 外的角色；maintainer 只能增删 reader / writer。
    pub fn can_assign(&self, target: RepoMemberRole) -> bool {
        match self {
            Self::Owner => !target.is_protected(),
            Self::Maintainer => matches!(target, Self::Writer | Self::Reader),
            _ => false,
        }
    }
}

// ---------------- 错误 ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Ok,
    OutOfDate,
    PathConflict,
    TreeConflict,
    PermissionDenied,
    QuotaExceeded,
    ContentPurged,
    Locked,
    ObjectMissing,
    InvalidSession,
    Unauthenticated,
    InvalidCredentials,
    AccountPending,
    AccountDisabled,
    RegistrationExists,
    ApprovalNotFound,
    SetupTokenInvalid,
    LdapUnavailable,
    TokenExpired,
    PatRevoked,
    RepoNotFound,
    RepoDisabled,
    RepoArchived,
    RepoNameInvalid,
    RepoCreationExists,
    RepoApprovalNotFound,
    PasswordPolicyViolation,
    RepoNotMember,
    RepoMemberExists,
    RepoMemberNotFound,
    RepoOwnerProtected,
    RepoMemberForbidden,
    UserNotFound,
    QuotaConfigInvalid,
    /// 本地文件系统 / 网络 IO 失败；服务端内部错误统一落在这里，细节走 message。
    IoError,
    /// 入参不合法（必填项为空、枚举值越界等）。刻意与 `InvalidSession` 区分：
    /// 后者会让客户端以为登录态失效并跳去重新登录，而这里只需要用户改输入。
    InvalidArgument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------- 对象存储契约 ----------------

/// 生产实现为本地文件系统 CAS；内存实现仅用于测试。
/// 保留 trait 抽象是为了让 GC 与业务层不依赖具体目录布局，
/// 而不是为了切换到远端对象存储。
pub trait ObjectStore: Send + Sync {
    fn get(&self, hash: &Hash) -> Result<Vec<u8>>;
    fn put(&self, hash: &Hash, data: &[u8]) -> Result<()>;
    fn exists(&self, hash: &Hash) -> bool;
    fn remove(&self, hash: &Hash) -> Result<()>;
    fn stored_size(&self, hash: &Hash) -> Result<u64>;
}

#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    inner: RwLock<HashMap<Hash, Vec<u8>>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ObjectStore for MemoryObjectStore {
    fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        self.inner
            .read()
            .expect("lock poisoned")
            .get(hash)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::ObjectMissing, "对象不存在"))
    }

    fn put(&self, hash: &Hash, data: &[u8]) -> Result<()> {
        self.inner
            .write()
            .expect("lock poisoned")
            .insert(*hash, data.to_vec());
        Ok(())
    }

    fn exists(&self, hash: &Hash) -> bool {
        self.inner.read().expect("lock poisoned").contains_key(hash)
    }

    fn remove(&self, hash: &Hash) -> Result<()> {
        self.inner
            .write()
            .expect("lock poisoned")
            .remove(hash)
            .map(|_| ())
            .ok_or_else(|| Error::new(ErrorCode::ObjectMissing, "对象不存在"))
    }

    fn stored_size(&self, hash: &Hash) -> Result<u64> {
        Ok(self.get(hash)?.len() as u64)
    }
}

/// 释放量预估契约：M2 由服务端可达性遍历实现。
pub trait PurgePlanner: Send + Sync {
    fn estimate(&self, paths: &[String]) -> Result<PurgeEstimate>;
}

// ---------------- 路径安全 ----------------

/// 统一路径规范化，防目录穿越；Windows 反斜杠一并处理。
pub fn normalize_path(input: &str) -> Result<String> {
    let unified = input.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::new(
                        ErrorCode::PathConflict,
                        "路径越出工作副本根目录",
                    ));
                }
            }
            other => parts.push(other),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(
            normalize_path("docs/../proto/repo.proto").unwrap(),
            "proto/repo.proto"
        );
        assert_eq!(normalize_path("a\\b\\.\\c").unwrap(), "a/b/c");
        assert_eq!(normalize_path("/202610/").unwrap(), "202610");
    }

    #[test]
    fn rejects_traversal() {
        assert_eq!(
            normalize_path("../outside").unwrap_err().code,
            ErrorCode::PathConflict
        );
    }

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryObjectStore::new();
        let hash = [7u8; HASH_LEN];
        assert!(!store.exists(&hash));
        store.put(&hash, b"content").unwrap();
        assert_eq!(store.get(&hash).unwrap(), b"content");
        assert_eq!(store.stored_size(&hash).unwrap(), 7);
        store.remove(&hash).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn single_shared_cas_layout() {
        assert_eq!(CAS_LAYOUT, "SHARED_CAS");
    }

    #[test]
    fn strict_out_of_date_policy() {
        let policy = CommitPolicy::default();
        assert_eq!(policy, CommitPolicy::StrictOutOfDate);
        assert!(policy.rejects_out_of_date(5, 6));
        assert!(!policy.rejects_out_of_date(6, 6));
    }

    #[test]
    fn only_active_accounts_may_login() {
        assert!(AccountStatus::Active.may_login());
        assert!(!AccountStatus::Pending.may_login());
        assert!(!AccountStatus::Disabled.may_login());
        assert!(!AccountStatus::Rejected.may_login());
        assert!(Role::Admin.can_approve_registrations());
        assert!(!Role::default().can_approve_registrations());
    }

    #[test]
    fn only_approved_active_repos_are_usable() {
        assert!(RepoStatus::Active.is_usable());
        assert!(!RepoStatus::Disabled.is_usable());
        assert!(!RepoStatus::Archived.is_usable());
        assert!(!RepoCreationStatus::Pending.is_terminal());
        assert!(RepoCreationStatus::Approved.is_terminal());
        assert!(RepoCreationStatus::Rejected.is_terminal());
    }

    #[test]
    fn password_policy_requires_all_four_classes() {
        assert!(password_is_valid("Abcdef1!"));
        assert!(password_is_valid("Aa1!aaaa"));
        assert!(!password_is_valid("Abcdef12")); // 缺特殊字符
        assert!(!password_is_valid("abcdef1!")); // 缺大写
        assert!(!password_is_valid("ABCDEF1!")); // 缺小写
        assert!(!password_is_valid("Abcdefg!")); // 缺数字
        assert!(!password_is_valid("Aa1!")); // 长度不足
        assert_eq!(
            validate_password("Aa1!"),
            vec![PasswordPolicyError::TooShort { min: 8, actual: 4 }]
        );
    }

    #[test]
    fn special_chars_are_ascii_punctuation_only() {
        assert!(is_special_char('!'));
        assert!(is_special_char('@'));
        assert!(is_special_char('_'));
        assert!(!is_special_char(' '));
        assert!(!is_special_char('A'));
        assert!(!is_special_char('7'));
        assert!(!is_special_char('中'));
    }

    #[test]
    fn member_roles_gate_repo_usage() {
        assert!(!RepoMemberRole::Reader.can_write());
        assert!(RepoMemberRole::Writer.can_write());
        assert!(RepoMemberRole::Owner.can_manage_members());
        assert!(RepoMemberRole::Maintainer.can_manage_members());
        assert!(!RepoMemberRole::Writer.can_manage_members());
        assert!(RepoMemberRole::Owner.is_protected());
        assert!(!RepoMemberRole::Owner.can_assign(RepoMemberRole::Owner));
        assert!(RepoMemberRole::Maintainer.can_assign(RepoMemberRole::Writer));
        assert!(!RepoMemberRole::Maintainer.can_assign(RepoMemberRole::Maintainer));
    }

    #[test]
    fn quota_resolution_precedence() {
        let fallback = resolve_effective_quota(0, 0);
        assert_eq!(fallback.source, QuotaSource::Fallback);
        assert_eq!(fallback.bytes, FALLBACK_QUOTA_BYTES);

        let global = resolve_effective_quota(0, 200);
        assert_eq!(global.source, QuotaSource::GlobalDefault);
        assert_eq!(global.bytes, 200);

        // 单仓库显式值优先于全局默认，且不做任何比例推导。
        let repo = resolve_effective_quota(300, 200);
        assert_eq!(repo.source, QuotaSource::RepoOverride);
        assert_eq!(repo.bytes, 300);
    }
}
