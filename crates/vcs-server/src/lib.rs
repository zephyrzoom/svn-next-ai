//! 服务端存储层（M1）。
//!
//! 目录布局：
//!
//! ```text
//! <root>/server.db        元数据（schema/server.sql）
//! <root>/objects/         共享 CAS，所有仓库共用同一个对象池
//! <root>/tmp/             CAS 原子写入暂存区
//! <root>/server.secret    JWT 签名密钥（首次启动生成，权限 0600）
//! ```
//!
//! M1 只实现「本地直连」形态：CLI 直接打开这个目录，验证版本语义是否正确。
//! M3 起同一套 `ServerStore` 被 gRPC 服务包装，业务代码不动。
//!
//! 已经落到这一层的约束：
//!   - 单一共享 CAS，跨仓库/跨路径全局去重；
//!   - revision 元数据永久保留，内容字节才可能被裁剪；
//!   - 成员资格是使用仓库的第一道门，`reader` 不能提交；
//!   - 配额只有一条上限，解析复用 `vcs-core::resolve_effective_quota`，不重复实现；
//!   - 每次提交用同一事务分配 rev，`head_rev` 乐观并发。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use vcs_core::auth::{self, Claims, TokenSigner};
use vcs_core::hash::hash_bytes;
use vcs_core::tree::{self, FlatEntry};
use vcs_core::{
    self as core_types, to_hex, ChangeKind, EffectiveQuota, EntryKind, Error, ErrorCode,
    FileObjectStore, Hash, ObjectStore, RepoMemberRole, RepoStatus, Result, Role,
};

mod admin;
pub use admin::{AdminOverview, AdminUser};
mod disk;
pub use disk::{disk_capacity, StorageCapacity};
mod ldap;
pub use ldap::{
    authenticate_ldap, test_ldap_connection, DirectoryUser, LdapPublicConfig, LdapSettings,
    LdapTestOutcome, TlsMode,
};

const SERVER_SCHEMA: &str = include_str!("../../../schema/server.sql");

// ---------------- 对外数据结构 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub status: core_types::AccountStatus,
}

/// 会话的发起者类型。
///
/// `System` 只由服务端内部构造（定时任务等无人触发的操作），**永远不会**从
/// `authenticate` 产生——RPC 层拿不到系统身份，这一点由构造函数收口保证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    User,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user_id: String,
    pub username: String,
    /// 姓名（纯展示）。JWT / PAT 校验时与用户名一起从 users 表读出，
    /// `WhoAmI` 才有东西可返回——否则免登录恢复后的界面只能退回显示账号。
    pub display_name: String,
    pub role: Role,
    pub kind: ActorKind,
    /// PAT 的能力范围；JWT 会话为 `None`，表示「本人全部的权限」。
    ///
    /// 不把 scope 折进 `role` 是刻意的：role 决定「在仓库里能做什么」，
    /// scopes 决定「这个令牌能做什么」。CI 里的只读令牌应该能拉取代码，
    /// 但即便持有人是 owner，也不能拿它去删成员或清理空间。
    pub scopes: Option<Vec<String>>,
}

impl Session {
    /// 系统身份：用于定时维护等无人触发的操作。
    ///
    /// 它不是「超级管理员」——只跳过**成员资格与角色**这两道门，仓库状态、配额、
    /// out-of-date 等业务校验一律照旧。所有放行点都在代码里逐个标注，便于审计。
    pub fn system() -> Self {
        Self {
            user_id: "system".to_string(),
            username: "system".to_string(),
            display_name: "系统".to_string(),
            role: Role::Admin,
            kind: ActorKind::System,
            scopes: None,
        }
    }

    pub fn is_system(&self) -> bool {
        self.kind == ActorKind::System
    }

    /// 是否持有某项能力。JWT 会话无 scope 限制，一律放行。
    pub fn permits(&self, scope: &str) -> bool {
        match &self.scopes {
            None => true,
            Some(list) => list.iter().any(|s| s == scope),
        }
    }

    fn require(&self, scope: &str, action: &str) -> Result<()> {
        if self.permits(scope) {
            return Ok(());
        }
        Err(Error::new(
            ErrorCode::PermissionDenied,
            format!("当前访问令牌没有 {scope} 权限，不能{action}"),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub repo_id: String,
    pub name: String,
    pub head_rev: u64,
    pub status: RepoStatus,
    pub owner_user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionInfo {
    pub rev: u64,
    pub root_tree: Hash,
    pub parent_rev: u64,
    pub author: String,
    pub message: String,
    pub committed_at: i64,
    pub change_count: u32,
    pub changes: Vec<ChangeRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRecord {
    pub path: String,
    pub kind: ChangeKind,
    pub old_hash: Option<Hash>,
    pub new_hash: Option<Hash>,
}

/// 一条待提交的变更。`hash` 为 `None` 表示删除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    pub path: String,
    pub kind: ChangeKind,
    /// 条目类型。符号链接必须原样带过去，否则会话提交后会退化成普通文件。
    pub entry_kind: EntryKind,
    pub hash: Option<Hash>,
    pub size: u64,
}

impl PendingChange {
    /// 普通文件的变更，覆盖绝大多数场景。
    pub fn file(path: impl Into<String>, kind: ChangeKind, hash: Option<Hash>, size: u64) -> Self {
        Self {
            path: path.into(),
            kind,
            entry_kind: EntryKind::File,
            hash,
            size,
        }
    }
}

/// 已开启的提交会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSession {
    pub session_id: String,
    pub repo_id: String,
    pub base_rev: u64,
    pub message: String,
    pub change_count: usize,
}

/// 会话表里保存的变更集（哈希以十六进制落库，便于排障时直接看）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredChange {
    path: String,
    kind: String,
    entry_kind: String,
    new_hash: Option<String>,
    size: u64,
}

struct SessionRow {
    repo_id: String,
    client_id: String,
    base_rev: u64,
    message: String,
    state: String,
    expires_at: i64,
}

/// 从会话变更集还原出来的变更，带上重新解析出的 entry 类型。
struct SessionChange {
    path: String,
    kind: ChangeKind,
    kind_entry: EntryKind,
    hash: Option<Hash>,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub role: RepoMemberRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    pub rev: u64,
    /// true 表示目录内容与上一版完全一致，未分配新版本号。
    pub no_changes: bool,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub new_object_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageInfo {
    pub logical_bytes: u64,
    pub used_bytes: u64,
    pub object_count: u64,
    pub quota: EffectiveQuota,
    pub file_count: u64,
}

/// 清理预估（dry-run）。与 `EstimatePurgeResponse` 一一对应。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeEstimate {
    pub paths: Vec<String>,
    /// 这些路径在 HEAD 的逻辑大小（含被别处共享的对象）。
    pub logical_bytes: u64,
    /// 真正能回收的字节数：只被这些路径独占的对象。
    pub reclaimable_bytes: u64,
    /// 被其它路径 / 其它仓库共享、因此不会被回收的字节数。
    pub shared_bytes: u64,
    /// 历史版本里出现过这些路径的版本数量。
    pub affected_revisions: u64,
    /// 共享引用落到了哪些路径上，供客户端解释「为什么释放不了那么多」。
    pub shared_paths: Vec<String>,
}

/// 一次清理的结果：既产生了删除提交，也排上了 GC。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeOutcome {
    pub purge_id: String,
    pub rev: u64,
    pub gc_job_id: String,
    pub deleted: usize,
    pub estimate: PurgeEstimate,
}

/// 一次后台维护的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// 本次从宽限期转正的清理记录数。
    pub promoted_purges: usize,
    pub gc_job_id: String,
    pub swept_objects: u64,
    pub reclaimed_bytes: u64,
    /// 参与本轮 GC 的仓库数（共享 CAS 下 GC 是全局的，这里只是信息）。
    pub repo_count: usize,
    /// 本轮因保留策略执行了清理的仓库数。
    pub retention_repos: usize,
    /// 本轮被保留策略清理掉的目录数。
    pub retention_paths: usize,
}

/// 一条保留策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicyInfo {
    pub policy_id: String,
    pub path_pattern: String,
    pub keep_months: u32,
    pub max_bytes: u64,
    pub max_age_days: u32,
    pub enabled: bool,
    pub dry_run: bool,
    pub created_at: i64,
}

/// 新建 / 更新保留策略的入参。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicyInput {
    /// 空表示新建。
    pub policy_id: Option<String>,
    pub path_pattern: String,
    pub keep_months: u32,
    pub max_bytes: u64,
    pub max_age_days: u32,
    pub enabled: bool,
    pub dry_run: bool,
}

/// 一次保留策略评估的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionApplyOutcome {
    pub candidates: Vec<String>,
    pub reclaimable_bytes: u64,
    /// 0 表示没有产生提交（dry-run，或本来就没有候选）。
    pub new_rev: u64,
}

/// 一条清理记录（回收站条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgedPathInfo {
    pub purge_id: String,
    pub path: String,
    /// `pending`（宽限期内）/ `active`（GC 会跳过它）/ `reverted`（已撤销）
    pub state: String,
    pub reason: Option<String>,
    pub purged_by: String,
    pub created_at: i64,
    pub grace_until: Option<i64>,
    pub activated_at: Option<i64>,
}

/// 一个可清理目录的用量画像。与 `PurgeCandidate` 对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeCandidate {
    pub path: String,
    pub logical_bytes: u64,
    pub unique_bytes: u64,
    pub first_seen_rev: u64,
    pub last_modified_rev: u64,
}

/// 一条自助注册申请。与 `RegistrationInfo` 对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationInfo {
    pub request_id: String,
    pub username: String,
    pub display_name: String,
    pub reason: Option<String>,
    pub state: core_types::RegistrationStatus,
    pub submitted_at: i64,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub expires_at: i64,
}

/// 审批通过的结果。
///
/// 审批只做「激活」：密码在提交申请时就已经由本人设好并落库，
/// 不再发放任何一次性凭证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedRegistration {
    pub user_id: String,
}

/// 自助注册的结果。
///
/// `first_user` 为 true 表示**这是全库第一个账号**：服务端在同一事务里把它
/// 自动批准为管理员，注册时设置的密码立即可用，不必等管理员审批。
/// 后续注册仍是普通的 PENDING 申请。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterOutcome {
    pub info: RegistrationInfo,
    pub first_user: bool,
}

/// 一条仓库创建申请。与 `RepoCreationInfo` 对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRequestInfo {
    pub request_id: String,
    pub requester_user_id: String,
    pub repo_name: String,
    pub reason: Option<String>,
    pub requested_quota_bytes: u64,
    pub state: core_types::RepoCreationStatus,
    pub submitted_at: i64,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub expires_at: i64,
    pub approved_repo_id: Option<String>,
}

/// 一条仓库扩容申请。
///
/// 申请人只提交「我想加空间」，容量由服务端按固定步长算出：
/// `target_bytes = quota_before_bytes + increment_bytes`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoQuotaRequestInfo {
    pub request_id: String,
    pub repo_id: String,
    pub repo_name: String,
    pub requester_user_id: String,
    /// 本次申请的增量（固定 `vcs_core::REPO_QUOTA_EXPANSION_STEP_BYTES`）。
    pub increment_bytes: u64,
    /// 提交时的生效上限，审批台用来展示「从多少加到多少」。
    pub quota_before_bytes: u64,
    pub reason: Option<String>,
    pub state: core_types::RepoCreationStatus,
    pub submitted_at: i64,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub expires_at: i64,
}

impl RepoQuotaRequestInfo {
    /// 审批通过后的目标上限。审批台与 CLI 都按这个口径展示。
    pub fn target_bytes(&self) -> u64 {
        self.quota_before_bytes.saturating_add(self.increment_bytes)
    }
}

/// 用户搜索结果。刻意只有三项字段：认人够用，不多给。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSummary {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
}

/// 一个 PAT 的元信息（不含明文，明文在创建时只返回一次）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatInfo {
    pub token_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

/// GC 任务状态。与 `GetGcStatusResponse` 对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcJobStatus {
    pub job_id: String,
    pub state: String,
    pub progress_percent: u32,
    pub marked_objects: u64,
    pub swept_objects: u64,
    pub reclaimed_bytes: u64,
    pub error: Option<String>,
}

// ---------------- 存储 ----------------

pub struct ServerStore {
    root: PathBuf,
    conn: Connection,
    objects: FileObjectStore,
    signer: TokenSigner,
    /// 与 JWT 同源的主机密钥，另用于加密 LDAP bind 密码（域分隔见 auth::seal_secret）。
    secret: Vec<u8>,
}

impl ServerStore {
    /// 打开（或初始化）一个服务端存储目录。
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|e| io("创建服务端目录失败", &root, e))?;
        let objects = FileObjectStore::open(root.join("objects"))?;
        objects.sweep_tmp()?;

        let db_path = root.join("server.db");
        let conn =
            Connection::open(&db_path).map_err(|e| io("打开 server.db 失败", &db_path, e))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| io("启用 WAL 失败", &db_path, e))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| io("启用外键失败", &db_path, e))?;
        conn.execute_batch(SERVER_SCHEMA)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("初始化 server.db 失败：{e}")))?;
        migrate_single_quota(&conn)?;
        migrate_user_roles(&conn)?;

        let secret = load_or_create_secret(&root)?;
        let signer = TokenSigner::new(secret.clone());
        Ok(Self {
            root,
            conn,
            objects,
            signer,
            secret,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn objects(&self) -> &FileObjectStore {
        &self.objects
    }

    // ---------------- 用户与认证 ----------------

    /// 是否已经存在任何用户。bootstrap 只允许在空库执行一次。
    pub fn has_any_user(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .map_err(db("统计用户失败"))?;
        Ok(count > 0)
    }

    /// 是否存在**真正能登录**的管理员。
    ///
    /// 只有 ACTIVE 且持有可用凭证的管理员才算数：旧流程留下的未设密码账号
    /// 登录不进去，运维仍可用 `init-admin` 在本机救援，不至于全库锁死。
    fn has_usable_admin(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM users
                 WHERE role = 'admin' AND status = 'active'
                   AND (source = 'ldap' OR password_hash IS NOT NULL)",
                [],
                |r| r.get(0),
            )
            .map_err(db("统计可用管理员失败"))?;
        Ok(count > 0)
    }

    /// 引导管理员（空库）或救援「有账号但没有可用口令」的首位用户。
    ///
    /// 只允许在「没有可用管理员」时执行：既保留原来「只能初始化一次」的语义，
    /// 又给旧流程遗留的未激活账号留一条本机恢复路径。CLI 走的是本地直连
    /// 存储，本来就是部署时的最高权限通道。
    pub fn bootstrap_admin(
        &self,
        username: &str,
        display_name: &str,
        password: &str,
    ) -> Result<UserInfo> {
        if self.has_usable_admin()? {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "已存在可登录的管理员，bootstrap 只用于首次初始化或首位账号救援",
            ));
        }
        validate_username(username)?;
        if display_name.trim().is_empty() {
            return Err(Error::new(ErrorCode::InvalidCredentials, "姓名不能为空"));
        }
        let issues = core_types::validate_password(password);
        if !issues.is_empty() {
            return Err(Error::new(
                ErrorCode::PasswordPolicyViolation,
                format!("密码不满足策略：{}", describe_password_issues(&issues)),
            ));
        }

        // 同名账号已存在：说明这是首位注册者 / 一次失败的引导，直接重置口令并提权。
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT user_id, source FROM users WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db("查询用户失败"))?;
        if let Some((user_id, source)) = existing {
            if source != "local" {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "该用户名属于目录账号，不能重置本地口令；请用 init-admin 另建管理员",
                ));
            }
            let hash = auth::hash_password(password)?;
            let now = now_secs();
            self.conn
                .execute(
                    "UPDATE users SET password_hash = ?2, display_name = ?3, status = 'active',
                                      role = 'admin', token_version = token_version + 1,
                                      approved_at = ?4
                     WHERE user_id = ?1",
                    params![user_id, hash, display_name.trim(), now],
                )
                .map_err(db("救援管理员账号失败"))?;
            self.audit("", &user_id, "user.bootstrap_rescue", username, None)?;
            return Ok(UserInfo {
                user_id,
                username: username.to_string(),
                display_name: display_name.trim().to_string(),
                role: Role::Admin,
                status: core_types::AccountStatus::Active,
            });
        }

        let hash = auth::hash_password(password)?;
        let user_id = new_id("u");
        let now = now_secs();
        self.conn
            .execute(
                "INSERT INTO users (user_id, username, display_name, source, status, role,
                                    password_hash, token_version, created_at, approved_at)
                 VALUES (?1, ?2, ?3, 'local', 'active', 'admin', ?4, 1, ?5, ?5)",
                params![user_id, username, display_name.trim(), hash, now],
            )
            .map_err(db("创建引导管理员失败"))?;
        self.audit("", &user_id, "user.bootstrap", username, None)?;

        Ok(UserInfo {
            user_id,
            username: username.to_string(),
            display_name: display_name.trim().to_string(),
            role: Role::Admin,
            status: core_types::AccountStatus::Active,
        })
    }

    /// 本地登录：默认 12 小时会话（CLI / Web 管理端）。
    pub fn login(&self, username: &str, password: &str) -> Result<(String, UserInfo)> {
        self.login_with_ttl(username, password, auth::DEFAULT_TOKEN_TTL_SECONDS)
            .map(|(token, _, user)| (token, user))
    }

    /// 本地登录并指定本次 token 有效期。
    ///
    /// 校验状态机 + 口令，签发带 `token_version` 的 JWT；返回值中间一项是
    /// `expires_at`（Unix 秒），供客户端在本地判断 30 天免登录会话何时到期并回登录页。
    pub fn login_with_ttl(
        &self,
        username: &str,
        password: &str,
        ttl_seconds: i64,
    ) -> Result<(String, i64, UserInfo)> {
        let row: Option<LoginRow> = self
            .conn
            .query_row(
                "SELECT user_id, username, display_name, status, role, source,
                        password_hash, token_version
                 FROM users WHERE username = ?1",
                params![username],
                |r| {
                    Ok(LoginRow {
                        user_id: r.get(0)?,
                        username: r.get(1)?,
                        display_name: r.get(2)?,
                        status: r.get(3)?,
                        role: r.get(4)?,
                        source: r.get(5)?,
                        password_hash: r.get(6)?,
                        token_version: r.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(db("查询用户失败"))?;

        let LoginRow {
            user_id,
            username,
            display_name,
            status,
            role,
            source,
            password_hash,
            token_version,
        } = row.ok_or_else(|| Error::new(ErrorCode::InvalidCredentials, "用户名或密码错误"))?;

        let status = parse_account_status(&status)?;
        if !status.may_login() {
            // 账号状态问题单独报错，便于 GUI 给出「等待审批」而不是「密码错」。
            return Err(match status {
                core_types::AccountStatus::Pending => {
                    Error::new(ErrorCode::AccountPending, "账号待审批，尚不能登录")
                }
                core_types::AccountStatus::Disabled => {
                    Error::new(ErrorCode::AccountDisabled, "账号已被禁用")
                }
                _ => Error::new(ErrorCode::InvalidCredentials, "账号不可用"),
            });
        }

        // 本地账号没有口令：新流程在注册时就设好密码，走到这里只可能是
        // 旧数据或人为改库，明确告诉用户该重新申请，而不是笼统报「密码错」。
        let phc = password_hash.ok_or_else(|| {
            if source == "ldap" {
                Error::new(
                    ErrorCode::InvalidCredentials,
                    "该账号为目录账号，请走 LDAP 登录",
                )
            } else {
                Error::new(
                    ErrorCode::AccountPending,
                    "该账号尚未设置密码，请重新提交注册申请或联系管理员",
                )
            }
        })?;
        if !auth::verify_password(password, &phc) {
            self.conn
                .execute(
                    "UPDATE users SET failed_logins = failed_logins + 1 WHERE user_id = ?1",
                    params![user_id],
                )
                .map_err(db("记录登录失败次数失败"))?;
            return Err(Error::new(
                ErrorCode::InvalidCredentials,
                "用户名或密码错误",
            ));
        }

        let role = parse_role(&role)?;
        let now = now_secs();
        self.conn
            .execute(
                "UPDATE users SET failed_logins = 0, last_login_at = ?2 WHERE user_id = ?1",
                params![user_id, now],
            )
            .map_err(db("更新登录信息失败"))?;

        let token = self.signer.sign_with_ttl(
            &user_id,
            &username,
            role_to_str(role),
            token_version,
            now,
            ttl_seconds,
        )?;
        self.audit("", &user_id, "user.login", &username, None)?;

        Ok((
            token,
            now + ttl_seconds,
            UserInfo {
                user_id,
                username,
                display_name,
                role,
                status,
            },
        ))
    }

    /// 校验 token 并加载当前用户状态：禁用或 token_version 变化都会立即失效。
    /// 校验凭证。JWT 与 PAT 共用同一个入口：客户端只持有一个「token」，
    /// 不应该让调用方先判断类型再选方法。
    pub fn authenticate(&self, token: &str) -> Result<Session> {
        if token.starts_with(PAT_PREFIX) {
            return self.authenticate_pat(token);
        }
        let claims = self.signer.verify(token, now_secs())?;
        self.session_from_claims(&claims)
    }

    fn session_from_claims(&self, claims: &Claims) -> Result<Session> {
        let row: Option<(String, String, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT username, display_name, status, role, token_version
                 FROM users WHERE user_id = ?1",
                params![claims.sub],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(db("查询用户失败"))?;

        let (username, display_name, status, role, token_version) =
            row.ok_or_else(|| Error::new(ErrorCode::Unauthenticated, "token 对应的用户不存在"))?;
        if !parse_account_status(&status)?.may_login() {
            return Err(Error::new(ErrorCode::AccountDisabled, "账号已不可用"));
        }
        if token_version != claims.tv {
            return Err(Error::new(
                ErrorCode::Unauthenticated,
                "登录状态已失效，请重新登录",
            ));
        }
        Ok(Session {
            user_id: claims.sub.clone(),
            username,
            display_name,
            role: parse_role(&role)?,
            kind: ActorKind::User,
            // JWT 就是「本人」，不做能力收窄；PAT 才带 scope。
            scopes: None,
        })
    }

    // ---------------- 仓库 ----------------

    /// 管理员直通建库（对应 `CreateRepoDirect`）：写审计，供紧急开通用。
    /// `quota_bytes = 0` 表示继承全局默认配额。
    pub fn create_repo_direct(
        &self,
        session: &Session,
        name: &str,
        quota_bytes: u64,
    ) -> Result<RepoInfo> {
        if session.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "仅管理员可直通创建仓库",
            ));
        }
        session.require(SCOPE_ADMIN, "直通创建仓库")?;
        validate_repo_name(name)?;

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        let repo_id = self.insert_new_repo(
            &tx,
            name,
            &session.user_id,
            quota_bytes,
            None,
            &session.username,
            "repo.create_direct",
        )?;
        tx.commit().map_err(db("提交建库事务失败"))?;

        Ok(RepoInfo {
            repo_id,
            name: name.to_string(),
            head_rev: 0,
            status: RepoStatus::Active,
            owner_user_id: session.user_id.clone(),
        })
    }

    pub fn find_repo(&self, name_or_id: &str) -> Result<RepoInfo> {
        let row: Option<RepoInfo> = self
            .conn
            .query_row(
                "SELECT repo_id, name, head_rev, status, owner_user_id
                 FROM repos WHERE repo_id = ?1 OR name = ?1",
                params![name_or_id],
                |r| {
                    Ok(RepoInfo {
                        repo_id: r.get(0)?,
                        name: r.get(1)?,
                        head_rev: r.get::<_, i64>(2)? as u64,
                        status: RepoStatus::Active,
                        owner_user_id: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(db("查询仓库失败"))?;
        let mut repo = row.ok_or_else(|| Error::new(ErrorCode::RepoNotFound, "仓库不存在"))?;
        let status: String = self
            .conn
            .query_row(
                "SELECT status FROM repos WHERE repo_id = ?1",
                params![repo.repo_id],
                |r| r.get(0),
            )
            .map_err(db("查询仓库状态失败"))?;
        repo.status = parse_repo_status(&status)?;
        Ok(repo)
    }

    /// 成员门：非成员一律 `REPO_NOT_MEMBER`，与全局角色无关。
    pub fn member_role(&self, repo_id: &str, user_id: &str) -> Result<RepoMemberRole> {
        let role: Option<String> = self
            .conn
            .query_row(
                "SELECT role FROM repo_members WHERE repo_id = ?1 AND user_id = ?2",
                params![repo_id, user_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询成员失败"))?;
        role.map(|r| parse_member_role(&r)).unwrap_or_else(|| {
            Err(Error::new(
                ErrorCode::RepoNotMember,
                "不是该仓库成员，无权访问",
            ))
        })
    }

    fn require_repo(&self, session: &Session, name_or_id: &str) -> Result<RepoInfo> {
        let repo = self.find_repo(name_or_id)?;
        if !repo.status.is_usable() {
            return Err(match repo.status {
                RepoStatus::Disabled => Error::new(ErrorCode::RepoDisabled, "仓库已被禁用"),
                _ => Error::new(ErrorCode::RepoArchived, "仓库已归档"),
            });
        }
        // 成员门先于任何业务逻辑；全局 admin 也不能绕过（审计另有通道）。
        // 唯一的例外是系统身份（定时维护），它没有「成员」可言。
        if !session.is_system() {
            self.member_role(&repo.repo_id, &session.user_id)?;
        }
        Ok(repo)
    }

    /// 管理员直接建号（对应审批通过后创建 ACTIVE 用户，或企业内批量开户）。
    ///
    /// 密码策略与自助注册共用同一份校验，不做「管理员就能设弱口令」的例外。
    pub fn create_user(
        &self,
        actor: &Session,
        username: &str,
        display_name: &str,
        password: &str,
        role: Role,
    ) -> Result<UserInfo> {
        if actor.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "仅管理员可创建用户",
            ));
        }
        actor.require(SCOPE_ADMIN, "创建用户")?;
        validate_username(username)?;
        if display_name.trim().is_empty() {
            return Err(Error::new(ErrorCode::InvalidCredentials, "姓名不能为空"));
        }
        let issues = core_types::validate_password(password);
        if !issues.is_empty() {
            return Err(Error::new(
                ErrorCode::PasswordPolicyViolation,
                format!("密码不满足策略：{}", describe_password_issues(&issues)),
            ));
        }
        let hash = auth::hash_password(password)?;
        let user_id = new_id("u");
        let now = now_secs();
        self.conn
            .execute(
                "INSERT INTO users (user_id, username, display_name, source, status, role,
                                    password_hash, token_version, created_at, approved_by, approved_at)
                 VALUES (?1, ?2, ?3, 'local', 'active', ?4, ?5, 1, ?6, ?7, ?6)",
                params![
                    user_id,
                    username,
                    display_name,
                    role_to_str(role),
                    hash,
                    now,
                    actor.user_id
                ],
            )
            .map_err(|e| {
                let text = e.to_string();
                if text.contains("UNIQUE") {
                    Error::new(ErrorCode::RegistrationExists, "用户名已被占用")
                } else {
                    Error::new(ErrorCode::IoError, text)
                }
            })?;
        self.audit("", &actor.user_id, "user.create", username, None)?;

        Ok(UserInfo {
            user_id,
            username: username.to_string(),
            display_name: display_name.to_string(),
            role,
            status: core_types::AccountStatus::Active,
        })
    }

    /// 添加仓库成员。调用者必须先通过成员门，再按角色矩阵判断能否授予目标角色。
    pub fn add_member(
        &self,
        actor: &Session,
        name_or_id: &str,
        username: &str,
        role: RepoMemberRole,
    ) -> Result<()> {
        let repo = self.find_repo(name_or_id)?;
        // 内置 Web 管理后台用 system 身份代理已鉴权的管理员，可管理任意仓库的成员；
        // 外部会话拿不到 system 身份，成员门依旧严格。
        let actor_role = if actor.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &actor.user_id)?
        };
        if !actor_role.can_manage_members() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "只有 owner / maintainer 可以管理成员",
            ));
        }
        if !actor_role.can_assign(role) {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                match role {
                    RepoMemberRole::Owner => "owner 唯一，不可授予他人",
                    _ => "maintainer 只能授予 writer / reader",
                },
            ));
        }

        let user: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT user_id, status FROM users WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db("查询用户失败"))?;
        let (user_id, status) =
            user.ok_or_else(|| Error::new(ErrorCode::UserNotFound, "用户不存在"))?;
        if !parse_account_status(&status)?.may_login() {
            return Err(Error::new(
                ErrorCode::AccountDisabled,
                "该用户当前不可用，不能加入仓库",
            ));
        }

        let now = now_secs();
        self.conn
            .execute(
                "INSERT INTO repo_members (repo_id, user_id, role, added_by, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    repo.repo_id,
                    user_id,
                    member_role_str(role),
                    actor.user_id,
                    now
                ],
            )
            .map_err(|e| {
                let text = e.to_string();
                if text.contains("UNIQUE") || text.contains("PRIMARY") {
                    Error::new(ErrorCode::RepoMemberExists, "该用户已是仓库成员")
                } else {
                    Error::new(ErrorCode::IoError, text)
                }
            })?;
        self.audit(
            &repo.repo_id,
            &actor.user_id,
            "repo.member.add",
            username,
            Some(&format!("{{\"role\":\"{}\"}}", member_role_str(role))),
        )?;
        Ok(())
    }

    /// 移除成员。
    ///
    /// 两条硬约束：owner 唯一且不可移除（`RepoOwnerProtected`）；maintainer 只能移除
    /// 自己管得动的角色（writer / reader）。移除同时把该用户在该仓库里未完成的提交会话
    /// 置为 `aborted`——设计 8.9 要求的「成员变更即时生效」，靠 `FinalizeCommit` 的二次
    /// 校验也能兜住结果，但那要等用户点了提交才失败，体验很差。
    pub fn remove_member(&self, actor: &Session, name_or_id: &str, username: &str) -> Result<()> {
        let repo = self.find_repo(name_or_id)?;
        // 内置 Web 管理后台用 system 身份代理已鉴权的管理员，可管理任意仓库的成员；
        // 外部会话拿不到 system 身份，成员门依旧严格。
        let actor_role = if actor.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &actor.user_id)?
        };
        if !actor_role.can_manage_members() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "只有 owner / maintainer 可以管理成员",
            ));
        }
        let (target_id, target_role) = self.load_member(&repo.repo_id, username)?;
        if target_role.is_protected() {
            return Err(Error::new(
                ErrorCode::RepoOwnerProtected,
                "owner 唯一且受保护，不可移除",
            ));
        }
        if !actor_role.can_assign(target_role) {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "maintainer 只能移除 writer / reader",
            ));
        }
        if target_id == actor.user_id {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "不能移除自己；请让 owner 处理",
            ));
        }

        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "DELETE FROM repo_members WHERE repo_id = ?1 AND user_id = ?2",
            params![repo.repo_id, target_id],
        )
        .map_err(db("删除成员失败"))?;
        let aborted = abort_sessions_of(&tx, &repo.repo_id, &target_id)?;
        write_audit(
            &tx,
            &repo.repo_id,
            &actor.user_id,
            "repo.member.remove",
            username,
            Some(&format!("{{\"aborted_sessions\":{aborted}}}")),
            now,
        )?;
        tx.commit().map_err(db("提交成员变更失败"))?;
        Ok(())
    }

    /// 修改成员角色。owner 不可降级；不能把别人提成 owner（owner 唯一）。
    pub fn set_member_role(
        &self,
        actor: &Session,
        name_or_id: &str,
        username: &str,
        role: RepoMemberRole,
    ) -> Result<MemberInfo> {
        let repo = self.find_repo(name_or_id)?;
        // 内置 Web 管理后台用 system 身份代理已鉴权的管理员，可管理任意仓库的成员；
        // 外部会话拿不到 system 身份，成员门依旧严格。
        let actor_role = if actor.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &actor.user_id)?
        };
        if !actor_role.can_manage_members() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "只有 owner / maintainer 可以管理成员",
            ));
        }
        let (target_id, current) = self.load_member(&repo.repo_id, username)?;
        if current.is_protected() {
            return Err(Error::new(
                ErrorCode::RepoOwnerProtected,
                "owner 唯一且受保护，不可降级",
            ));
        }
        if !actor_role.can_assign(current) || !actor_role.can_assign(role) {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "maintainer 只能调整 writer / reader 的角色",
            ));
        }

        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "UPDATE repo_members SET role = ?3 WHERE repo_id = ?1 AND user_id = ?2",
            params![repo.repo_id, target_id, member_role_str(role)],
        )
        .map_err(db("更新成员角色失败"))?;
        // 降级到只读要立刻生效：否则对方手上的提交会话还能写完。
        let aborted = if role.can_write() {
            0
        } else {
            abort_sessions_of(&tx, &repo.repo_id, &target_id)?
        };
        write_audit(
            &tx,
            &repo.repo_id,
            &actor.user_id,
            "repo.member.set_role",
            username,
            Some(&format!(
                "{{\"from\":\"{}\",\"to\":\"{}\",\"aborted_sessions\":{aborted}}}",
                member_role_str(current),
                member_role_str(role)
            )),
            now,
        )?;
        tx.commit().map_err(db("提交成员变更失败"))?;

        let (username, display_name) = self.user_identity(&target_id)?;
        Ok(MemberInfo {
            user_id: target_id,
            username,
            display_name,
            role,
        })
    }

    /// 设置单仓库配额上限。`inherit = true` 表示清零并回到继承全局默认。
    pub fn set_repo_quota(
        &self,
        actor: &Session,
        name_or_id: &str,
        quota_bytes: u64,
        inherit: bool,
    ) -> Result<EffectiveQuota> {
        if actor.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "仅管理员可调整单仓库配额",
            ));
        }
        actor.require(SCOPE_ADMIN, "调整单仓库配额")?;
        let repo = self.find_repo(name_or_id)?;
        let quota = if inherit { 0 } else { quota_bytes };
        let before = self.repo_quota_override(&repo.repo_id)?;
        self.conn
            .execute(
                "UPDATE repos SET quota_bytes = ?2, updated_at = ?3
                 WHERE repo_id = ?1",
                params![repo.repo_id, quota as i64, now_secs()],
            )
            .map_err(db("更新仓库配额失败"))?;
        self.audit(
            &repo.repo_id,
            &actor.user_id,
            "repo.quota.update",
            &repo.name,
            Some(&format!(
                "{{\"from\":{},\"to\":{},\"inherit\":{}}}",
                before, quota, inherit
            )),
        )?;
        self.effective_quota(&repo.repo_id)
    }

    /// 变更仓库状态（active / disabled / archived）。非 active 时立刻中止该仓库所有在途提交。
    pub fn set_repo_status(
        &self,
        actor: &Session,
        name_or_id: &str,
        status: RepoStatus,
    ) -> Result<RepoInfo> {
        if actor.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "仅管理员可变更仓库状态",
            ));
        }
        actor.require(SCOPE_ADMIN, "变更仓库状态")?;
        let repo = self.find_repo(name_or_id)?;
        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "UPDATE repos SET status = ?2, updated_at = ?3 WHERE repo_id = ?1",
            params![repo.repo_id, repo_status_str(status), now],
        )
        .map_err(db("更新仓库状态失败"))?;
        let aborted = if status.is_usable() {
            0
        } else {
            // 整个仓库都不让写了，所有成员的在途会话一起作废。
            tx.execute(
                "UPDATE upload_sessions SET state = 'aborted'
                 WHERE repo_id = ?1 AND state IN ('open', 'pushing')",
                params![repo.repo_id],
            )
            .map_err(db("中止在途提交失败"))? as i64
        };
        write_audit(
            &tx,
            &repo.repo_id,
            &actor.user_id,
            "repo.status.set",
            &repo.name,
            Some(&format!(
                "{{\"from\":\"{}\",\"to\":\"{}\",\"aborted_sessions\":{aborted}}}",
                repo_status_str(repo.status),
                repo_status_str(status)
            )),
            now,
        )?;
        tx.commit().map_err(db("提交仓库状态变更失败"))?;
        self.find_repo(&repo.repo_id)
    }

    fn load_member(&self, repo_id: &str, username: &str) -> Result<(String, RepoMemberRole)> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT m.user_id, m.role FROM repo_members m
                 JOIN users u ON u.user_id = m.user_id
                 WHERE m.repo_id = ?1 AND u.username = ?2",
                params![repo_id, username],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db("查询成员失败"))?;
        let (user_id, role) =
            row.ok_or_else(|| Error::new(ErrorCode::RepoMemberNotFound, "该用户不是仓库成员"))?;
        Ok((user_id, parse_member_role(&role)?))
    }

    /// 按 user_id 取用户名。gRPC 的成员接口用 user_id 定位，业务层按用户名操作。
    pub fn username_by_id(&self, user_id: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT username FROM users WHERE user_id = ?1",
                params![user_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询用户失败"))?
            .ok_or_else(|| Error::new(ErrorCode::UserNotFound, "用户不存在"))
    }

    fn user_identity(&self, user_id: &str) -> Result<(String, String)> {
        self.conn
            .query_row(
                "SELECT username, display_name FROM users WHERE user_id = ?1",
                params![user_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询用户失败：{e}")))
    }

    pub fn list_members(&self, actor: &Session, name_or_id: &str) -> Result<Vec<MemberInfo>> {
        let repo = self.find_repo(name_or_id)?;
        // 成员门：连成员名单都不给非成员看；system 身份仅由内置管理后台使用。
        if !actor.is_system() {
            self.member_role(&repo.repo_id, &actor.user_id)?;
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.user_id, u.username, u.display_name, m.role
                 FROM repo_members m JOIN users u ON u.user_id = m.user_id
                 WHERE m.repo_id = ?1
                 ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'maintainer' THEN 1
                                       WHEN 'writer' THEN 2 ELSE 3 END, u.username",
            )
            .map_err(db("查询成员失败"))?;
        let rows = stmt
            .query_map(params![repo.repo_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(db("查询成员失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (user_id, username, display_name, role) = row.map_err(db("读取成员行失败"))?;
            out.push(MemberInfo {
                user_id,
                username,
                display_name,
                role: parse_member_role(&role)?,
            });
        }
        Ok(out)
    }

    // ---------------- 提交（import） ----------------

    /// 把本地目录导入为下一个版本。
    ///
    /// 这是「全量提交」的便捷入口：自动识别增删改，然后走标准的
    /// `begin_commit` → `push_objects` → `finalize_commit` 三段式。
    /// 走同一条路径的好处是配额预检、严格 out-of-date、对象校验只有一处实现。
    pub fn import_dir(
        &self,
        session: &Session,
        name_or_id: &str,
        dir: &Path,
        message: &str,
    ) -> Result<ImportOutcome> {
        session.require(SCOPE_WRITE, "提交")?;
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "当前成员角色为只读，不能提交",
            ));
        }
        if !dir.is_dir() {
            return Err(Error::new(
                ErrorCode::IoError,
                format!("{} 不是目录", dir.display()),
            ));
        }

        // 1) 第一遍：只读磁盘算对象地址，**一个字节都不写**。
        let mut scanned: BTreeMap<String, (Hash, u64, EntryKind)> = BTreeMap::new();
        scan_dir(dir, "", &mut scanned)?;

        // 2) 与上一版比较得出变更集。
        let parent_tree = self.revision_root_tree(&repo.repo_id, repo.head_rev)?;
        let old_flat = flatten_map(&self.objects, &parent_tree)?;
        let changes = diff_maps(&old_flat, &scanned);

        let unchanged = scanned
            .keys()
            .filter(|p| old_flat.get(*p).map(|(h, _, _)| h) == scanned.get(*p).map(|(h, _, _)| h))
            .count();

        if changes.is_empty() {
            return Ok(ImportOutcome {
                rev: repo.head_rev,
                no_changes: true,
                added: 0,
                modified: 0,
                deleted: 0,
                unchanged,
                new_object_bytes: 0,
            });
        }

        // 3) 预检：预计新增字节扣掉本仓库即将不再引用的部分。
        let estimated = estimate_new_bytes(&scanned, &self.objects)
            .saturating_sub(self.reclaimable_hint(&old_flat, &scanned));
        let commit = self.begin_commit(
            session,
            &repo.repo_id,
            repo.head_rev,
            &changes,
            message,
            estimated,
        )?;

        // 4) 分批推送对象，避免把整个目录读进内存。
        let mut batch: Vec<(Hash, Vec<u8>)> = Vec::new();
        let mut batch_bytes = 0u64;
        let mut seen: std::collections::BTreeSet<Hash> = std::collections::BTreeSet::new();
        for (rel, (hash, size, kind)) in &scanned {
            if seen.contains(hash) || self.objects.exists(hash) {
                continue;
            }
            let data = read_entry_bytes(dir, rel, *kind)?;
            batch.push((*hash, data));
            batch_bytes += *size;
            seen.insert(*hash);
            if batch.len() >= 32 || batch_bytes >= 32 * 1024 * 1024 {
                self.push_objects(session, &commit.session_id, &batch)?;
                batch.clear();
                batch_bytes = 0;
            }
        }
        if !batch.is_empty() {
            self.push_objects(session, &commit.session_id, &batch)?;
        }

        // 5) 分配版本号。会话本身不知道「未变」的数量，由本次扫描补上。
        let mut outcome = self.finalize_commit(session, &commit.session_id)?;
        outcome.unchanged = unchanged;
        Ok(outcome)
    }

    /// 开始一次提交：校验权限与版本、配额预检，并把变更集落到会话表。
    ///
    /// 严格 out-of-date 在**这里**就拦一次，`finalize_commit` 再拦一次：
    /// 第一次给用户明确反馈，第二次防住会话期间别人抢先提交的竞态。
    pub fn begin_commit(
        &self,
        session: &Session,
        name_or_id: &str,
        base_rev: u64,
        changes: &[PendingChange],
        message: &str,
        estimated_new_bytes: u64,
    ) -> Result<CommitSession> {
        // PAT 的能力范围先于成员角色判断：只读令牌即便持有人是 owner 也不能提交。
        session.require(SCOPE_WRITE, "提交")?;
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "当前成员角色为只读，不能提交",
            ));
        }
        if core_types::CommitPolicy::default().rejects_out_of_date(base_rev, repo.head_rev) {
            return Err(Error::new(
                ErrorCode::OutOfDate,
                format!(
                    "本地版本 r{base_rev} 已落后于仓库 r{}，请先 update 再提交",
                    repo.head_rev
                ),
            ));
        }
        if changes.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidSession,
                "变更集为空，无需提交",
            ));
        }

        // 变更集本身要自洽：路径唯一、删除不带内容、写入必带地址。
        let mut seen_paths = std::collections::BTreeSet::new();
        for change in changes {
            let path = core_types::normalize_path(&change.path)?;
            if path.is_empty() {
                return Err(Error::new(ErrorCode::PathConflict, "路径不能为空"));
            }
            if !seen_paths.insert(path.clone()) {
                return Err(Error::new(
                    ErrorCode::PathConflict,
                    format!("同一路径在变更集中出现多次：{path}"),
                ));
            }
            match change.kind {
                ChangeKind::Delete if change.hash.is_some() => {
                    return Err(Error::new(
                        ErrorCode::PathConflict,
                        format!("删除 {path} 不应携带内容地址"),
                    ));
                }
                ChangeKind::Add | ChangeKind::Modify if change.hash.is_none() => {
                    return Err(Error::new(
                        ErrorCode::PathConflict,
                        format!("写入 {path} 必须携带内容地址"),
                    ));
                }
                _ => {}
            }
        }

        let usage = self.usage(&repo.repo_id)?;
        let projected = usage.used_bytes.saturating_add(estimated_new_bytes);
        if projected > usage.quota.bytes {
            return Err(Error::new(
                ErrorCode::QuotaExceeded,
                format!(
                    "提交将超出仓库配额：预计 {} 字节，上限 {} 字节。请先清理目录释放空间",
                    projected, usage.quota.bytes
                ),
            ));
        }

        let session_id = new_id("sess");
        let now = now_secs();
        let stored: Vec<StoredChange> = changes
            .iter()
            .map(|c| StoredChange {
                path: c.path.clone(),
                kind: change_kind_str(c.kind).to_string(),
                entry_kind: kind_str(c.entry_kind).to_string(),
                new_hash: c.hash.as_ref().map(to_hex),
                size: c.size,
            })
            .collect();
        let changeset = serde_json::to_string(&stored)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("序列化变更集失败：{e}")))?;

        self.conn
            .execute(
                "INSERT INTO upload_sessions (session_id, repo_id, client_id, base_rev, message,
                    changeset, received_hashes, estimated_new_bytes, state, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '[]', ?7, 'open', ?8, ?9)",
                params![
                    session_id,
                    repo.repo_id,
                    session.user_id,
                    base_rev as i64,
                    message,
                    changeset,
                    estimated_new_bytes as i64,
                    now,
                    now + SESSION_TTL_SECONDS
                ],
            )
            .map_err(db("创建提交会话失败"))?;
        self.audit(
            &repo.repo_id,
            &session.user_id,
            "commit.begin",
            &session_id,
            None,
        )?;

        Ok(CommitSession {
            session_id,
            repo_id: repo.repo_id,
            base_rev,
            message: message.to_string(),
            change_count: changes.len(),
        })
    }

    /// 推送对象内容。幂等：已存在的对象直接跳过，重复推送不报错（断点续传语义）。
    pub fn push_objects(
        &self,
        session: &Session,
        session_id: &str,
        objects: &[(Hash, Vec<u8>)],
    ) -> Result<usize> {
        let row = self.load_session(session, session_id)?;
        if matches!(row.state.as_str(), "committed" | "aborted" | "expired") {
            return Err(Error::new(
                ErrorCode::InvalidSession,
                format!("会话已结束（{}），不能继续推送", row.state),
            ));
        }

        let mut accepted = 0usize;
        for (expected, data) in objects {
            // 地址必须与内容一致：不允许客户端把任意字节塞进来。
            let actual = hash_bytes(data);
            if actual != *expected {
                return Err(Error::new(
                    ErrorCode::ObjectMissing,
                    format!(
                        "对象内容与地址不符：期望 {}，实际 {}",
                        to_hex(expected),
                        to_hex(&actual)
                    ),
                ));
            }
            self.objects.put(expected, data)?;
            accepted += 1;
        }

        self.conn
            .execute(
                "UPDATE upload_sessions SET state = 'pushing' WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(db("更新会话状态失败"))?;
        Ok(accepted)
    }

    /// 结束提交：在单事务内分配版本号、应用变更集、写审计。
    pub fn finalize_commit(&self, session: &Session, session_id: &str) -> Result<ImportOutcome> {
        let row = self.load_session(session, session_id)?;
        if row.state == "committed" {
            return Err(Error::new(ErrorCode::InvalidSession, "该会话已提交"));
        }
        if !matches!(row.state.as_str(), "open" | "pushing") {
            return Err(Error::new(
                ErrorCode::InvalidSession,
                format!("会话状态 {} 不允许提交", row.state),
            ));
        }

        // 成员资格与写权限在这里再校验一次：会话过程中可能被移除或降级。
        let repo = self.require_repo(session, &row.repo_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "提交过程中成员权限被降级，提交已取消",
            ));
        }

        let changes = self.session_changes(session_id)?;
        let (added, modified, deleted) = count_changes(&changes);

        // 所有新内容必须已经在对象池里，否则 commit 会写出一条指向不存在内容的版本。
        for change in &changes {
            if let Some(hash) = change.hash {
                if !self.objects.exists(&hash) {
                    return Err(Error::new(
                        ErrorCode::ObjectMissing,
                        format!(
                            "路径 {} 的内容 {} 尚未推送，拒绝提交",
                            change.path,
                            to_hex(&hash)
                        ),
                    ));
                }
            }
        }

        // 严格 out-of-date：会话开始后有人提交过，整个变更集作废。
        if core_types::CommitPolicy::default().rejects_out_of_date(row.base_rev, repo.head_rev) {
            self.conn
                .execute(
                    "UPDATE upload_sessions SET state = 'aborted' WHERE session_id = ?1",
                    params![session_id],
                )
                .map_err(db("标记会话失败"))?;
            return Err(Error::new(
                ErrorCode::OutOfDate,
                format!(
                    "仓库已更新到 r{}（本次基于 r{}），请先 update 再提交",
                    repo.head_rev, row.base_rev
                ),
            ));
        }

        let parent_tree = self.revision_root_tree(&repo.repo_id, row.base_rev)?;
        let ops: Vec<vcs_core::TreeOp> = changes
            .iter()
            .map(|c| match c.hash {
                Some(hash) => vcs_core::TreeOp::Upsert {
                    path: c.path.clone(),
                    kind: c.kind_entry,
                    hash,
                    size: c.size,
                },
                None => vcs_core::TreeOp::Delete {
                    path: c.path.clone(),
                },
            })
            .collect();
        let (root_tree, written_trees) =
            vcs_core::apply_changes_tracked(&self.objects, &parent_tree, &ops)?;

        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;

        // BEGIN IMMEDIATE 语义：事务内重读 head，保证单写者。
        let head: i64 = tx
            .query_row(
                "SELECT head_rev FROM repos WHERE repo_id = ?1",
                params![repo.repo_id],
                |r| r.get(0),
            )
            .map_err(db("读取 head_rev 失败"))?;
        if head as u64 != row.base_rev {
            return Err(Error::new(
                ErrorCode::OutOfDate,
                "仓库在提交瞬间被更新，请重试",
            ));
        }

        let next_rev = row.base_rev + 1;
        for (seq, change) in changes.iter().enumerate() {
            tx.execute(
                "INSERT INTO revision_changes (repo_id, rev, seq, path, kind, old_hash, new_hash, from_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)",
                params![
                    repo.repo_id,
                    next_rev as i64,
                    seq as i64,
                    change.path,
                    change_kind_str(change.kind),
                    change.hash.as_ref().map(to_hex),
                ],
            )
            .map_err(db("写入变更记录失败"))?;
        }
        tx.execute(
            "INSERT INTO revisions (repo_id, rev, root_tree, parent_rev, author, message, committed_at, change_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                repo.repo_id,
                next_rev as i64,
                to_hex(&root_tree),
                row.base_rev as i64,
                session.username,
                row.message,
                now,
                changes.len() as i64
            ],
        )
        .map_err(db("写入版本失败"))?;
        tx.execute(
            "UPDATE repos SET head_rev = ?2, updated_at = ?3 WHERE repo_id = ?1",
            params![repo.repo_id, next_rev as i64, now],
        )
        .map_err(db("更新 head_rev 失败"))?;

        // 只登记本次新增的对象：变更内容 + COW 实际重写的树节点。
        let mut new_object_bytes = 0u64;
        for change in &changes {
            if let Some(hash) = change.hash {
                new_object_bytes += index_object(
                    &tx,
                    &self.objects,
                    &hash,
                    change.size,
                    ObjectKindTag::Blob,
                    next_rev,
                    now,
                )?;
            }
        }
        for tree_hash in written_trees {
            let size = self
                .objects
                .get(&tree_hash)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            new_object_bytes += index_object(
                &tx,
                &self.objects,
                &tree_hash,
                size,
                ObjectKindTag::Tree,
                next_rev,
                now,
            )?;
        }

        tx.execute(
            "UPDATE upload_sessions SET state = 'committed' WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(db("更新会话状态失败"))?;

        write_audit(
            &tx,
            &repo.repo_id,
            &session.user_id,
            "commit.finalize",
            &format!("r{next_rev}"),
            Some(&format!(
                "{{\"added\":{added},\"modified\":{modified},\"deleted\":{deleted},\"session\":\"{session_id}\"}}"
            )),
            now,
        )?;
        tx.commit().map_err(db("提交事务失败"))?;

        Ok(ImportOutcome {
            rev: next_rev,
            no_changes: false,
            added,
            modified,
            deleted,
            unchanged: 0,
            new_object_bytes,
        })
    }

    /// 放弃一次提交。已经写入的对象留给 GC 清扫，不影响版本链。
    pub fn abort_commit(&self, session: &Session, session_id: &str) -> Result<()> {
        self.load_session(session, session_id)?;
        self.conn
            .execute(
                "UPDATE upload_sessions SET state = 'aborted' WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(db("放弃提交失败"))?;
        Ok(())
    }

    /// 两棵树之间的差异，供 update 使用。
    pub fn get_changes(
        &self,
        session: &Session,
        name_or_id: &str,
        from_rev: u64,
        to_rev: u64,
    ) -> Result<Vec<PendingChange>> {
        let repo = self.require_repo(session, name_or_id)?;
        if to_rev < from_rev {
            return Err(Error::new(
                ErrorCode::PathConflict,
                "目标版本不能早于起始版本",
            ));
        }
        if to_rev > repo.head_rev {
            return Err(Error::new(
                ErrorCode::RepoNotFound,
                format!("r{to_rev} 不存在（当前 head 为 r{}）", repo.head_rev),
            ));
        }
        let from_tree = self.revision_root_tree(&repo.repo_id, from_rev)?;
        let to_tree = self.revision_root_tree(&repo.repo_id, to_rev)?;
        let from_map = flatten_map(&self.objects, &from_tree)?;
        let to_map = flatten_map(&self.objects, &to_tree)?;
        Ok(diff_maps(&from_map, &to_map))
    }

    /// 列出调用者是成员的仓库。
    ///
    /// 设计 8.9 明确：`ListRepos` 不返回非成员仓库——仓库名本身也是信息。
    /// `filter` 为空时只看 ACTIVE，避免 GUI 首页出现一堆已归档仓库。
    pub fn list_repos(
        &self,
        session: &Session,
        filter: Option<RepoStatus>,
    ) -> Result<Vec<RepoInfo>> {
        let status = match filter {
            Some(RepoStatus::Active) | None => "active",
            Some(RepoStatus::Disabled) => "disabled",
            Some(RepoStatus::Archived) => "archived",
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT r.repo_id, r.name, r.head_rev, r.status, r.owner_user_id
                 FROM repos r
                 JOIN repo_members m ON m.repo_id = r.repo_id
                 WHERE m.user_id = ?1 AND r.status = ?2
                 ORDER BY r.name",
            )
            .map_err(db("查询仓库列表失败"))?;
        let rows = stmt
            .query_map(params![session.user_id, status], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(db("查询仓库列表失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (repo_id, name, head_rev, status, owner_user_id) =
                row.map_err(db("读取仓库行失败"))?;
            out.push(RepoInfo {
                repo_id,
                name,
                head_rev: head_rev.max(0) as u64,
                status: parse_repo_status(&status)?,
                owner_user_id,
            });
        }
        Ok(out)
    }

    /// 按用户名 / 姓名查找可添加的 ACTIVE 用户。
    ///
    /// 只返回 user_id / username / display_name 三项：owner 添加成员需要认人，
    /// 但不需要看到别人的角色、状态或任何凭证信息。
    pub fn search_users(&self, query: &str, limit: usize) -> Result<Vec<UserSummary>> {
        let pattern = format!("%{}%", query.trim());
        let mut stmt = self
            .conn
            .prepare(
                "SELECT user_id, username, display_name FROM users
                 WHERE status = 'active' AND (username LIKE ?1 OR display_name LIKE ?1)
                 ORDER BY username LIMIT ?2",
            )
            .map_err(db("查询用户失败"))?;
        let rows = stmt
            .query_map(params![pattern, limit.max(1) as i64], |r| {
                Ok(UserSummary {
                    user_id: r.get(0)?,
                    username: r.get(1)?,
                    display_name: r.get(2)?,
                })
            })
            .map_err(db("查询用户失败"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db("查询用户失败"))
    }

    /// `GetHead` 的专用查询：走 `require_repo`（仓库状态 + 成员门），
    /// 非成员或已禁用的仓库不该知道 head 是多少。
    pub fn head_info(&self, session: &Session, name_or_id: &str) -> Result<(u64, Hash)> {
        let repo = self.require_repo(session, name_or_id)?;
        let root = self.revision_root_tree(&repo.repo_id, repo.head_rev)?;
        Ok((repo.head_rev, root))
    }

    /// 取单个版本（含变更清单）。`GetRevision` 需要它，而 `log` 只返回最近 N 条。
    pub fn get_revision(
        &self,
        session: &Session,
        name_or_id: &str,
        rev: u64,
    ) -> Result<RevisionInfo> {
        let repo = self.require_repo(session, name_or_id)?;
        let row: Option<(String, i64, String, String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT root_tree, parent_rev, author, message, committed_at, change_count
                 FROM revisions WHERE repo_id = ?1 AND rev = ?2",
                params![repo.repo_id, rev as i64],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(db("查询版本失败"))?;
        let (root_tree, parent_rev, author, message, committed_at, change_count) =
            row.ok_or_else(|| {
                Error::new(
                    ErrorCode::RepoNotFound,
                    format!("r{rev} 不存在（仓库 {}）", repo.name),
                )
            })?;
        Ok(RevisionInfo {
            rev,
            root_tree: vcs_core::from_hex(&root_tree)?,
            parent_rev: parent_rev.max(0) as u64,
            author,
            message,
            committed_at,
            change_count: change_count.max(0) as u32,
            changes: self.revision_changes(&repo.repo_id, rev)?,
        })
    }

    /// 取单条仓库创建申请。普通用户只能看自己的申请，避免审批状态在用户之间泄露。
    pub fn get_repo_request(&self, actor: &Session, request_id: &str) -> Result<RepoRequestInfo> {
        self.expire_stale_requests()?;
        let mine = if actor.role == Role::Admin {
            ""
        } else {
            actor.user_id.as_str()
        };
        let row: Option<RepoRequestRow> = self
            .conn
            .query_row(
                "SELECT request_id, requester_user_id, repo_name, reason, requested_quota_bytes,
                        status, submitted_at, decided_at, decided_by, decision_note,
                        expires_at, approved_repo_id
                 FROM repo_creation_requests
                 WHERE request_id = ?1 AND (?2 = '' OR requester_user_id = ?2)",
                params![request_id, mine],
                |r| {
                    Ok(RepoRequestRow {
                        request_id: r.get(0)?,
                        requester_user_id: r.get(1)?,
                        repo_name: r.get(2)?,
                        reason: r.get(3)?,
                        requested_quota_bytes: r.get(4)?,
                        status: r.get(5)?,
                        submitted_at: r.get(6)?,
                        decided_at: r.get(7)?,
                        decided_by: r.get(8)?,
                        decision_note: r.get(9)?,
                        expires_at: r.get(10)?,
                        approved_repo_id: r.get(11)?,
                    })
                },
            )
            .optional()
            .map_err(db("查询仓库申请失败"))?;
        let RepoRequestRow {
            request_id,
            requester_user_id,
            repo_name,
            reason,
            requested_quota_bytes,
            status,
            submitted_at,
            decided_at,
            decided_by,
            decision_note,
            expires_at,
            approved_repo_id,
        } = row.ok_or_else(|| Error::new(ErrorCode::RepoApprovalNotFound, "仓库申请不存在"))?;
        Ok(RepoRequestInfo {
            request_id,
            requester_user_id,
            repo_name,
            reason,
            requested_quota_bytes: requested_quota_bytes.max(0) as u64,
            state: parse_creation_status(&status)?,
            submitted_at,
            decided_at,
            decided_by,
            decision_note,
            expires_at,
            approved_repo_id,
        })
    }

    fn load_session(&self, session: &Session, session_id: &str) -> Result<SessionRow> {
        let row: Option<SessionRow> = self
            .conn
            .query_row(
                "SELECT repo_id, client_id, base_rev, message, state, expires_at
                 FROM upload_sessions WHERE session_id = ?1",
                params![session_id],
                |r| {
                    Ok(SessionRow {
                        repo_id: r.get(0)?,
                        client_id: r.get(1)?,
                        base_rev: r.get::<_, i64>(2)? as u64,
                        message: r.get(3)?,
                        state: r.get(4)?,
                        expires_at: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(db("查询提交会话失败"))?;
        let row = row.ok_or_else(|| Error::new(ErrorCode::InvalidSession, "提交会话不存在"))?;
        if row.client_id != session.user_id {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "不能操作他人的提交会话",
            ));
        }
        if row.expires_at < now_secs() && row.state != "committed" {
            return Err(Error::new(
                ErrorCode::InvalidSession,
                "提交会话已过期，请重新发起提交",
            ));
        }
        Ok(row)
    }

    fn session_changes(&self, session_id: &str) -> Result<Vec<SessionChange>> {
        let raw: String = self
            .conn
            .query_row(
                "SELECT changeset FROM upload_sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(db("读取变更集失败"))?;
        let stored: Vec<StoredChange> = serde_json::from_str(&raw)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("解析变更集失败：{e}")))?;
        stored
            .into_iter()
            .map(|c| {
                Ok(SessionChange {
                    path: c.path,
                    kind: parse_change_kind(&c.kind)?,
                    kind_entry: parse_kind(&c.entry_kind)?,
                    hash: c.new_hash.as_deref().map(vcs_core::from_hex).transpose()?,
                    size: c.size,
                })
            })
            .collect()
    }

    /// 本次提交后「本仓库不再引用」的对象字节数，让配额预检更接近真实可用空间。
    ///
    /// 注意这是**乐观估计**：共享 CAS 下这些字节可能仍被其他仓库引用，只有 M2 的 GC
    /// 标记阶段用反向引用集合才能算出精确的 unique_bytes。乐观估计的后果是用户在
    /// 「删除目录后立刻提交」时可能仍然被拒——此时真正的解法是跑一次 GC，而不是放宽配额。
    fn reclaimable_hint(
        &self,
        old_flat: &BTreeMap<String, (Hash, u64, EntryKind)>,
        new_map: &BTreeMap<String, (Hash, u64, EntryKind)>,
    ) -> u64 {
        let mut freed = 0u64;
        let still_used: std::collections::BTreeSet<Hash> =
            new_map.values().map(|(h, _, _)| *h).collect();
        for (path, (hash, _, _)) in old_flat {
            if new_map.contains_key(path) {
                continue;
            }
            if still_used.contains(hash) {
                continue;
            }
            freed += self.objects.stored_size(hash).unwrap_or(0);
        }
        freed
    }

    // ---------------- 读取 ----------------

    pub fn revision_root_tree(&self, repo_id: &str, rev: u64) -> Result<Hash> {
        let hex: Option<String> = self
            .conn
            .query_row(
                "SELECT root_tree FROM revisions WHERE repo_id = ?1 AND rev = ?2",
                params![repo_id, rev as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询版本失败"))?;
        match hex {
            Some(h) => vcs_core::from_hex(&h),
            None => Err(Error::new(
                ErrorCode::RepoNotFound,
                format!("版本 r{rev} 不存在"),
            )),
        }
    }

    pub fn log(
        &self,
        session: &Session,
        name_or_id: &str,
        limit: usize,
    ) -> Result<Vec<RevisionInfo>> {
        let repo = self.require_repo(session, name_or_id)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT rev, root_tree, parent_rev, author, message, committed_at, change_count
                 FROM revisions WHERE repo_id = ?1 ORDER BY rev DESC LIMIT ?2",
            )
            .map_err(db("查询日志失败"))?;
        let rows = stmt
            .query_map(params![repo.repo_id, limit as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })
            .map_err(db("查询日志失败"))?;

        let mut out = Vec::new();
        for row in rows {
            let (rev, root_tree, parent_rev, author, message, committed_at, change_count) =
                row.map_err(db("读取日志行失败"))?;
            out.push(RevisionInfo {
                rev: rev as u64,
                root_tree: vcs_core::from_hex(&root_tree)?,
                parent_rev: parent_rev as u64,
                author,
                message,
                committed_at,
                change_count: change_count as u32,
                changes: Vec::new(),
            });
        }
        Ok(out)
    }

    pub fn revision_changes(&self, repo_id: &str, rev: u64) -> Result<Vec<ChangeRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, kind, old_hash, new_hash FROM revision_changes
                 WHERE repo_id = ?1 AND rev = ?2 ORDER BY seq",
            )
            .map_err(db("查询变更失败"))?;
        let rows = stmt
            .query_map(params![repo_id, rev as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(db("查询变更失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (path, kind, old_hash, new_hash) = row.map_err(db("读取变更行失败"))?;
            out.push(ChangeRecord {
                path,
                kind: parse_change_kind(&kind)?,
                old_hash: old_hash.as_deref().map(vcs_core::from_hex).transpose()?,
                new_hash: new_hash.as_deref().map(vcs_core::from_hex).transpose()?,
            });
        }
        Ok(out)
    }

    /// 某版本的完整文件清单，检出时直接用。
    pub fn files_at(
        &self,
        session: &Session,
        name_or_id: &str,
        rev: Option<u64>,
    ) -> Result<(RepoInfo, u64, Vec<FlatEntry>)> {
        let repo = self.require_repo(session, name_or_id)?;
        let rev = rev.unwrap_or(repo.head_rev);
        let root = self.revision_root_tree(&repo.repo_id, rev)?;
        let flat = tree::flatten_tree(&self.objects, &root)?;
        Ok((repo, rev, flat))
    }

    pub fn read_file(
        &self,
        session: &Session,
        name_or_id: &str,
        rev: Option<u64>,
        path: &str,
    ) -> Result<Vec<u8>> {
        let (repo, rev, flat) = self.files_at(session, name_or_id, rev)?;
        let normalized = core_types::normalize_path(path)?;
        let entry = flat.iter().find(|e| e.path == normalized).ok_or_else(|| {
            Error::new(
                ErrorCode::ObjectMissing,
                format!("r{rev}（仓库 {}）中不存在路径 {normalized}", repo.name),
            )
        })?;
        self.objects.get(&entry.hash).map_err(|err| {
            if err.code == ErrorCode::ObjectMissing {
                // 树里还留着哈希，但内容已被容量策略清理：这是设计 9.4 预期的悬空引用，
                // 必须给出明确错误码，让客户端提示「该内容已按容量策略清理」而不是「仓库损坏」。
                Error::new(
                    ErrorCode::ContentPurged,
                    format!(
                        "r{rev}（仓库 {}）的 {normalized} 内容已按容量策略清理，无法再读取",
                        repo.name
                    ),
                )
            } else {
                err
            }
        })
    }

    // ---------------- 配额与用量 ----------------

    pub fn effective_quota(&self, repo_id: &str) -> Result<EffectiveQuota> {
        let repo_bytes: i64 = self
            .conn
            .query_row(
                "SELECT quota_bytes FROM repos WHERE repo_id = ?1",
                params![repo_id],
                |r| r.get(0),
            )
            .map_err(db("查询仓库配额失败"))?;
        let default_bytes = self.setting_u64("default_quota_bytes")?;
        Ok(core_types::resolve_effective_quota(
            repo_bytes.max(0) as u64,
            default_bytes,
        ))
    }

    pub fn set_default_quota(&self, session: &Session, quota_bytes: u64) -> Result<EffectiveQuota> {
        if session.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "仅管理员可修改全局配额",
            ));
        }
        let now = now_secs();
        self.conn
            .execute(
                "INSERT INTO system_settings (key, value, updated_at, updated_by)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                   updated_at = excluded.updated_at, updated_by = excluded.updated_by",
                params![
                    "default_quota_bytes",
                    quota_bytes.to_string(),
                    now,
                    session.user_id
                ],
            )
            .map_err(db("写入全局配额失败"))?;
        self.audit(
            "",
            &session.user_id,
            "quota.default.update",
            "",
            Some(&format!("{{\"quota\":{quota_bytes}}}")),
        )?;
        Ok(core_types::resolve_effective_quota(0, quota_bytes))
    }

    /// 全局默认配额（0 表示未配置，由内置兜底接管）。
    pub fn quota_defaults(&self) -> Result<u64> {
        self.setting_u64("default_quota_bytes")
    }

    /// 单仓库的显式配额上限（0 表示继承全局默认）。
    pub fn repo_quota_override(&self, repo_id: &str) -> Result<u64> {
        let quota: i64 = self
            .conn
            .query_row(
                "SELECT quota_bytes FROM repos WHERE repo_id = ?1",
                params![repo_id],
                |r| r.get(0),
            )
            .map_err(db("查询仓库配额失败"))?;
        Ok(quota.max(0) as u64)
    }

    /// 提交会话所属的仓库。`FinalizeCommit` 的响应要带 root_tree，而 `ImportOutcome`
    /// 只有版本号，需要靠会话把仓库找回来。
    pub fn session_repo_id(&self, session: &Session, session_id: &str) -> Result<String> {
        Ok(self.load_session(session, session_id)?.repo_id)
    }

    fn setting_u64(&self, key: &str) -> Result<u64> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM system_settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("读取系统设置失败"))?;
        Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    pub fn usage(&self, repo_id: &str) -> Result<UsageInfo> {
        let head_rev: i64 = self
            .conn
            .query_row(
                "SELECT head_rev FROM repos WHERE repo_id = ?1",
                params![repo_id],
                |r| r.get(0),
            )
            .map_err(db("查询仓库失败"))?;
        let root = self.revision_root_tree(repo_id, head_rev as u64)?;
        let flat = tree::flatten_tree(&self.objects, &root)?;
        let logical_bytes: u64 = flat.iter().map(|e| e.size).sum();

        // 共享 CAS 下这是「全库对象池」的实际内容字节：同一份内容被多个仓库共享时只算一次，
        // 也正是磁盘上真正被吃掉的空间。按仓库切分的 unique_bytes 需要 GC 标记阶段的
        // 反向引用集合（M2），这里不假装能算准。
        //
        // 直接读对象目录而不是 SUM(objects.stored_size)：索引表可能因为历史原因缺行，
        // 但磁盘上的字节数永远是事实。
        let object_count = self.objects.list_hashes()?.len() as u64;
        let used_bytes = self.objects.total_stored_bytes()?;

        Ok(UsageInfo {
            logical_bytes,
            used_bytes,
            object_count,
            quota: self.effective_quota(repo_id)?,
            file_count: flat.len() as u64,
        })
    }

    // ---------------- 用户体系：自助注册、审批、PAT ----------------

    /// 把到期的申请标记为 EXPIRED。
    ///
    /// 没有后台定时任务，所以是「惰性过期」：所有读写申请的入口先调一次。
    /// 否则会出现「列表显示 PENDING，点审批却提示已过期」的割裂感。
    fn expire_stale_requests(&self) -> Result<()> {
        let now = now_secs();
        self.conn
            .execute(
                "UPDATE registration_requests SET status = 'expired'
                 WHERE status = 'pending' AND expires_at <= ?1",
                params![now],
            )
            .map_err(db("过期注册申请失败"))?;
        // 账号状态跟着申请走：注册被过期处理，对应账号也不能停在 PENDING 骗用户等下去。
        self.conn
            .execute(
                "UPDATE users SET status = 'expired'
                 WHERE status = 'pending' AND username IN (
                     SELECT username FROM registration_requests WHERE status = 'expired')",
                [],
            )
            .map_err(db("过期待审批账号失败"))?;
        self.conn
            .execute(
                "UPDATE repo_creation_requests SET status = 'expired'
                 WHERE status = 'pending' AND expires_at <= ?1",
                params![now],
            )
            .map_err(db("过期仓库申请失败"))?;
        self.conn
            .execute(
                "UPDATE repo_quota_requests SET status = 'expired'
                 WHERE status = 'pending' AND expires_at <= ?1",
                params![now],
            )
            .map_err(db("过期扩容申请失败"))?;
        Ok(())
    }

    fn require_admin(&self, actor: &Session, action: &str) -> Result<()> {
        if actor.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                format!("仅管理员可{action}"),
            ));
        }
        actor.require(SCOPE_ADMIN, action)
    }

    /// 供 gRPC 层「先鉴权、再把网络操作丢进 spawn_blocking」时使用。
    pub fn require_admin_action(&self, actor: &Session, action: &str) -> Result<()> {
        self.require_admin(actor, action)
    }

    // ---------------- 目录认证（LDAP / AD） ----------------

    /// 读取完整目录配置（含解密后的服务账号密码）。**仅供服务端内部使用**。
    pub fn ldap_config(&self) -> Result<Option<LdapSettings>> {
        let row: Option<LdapConfigRow> = self
            .conn
            .query_row(
                "SELECT host, port, tls_mode, base_dn, bind_dn, bind_password_enc,
                        user_filter, group_filter, attr_username, attr_display_name, enabled
                 FROM ldap_config WHERE config_id = 'default'",
                [],
                |r| {
                    Ok(LdapConfigRow {
                        host: r.get(0)?,
                        port: r.get(1)?,
                        tls_mode: r.get(2)?,
                        base_dn: r.get(3)?,
                        bind_dn: r.get(4)?,
                        bind_password_enc: r.get(5)?,
                        user_filter: r.get(6)?,
                        group_filter: r.get(7)?,
                        attr_username: r.get(8)?,
                        attr_display_name: r.get(9)?,
                        enabled: r.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(db("读取 LDAP 配置失败"))?;
        let Some(row) = row else { return Ok(None) };
        let password = String::from_utf8(auth::open_secret(&self.secret, &row.bind_password_enc)?)
            .map_err(|_| Error::new(ErrorCode::IoError, "LDAP bind 密码不是合法 UTF-8"))?;
        Ok(Some(LdapSettings {
            host: row.host,
            port: row.port as u16,
            tls_mode: TlsMode::parse(&row.tls_mode)?,
            base_dn: row.base_dn,
            bind_dn: row.bind_dn,
            bind_password: password,
            user_filter: row.user_filter,
            group_filter: row.group_filter.unwrap_or_default(),
            attr_username: row.attr_username,
            attr_display_name: row.attr_display_name,
            enabled: row.enabled != 0,
            group_roles: self.ldap_group_roles()?,
        }))
    }

    pub fn ldap_group_roles(&self) -> Result<Vec<(String, Role)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT group_dn, role FROM ldap_group_roles ORDER BY group_dn")
            .map_err(db("查询 LDAP 组映射失败"))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db("查询 LDAP 组映射失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (dn, role) = row.map_err(db("解析 LDAP 组映射失败"))?;
            out.push((dn, parse_role(&role)?));
        }
        Ok(out)
    }

    /// 管理界面用的配置视图：`settings.bind_password` 恒为空串。
    /// `actor = None` 是本机 CLI 通道。
    pub fn ldap_config_public(&self, actor: Option<&Session>) -> Result<Option<LdapPublicConfig>> {
        if let Some(session) = actor {
            self.require_admin(session, "查看 LDAP 配置")?;
        }
        let Some(mut settings) = self.ldap_config()? else {
            return Ok(None);
        };
        let bind_password_set = !settings.bind_password.is_empty();
        settings.bind_password.clear();
        Ok(Some(LdapPublicConfig {
            settings,
            bind_password_set,
        }))
    }

    /// 保存目录配置。`actor = None` 是本机 CLI 通道（部署时还没有任何管理员），
    /// 走 gRPC 则必须是管理员；bind 密码加密后落库，明文从不写入数据库。
    pub fn set_ldap_config(&self, actor: Option<&Session>, cfg: &LdapSettings) -> Result<()> {
        if let Some(session) = actor {
            self.require_admin(session, "修改 LDAP 配置")?;
        }
        cfg.validate()?;
        let enc = auth::seal_secret(&self.secret, cfg.bind_password.as_bytes())?;
        let now = now_secs();
        let actor_id = actor.map(|s| s.user_id.as_str()).unwrap_or("cli");
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启 LDAP 配置事务失败"))?;
        tx.execute(
            "INSERT INTO ldap_config
               (config_id, host, port, tls_mode, base_dn, bind_dn, bind_password_enc,
                user_filter, group_filter, attr_username, attr_display_name, enabled,
                updated_at, updated_by)
             VALUES ('default', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(config_id) DO UPDATE SET
                host = excluded.host, port = excluded.port, tls_mode = excluded.tls_mode,
                base_dn = excluded.base_dn, bind_dn = excluded.bind_dn,
                bind_password_enc = excluded.bind_password_enc,
                user_filter = excluded.user_filter, group_filter = excluded.group_filter,
                attr_username = excluded.attr_username,
                attr_display_name = excluded.attr_display_name,
                enabled = excluded.enabled, updated_at = excluded.updated_at,
                updated_by = excluded.updated_by",
            params![
                cfg.host,
                cfg.port as i64,
                cfg.tls_mode.as_str(),
                cfg.base_dn,
                cfg.bind_dn,
                enc,
                cfg.user_filter,
                cfg.group_filter,
                cfg.attr_username,
                cfg.attr_display_name,
                i64::from(cfg.enabled),
                now,
                actor_id
            ],
        )
        .map_err(db("写入 LDAP 配置失败"))?;
        // 组映射整体替换：配置界面每次提交的是完整列表，避免留下幽灵规则。
        tx.execute("DELETE FROM ldap_group_roles", [])
            .map_err(db("清空 LDAP 组映射失败"))?;
        for (dn, role) in &cfg.group_roles {
            tx.execute(
                "INSERT INTO ldap_group_roles (group_dn, role, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(group_dn) DO UPDATE SET role = excluded.role,
                                                     updated_at = excluded.updated_at",
                params![dn, role_to_str(*role), now],
            )
            .map_err(db("写入 LDAP 组映射失败"))?;
        }
        write_audit(
            &tx,
            "",
            actor_id,
            "ldap.config.update",
            &cfg.host,
            Some(&format!(
                "{{\"enabled\":{},\"tls_mode\":\"{}\",\"groups\":{}}}",
                cfg.enabled,
                cfg.tls_mode.as_str(),
                cfg.group_roles.len()
            )),
            now,
        )?;
        tx.commit().map_err(db("提交 LDAP 配置失败"))?;
        Ok(())
    }

    /// 连通性测试：只连目录、不做任何本地账号变更。
    pub fn test_ldap_config(
        &self,
        actor: Option<&Session>,
        cfg: &LdapSettings,
    ) -> Result<LdapTestOutcome> {
        if let Some(session) = actor {
            self.require_admin(session, "测试 LDAP 连接")?;
        }
        test_ldap_connection(cfg)
    }

    /// 目录组 → 初始身份：命中多条取最高，没命中一律普通用户。
    fn role_for_groups(&self, tx: &Connection, groups: &[String]) -> Result<Role> {
        let mut stmt = tx
            .prepare("SELECT group_dn, role FROM ldap_group_roles")
            .map_err(db("查询 LDAP 组映射失败"))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db("查询 LDAP 组映射失败"))?;
        let mut best = Role::User;
        for row in rows {
            let (dn, role) = row.map_err(db("解析 LDAP 组映射失败"))?;
            if groups.iter().any(|g| g.eq_ignore_ascii_case(&dn)) {
                let candidate = parse_role(&role)?;
                if role_rank(candidate) > role_rank(best) {
                    best = candidate;
                }
            }
        }
        Ok(best)
    }

    /// 目录认证**成功之后**的本地落库：JIT 建号（或更新映射）+ 签发默认 12 小时 JWT。
    ///
    /// `profile` 由目录层二次 bind 验证过才传进来；本方法不碰网络，便于测试。
    /// 与自助注册共用 [`first_user_slot`]：两条入口谁先到，谁成为第一个管理员，
    /// 之后所有人都按普通规则处理。
    pub fn login_ldap(&self, profile: &DirectoryUser) -> Result<(String, UserInfo)> {
        self.login_ldap_with_ttl(profile, auth::DEFAULT_TOKEN_TTL_SECONDS)
            .map(|(token, _, user)| (token, user))
    }

    /// 同 [`Self::login_ldap`]，但按客户端「记住我」的选择指定 token 有效期。
    pub fn login_ldap_with_ttl(
        &self,
        profile: &DirectoryUser,
        ttl_seconds: i64,
    ) -> Result<(String, i64, UserInfo)> {
        validate_username(&profile.username)?;
        let display_name = profile.display_name.trim();
        if display_name.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "目录账号缺少姓名属性，无法创建本地映射",
            ));
        }
        let now = now_secs();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(db("开启目录登录事务失败"))?;

        let existing: Option<(String, String, String, String, i64)> = tx
            .query_row(
                "SELECT user_id, source, status, role, token_version
                 FROM users WHERE username = ?1",
                params![profile.username],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(db("查询目录账号映射失败"))?;

        let (user_id, role, token_version, created) = match existing {
            Some((user_id, source, status, role, token_version)) => {
                if source != "ldap" {
                    return Err(Error::new(
                        ErrorCode::RegistrationExists,
                        "该用户名已被本地账号占用，请改用本地登录",
                    ));
                }
                if status != "active" {
                    return Err(Error::new(
                        ErrorCode::AccountDisabled,
                        "该目录账号在本地已被禁用",
                    ));
                }
                // 角色以本地为准：目录组变动不该把引导管理员悄悄降级。
                tx.execute(
                    "UPDATE users SET display_name = ?2, ldap_dn = ?3, last_login_at = ?4
                     WHERE user_id = ?1",
                    params![user_id, display_name, profile.dn, now],
                )
                .map_err(db("更新目录账号映射失败"))?;
                (user_id, parse_role(&role)?, token_version, false)
            }
            None => {
                let first = first_user_slot(&tx)?;
                let role = if first {
                    Role::Admin
                } else {
                    self.role_for_groups(&tx, &profile.groups)?
                };
                let user_id = new_id("u");
                tx.execute(
                    "INSERT INTO users (user_id, username, display_name, source, status, role,
                                        password_hash, ldap_dn, token_version, created_at, approved_at)
                     VALUES (?1, ?2, ?3, 'ldap', 'active', ?4, NULL, ?5, 1, ?6, ?6)",
                    params![
                        user_id,
                        profile.username,
                        display_name,
                        role_to_str(role),
                        profile.dn,
                        now
                    ],
                )
                .map_err(|e| {
                    map_unique_as(e, ErrorCode::RegistrationExists, "用户名已被占用")
                })?;
                write_audit(
                    &tx,
                    "",
                    &user_id,
                    if first {
                        "user.first_admin.ldap"
                    } else {
                        "user.ldap.jit"
                    },
                    &profile.username,
                    Some(&format!(
                        "{{\"role\":\"{}\",\"first_user\":{first}}}",
                        role_to_str(role)
                    )),
                    now,
                )?;
                (user_id, role, 1, true)
            }
        };

        let token = self.signer.sign_with_ttl(
            &user_id,
            &profile.username,
            role_to_str(role),
            token_version,
            now,
            ttl_seconds,
        )?;
        write_audit(
            &tx,
            "",
            &user_id,
            "user.login_ldap",
            &profile.username,
            Some(&format!("{{\"created\":{created}}}")),
            now,
        )?;
        tx.commit().map_err(db("提交目录登录事务失败"))?;

        Ok((
            token,
            now + ttl_seconds,
            UserInfo {
                user_id,
                username: profile.username.clone(),
                display_name: display_name.to_string(),
                role,
                status: core_types::AccountStatus::Active,
            },
        ))
    }

    /// 自助注册（免认证）。用户名、姓名、密码与理由一次交齐。
    ///
    /// **密码在这里就设置并落库**：审批通过后本人直接用它登录，服务端不再
    /// 发放一次性设置密码链接，也不需要在「激活」环节再收集一次密码。
    ///
    /// 两条分支共用同一个 `IMMEDIATE` 事务：
    ///   - **全库第一个账号**：自动批准为管理员，注册后即可登录；
    ///   - 其余：仍是 PENDING 申请，等管理员审批；未激活前密码不可用于登录。
    ///
    /// 首位判定放在写事务内（见 [`first_user_slot`]），CLI 与 gRPC 即使跨进程
    /// 同时引导，也只有一个请求能拿到管理员名额。
    pub fn register(
        &self,
        username: &str,
        display_name: &str,
        password: &str,
        reason: &str,
    ) -> Result<RegisterOutcome> {
        validate_username(username)?;
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.chars().count() > 30 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "姓名不能为空，且不超过 30 个字符",
            ));
        }
        // 密码策略与管理员建号共用同一份校验：服务端是安全边界，客户端提示只是体验。
        let issues = core_types::validate_password(password);
        if !issues.is_empty() {
            return Err(Error::new(
                ErrorCode::PasswordPolicyViolation,
                format!("密码不满足策略：{}", describe_password_issues(&issues)),
            ));
        }
        if reason.chars().count() > 200 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "申请理由不超过 200 个字符",
            ));
        }
        self.expire_stale_requests()?;

        // 用户名冲突分三种情况，客户端据此给出可操作的提示，而不是笼统的「已存在」。
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT source FROM users WHERE username = ?1",
                params![username],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询用户失败"))?;
        if let Some(source) = existing {
            return Err(Error::new(
                ErrorCode::RegistrationExists,
                if source == "ldap" {
                    "该用户名属于目录账号，无需注册；请走 LDAP 登录"
                } else {
                    "用户名已被占用"
                },
            ));
        }
        let pending: Option<String> = self
            .conn
            .query_row(
                "SELECT request_id FROM registration_requests
                 WHERE username = ?1 AND status = 'pending'",
                params![username],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询注册申请失败"))?;
        if pending.is_some() {
            return Err(Error::new(
                ErrorCode::RegistrationExists,
                "该用户名已有待审批的注册申请，请等待管理员处理",
            ));
        }

        let now = now_secs();
        let user_id = new_id("usr");
        let request_id = new_id("reg");
        let expires_at = now + REGISTRATION_TTL_SECONDS;
        // Argon2id 很贵：放在事务外算，别把写锁握在 KDF 上。
        let phc = auth::hash_password(password)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(db("开启注册事务失败"))?;

        // 首位用户在这一刻「占住」名额：账号直接 ACTIVE + admin，申请单直接 approved。
        // 密码在注册时就已设好，因此首个账号注册完即可登录。
        let first = first_user_slot(&tx)?;
        let (status, role, request_state, decided_by, decision_note) = if first {
            (
                "active",
                "admin",
                "approved",
                Some("system"),
                Some("系统自动批准：全库第一个账号"),
            )
        } else {
            // 待审批账号先按普通用户落库；审批只是把状态转 ACTIVE，不改身份。
            ("pending", "user", "pending", None, None)
        };

        tx.execute(
            "INSERT INTO users (user_id, username, display_name, source, status, role,
                                password_hash, token_version, created_at, approved_at)
             VALUES (?1, ?2, ?3, 'local', ?4, ?5, ?6, 1, ?7, ?8)",
            params![
                user_id,
                username,
                display_name,
                status,
                role,
                phc,
                now,
                if first { Some(now) } else { None }
            ],
        )
        .map_err(|e| map_unique_as(e, ErrorCode::RegistrationExists, "用户名已被占用"))?;
        tx.execute(
            "INSERT INTO registration_requests
               (request_id, username, display_name, reason, status, submitted_at, expires_at,
                decided_at, decided_by, decision_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                request_id,
                username,
                display_name,
                if reason.is_empty() {
                    None
                } else {
                    Some(reason)
                },
                request_state,
                now,
                expires_at,
                if first { Some(now) } else { None },
                decided_by,
                decision_note
            ],
        )
        .map_err(db("写入注册申请失败"))?;
        write_audit(
            &tx,
            "",
            &user_id,
            if first {
                "user.first_admin.register"
            } else {
                "user.register"
            },
            username,
            Some(&format!(
                "{{\"request_id\":\"{request_id}\",\"first_user\":{first}}}"
            )),
            now,
        )?;
        tx.commit().map_err(db("提交注册事务失败"))?;

        Ok(RegisterOutcome {
            info: RegistrationInfo {
                request_id,
                username: username.to_string(),
                display_name: display_name.to_string(),
                reason: if reason.is_empty() {
                    None
                } else {
                    Some(reason.to_string())
                },
                state: if first {
                    core_types::RegistrationStatus::Approved
                } else {
                    core_types::RegistrationStatus::Pending
                },
                submitted_at: now,
                decided_at: if first { Some(now) } else { None },
                decided_by: decided_by.map(str::to_string),
                decision_note: decision_note.map(str::to_string),
                expires_at,
            },
            first_user: first,
        })
    }

    pub fn list_registrations(
        &self,
        actor: &Session,
        filter: Option<core_types::RegistrationStatus>,
        limit: usize,
    ) -> Result<Vec<RegistrationInfo>> {
        self.require_admin(actor, "查看注册申请")?;
        self.expire_stale_requests()?;
        let filter = filter.map(reg_status_str).unwrap_or("");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT request_id, username, display_name, reason, status, submitted_at,
                        decided_at, decided_by, decision_note, expires_at
                 FROM registration_requests
                 WHERE (?1 = '' OR status = ?1)
                 ORDER BY submitted_at DESC LIMIT ?2",
            )
            .map_err(db("查询注册申请失败"))?;
        let rows = stmt
            .query_map(params![filter, limit.max(1) as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            })
            .map_err(db("查询注册申请失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (
                request_id,
                username,
                display_name,
                reason,
                status,
                submitted_at,
                decided_at,
                decided_by,
                decision_note,
                expires_at,
            ) = row.map_err(db("查询注册申请失败"))?;
            out.push(RegistrationInfo {
                request_id,
                username,
                display_name,
                reason,
                state: parse_reg_status(&status)?,
                submitted_at,
                decided_at,
                decided_by,
                decision_note,
                expires_at,
            });
        }
        Ok(out)
    }

    /// 审批通过：账号转 ACTIVE。密码在注册时已由本人设好，审批只是放行。
    ///
    /// 审批**不选类型**：通过即普通用户。读写权限按仓库授予，管理员身份只能由管理员
    /// 事后在用户页调整（`admin_set_user_role`），不在审批时顺手发放。
    pub fn approve_registration(
        &self,
        actor: &Session,
        request_id: &str,
    ) -> Result<ApprovedRegistration> {
        self.require_admin(actor, "审批注册申请")?;
        self.expire_stale_requests()?;
        let (username, state) = self.load_registration(request_id)?;
        if state != core_types::RegistrationStatus::Pending {
            return Err(Error::new(
                ErrorCode::ApprovalNotFound,
                format!("该申请已处理（{}），不能重复审批", reg_status_str(state)),
            ));
        }
        let (user_id, password_hash): (String, Option<String>) = self
            .conn
            .query_row(
                "SELECT user_id, password_hash FROM users
                 WHERE username = ?1 AND status = 'pending'",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db("查询待审批账号失败"))?
            .ok_or_else(|| Error::new(ErrorCode::ApprovalNotFound, "对应的待审批账号不存在"))?;
        // 旧流程（注册不收密码）留下的申请没有口令，审批后也登不进去；
        // 与其放行一个死账号，不如让管理员明确要求对方重新申请。
        if password_hash.is_none() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "该申请是旧流程提交的，没有设置密码；请驳回后让对方重新申请",
            ));
        }

        let now = now_secs();

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        // 审批通过同时递增 token_version：即使之前有人拿到过该账号的任何凭证，也立即失效。
        tx.execute(
            "UPDATE users SET status = 'active', role = 'user', approved_by = ?2, approved_at = ?3,
                              token_version = token_version + 1
             WHERE user_id = ?1",
            params![user_id, actor.user_id, now],
        )
        .map_err(db("激活账号失败"))?;
        tx.execute(
            "UPDATE registration_requests
             SET status = 'approved', decided_at = ?2, decided_by = ?3
             WHERE request_id = ?1",
            params![request_id, now, actor.user_id],
        )
        .map_err(db("更新注册申请失败"))?;
        write_audit(
            &tx,
            "",
            &actor.user_id,
            "user.approve",
            &username,
            Some("{\"role\":\"user\"}"),
            now,
        )?;
        tx.commit().map_err(db("提交审批事务失败"))?;

        Ok(ApprovedRegistration { user_id })
    }

    /// 拒绝申请必须写原因：用户需要知道为什么被拒，否则只会反复重试。
    pub fn reject_registration(
        &self,
        actor: &Session,
        request_id: &str,
        reason: &str,
    ) -> Result<()> {
        self.require_admin(actor, "驳回注册申请")?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::new(ErrorCode::InvalidArgument, "驳回必须填写原因"));
        }
        self.expire_stale_requests()?;
        let (username, state) = self.load_registration(request_id)?;
        if state != core_types::RegistrationStatus::Pending {
            return Err(Error::new(
                ErrorCode::ApprovalNotFound,
                format!("该申请已处理（{}），不能重复处理", reg_status_str(state)),
            ));
        }
        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "UPDATE users SET status = 'rejected' WHERE username = ?1 AND status = 'pending'",
            params![username],
        )
        .map_err(db("更新账号状态失败"))?;
        tx.execute(
            "UPDATE registration_requests
             SET status = 'rejected', decided_at = ?2, decided_by = ?3, decision_note = ?4
             WHERE request_id = ?1",
            params![request_id, now, actor.user_id, reason],
        )
        .map_err(db("更新注册申请失败"))?;
        write_audit(
            &tx,
            "",
            &actor.user_id,
            "user.reject",
            &username,
            Some(&format!("{{\"reason\":\"{}\"}}", json_escape(reason))),
            now,
        )?;
        tx.commit().map_err(db("提交驳回事务失败"))?;
        Ok(())
    }

    fn load_registration(
        &self,
        request_id: &str,
    ) -> Result<(String, core_types::RegistrationStatus)> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT username, status FROM registration_requests WHERE request_id = ?1",
                params![request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db("查询注册申请失败"))?;
        let (username, status) =
            row.ok_or_else(|| Error::new(ErrorCode::ApprovalNotFound, "注册申请不存在"))?;
        Ok((username, parse_reg_status(&status)?))
    }

    // ---------------- 仓库创建申请与审批 ----------------

    /// 提交仓库创建申请。任何 ACTIVE 用户都可以申请，但必须管理员审批后才分配 repo_id。
    ///
    /// 容量不由申请人填写：一律按 `vcs_core::REPO_INITIAL_QUOTA_BYTES`（10 GiB）落库，
    /// 管理员审批时也只回答「同意 / 拒绝」，不再核定数字。空间不够走扩容申请。
    pub fn request_repo(
        &self,
        actor: &Session,
        repo_name: &str,
        reason: &str,
    ) -> Result<RepoRequestInfo> {
        validate_repo_name(repo_name)?;
        if reason.chars().count() > 200 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "用途说明不超过 200 个字符",
            ));
        }
        let requested_quota_bytes = core_types::REPO_INITIAL_QUOTA_BYTES;
        self.expire_stale_requests()?;

        let taken: Option<String> = self
            .conn
            .query_row(
                "SELECT repo_id FROM repos WHERE name = ?1",
                params![repo_name],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询仓库失败"))?;
        if taken.is_some() {
            return Err(Error::new(
                ErrorCode::RepoCreationExists,
                "该仓库名已被占用",
            ));
        }
        let pending: Option<String> = self
            .conn
            .query_row(
                "SELECT request_id FROM repo_creation_requests
                 WHERE repo_name = ?1 AND status = 'pending'",
                params![repo_name],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询仓库申请失败"))?;
        if pending.is_some() {
            return Err(Error::new(
                ErrorCode::RepoCreationExists,
                "已有同名仓库的待审批申请",
            ));
        }

        let now = now_secs();
        let request_id = new_id("rq");
        let expires_at = now + REPO_REQUEST_TTL_SECONDS;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "INSERT INTO repo_creation_requests
               (request_id, requester_user_id, repo_name, reason, requested_quota_bytes,
                status, submitted_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
            params![
                request_id,
                actor.user_id,
                repo_name,
                reason,
                requested_quota_bytes,
                now,
                expires_at
            ],
        )
        .map_err(db("写入仓库申请失败"))?;
        write_audit(
            &tx,
            "",
            &actor.user_id,
            "repo.request",
            repo_name,
            Some(&format!(
                "{{\"request_id\":\"{request_id}\",\"quota\":{requested_quota_bytes}}}"
            )),
            now,
        )?;
        tx.commit().map_err(db("提交仓库申请事务失败"))?;

        Ok(RepoRequestInfo {
            request_id,
            requester_user_id: actor.user_id.clone(),
            repo_name: repo_name.to_string(),
            reason: if reason.is_empty() {
                None
            } else {
                Some(reason.to_string())
            },
            requested_quota_bytes,
            state: core_types::RepoCreationStatus::Pending,
            submitted_at: now,
            decided_at: None,
            decided_by: None,
            decision_note: None,
            expires_at,
            approved_repo_id: None,
        })
    }

    /// 列出仓库申请。管理员看全部，普通用户只看自己的——避免泄露别人的申请状态。
    pub fn list_repo_requests(
        &self,
        actor: &Session,
        filter: Option<core_types::RepoCreationStatus>,
        limit: usize,
    ) -> Result<Vec<RepoRequestInfo>> {
        self.expire_stale_requests()?;
        let filter = filter.map(creation_status_str).unwrap_or("");
        let mine = if actor.role == Role::Admin {
            ""
        } else {
            actor.user_id.as_str()
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT request_id, requester_user_id, repo_name, reason, requested_quota_bytes,
                        status, submitted_at, decided_at, decided_by, decision_note,
                        expires_at, approved_repo_id
                 FROM repo_creation_requests
                 WHERE (?1 = '' OR status = ?1)
                   AND (?2 = '' OR requester_user_id = ?2)
                 ORDER BY submitted_at DESC LIMIT ?3",
            )
            .map_err(db("查询仓库申请失败"))?;
        let rows = stmt
            .query_map(params![filter, mine, limit.max(1) as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, Option<String>>(11)?,
                ))
            })
            .map_err(db("查询仓库申请失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (
                request_id,
                requester_user_id,
                repo_name,
                reason,
                requested_quota_bytes,
                status,
                submitted_at,
                decided_at,
                decided_by,
                decision_note,
                expires_at,
                approved_repo_id,
            ) = row.map_err(db("查询仓库申请失败"))?;
            out.push(RepoRequestInfo {
                request_id,
                requester_user_id,
                repo_name,
                reason,
                requested_quota_bytes: requested_quota_bytes.max(0) as u64,
                state: parse_creation_status(&status)?,
                submitted_at,
                decided_at,
                decided_by,
                decision_note,
                expires_at,
                approved_repo_id,
            });
        }
        Ok(out)
    }

    /// 审批通过：单事务内建仓库 + 登记申请人 owner + 写入初始空版本 r0。
    ///
    /// 容量取自申请单上服务端写死的值（10 GiB），不接受审批人再填数字；
    /// 审批人在这里只有「通过 / 驳回」两个动作。
    pub fn approve_repo_request(
        &self,
        actor: &Session,
        request_id: &str,
        decision_note: &str,
    ) -> Result<RepoInfo> {
        self.require_admin(actor, "审批仓库申请")?;
        self.expire_stale_requests()?;

        // 连申请人用户名一起取出来：初始空版本 r0 的作者应该是仓库的主人，而不是审批人。
        let (requester, requester_name, repo_name, state, requested_quota_bytes): (
            String,
            String,
            String,
            String,
            i64,
        ) = self
            .conn
            .query_row(
                "SELECT q.requester_user_id, COALESCE(u.username, ''),
                        q.repo_name, q.status, q.requested_quota_bytes
                 FROM repo_creation_requests q
                 LEFT JOIN users u ON u.user_id = q.requester_user_id
                 WHERE q.request_id = ?1",
                params![request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(db("查询仓库申请失败"))?
            .ok_or_else(|| Error::new(ErrorCode::RepoApprovalNotFound, "仓库申请不存在"))?;
        if parse_creation_status(&state)? != core_types::RepoCreationStatus::Pending {
            return Err(Error::new(
                ErrorCode::RepoApprovalNotFound,
                format!("该申请已处理（{state}），不能重复审批"),
            ));
        }
        validate_repo_name(&repo_name)?;
        // 老库里的申请单可能没写过容量，按固定初始配额补齐，而不是退回「继承默认」。
        let quota_bytes = if requested_quota_bytes > 0 {
            requested_quota_bytes as u64
        } else {
            core_types::REPO_INITIAL_QUOTA_BYTES
        };

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        let repo_id = self.insert_new_repo(
            &tx,
            &repo_name,
            &requester,
            quota_bytes,
            Some(request_id),
            &requester_name,
            "repo.approve",
        )?;
        tx.execute(
            "UPDATE repo_creation_requests
             SET status = 'approved', decided_at = ?2, decided_by = ?3,
                 decision_note = ?4, approved_repo_id = ?5
             WHERE request_id = ?1",
            params![
                request_id,
                now_secs(),
                actor.user_id,
                decision_note,
                repo_id
            ],
        )
        .map_err(db("更新仓库申请失败"))?;
        tx.commit().map_err(db("提交审批事务失败"))?;

        Ok(RepoInfo {
            repo_id,
            name: repo_name,
            head_rev: 0,
            status: RepoStatus::Active,
            owner_user_id: requester,
        })
    }

    pub fn reject_repo_request(
        &self,
        actor: &Session,
        request_id: &str,
        reason: &str,
    ) -> Result<()> {
        self.require_admin(actor, "驳回仓库申请")?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::new(ErrorCode::InvalidArgument, "驳回必须填写原因"));
        }
        self.expire_stale_requests()?;
        let state: String = self
            .conn
            .query_row(
                "SELECT status FROM repo_creation_requests WHERE request_id = ?1",
                params![request_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询仓库申请失败"))?
            .ok_or_else(|| Error::new(ErrorCode::RepoApprovalNotFound, "仓库申请不存在"))?;
        if parse_creation_status(&state)? != core_types::RepoCreationStatus::Pending {
            return Err(Error::new(
                ErrorCode::RepoApprovalNotFound,
                format!("该申请已处理（{state}），不能重复处理"),
            ));
        }
        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "UPDATE repo_creation_requests
             SET status = 'rejected', decided_at = ?2, decided_by = ?3, decision_note = ?4
             WHERE request_id = ?1",
            params![request_id, now, actor.user_id, reason],
        )
        .map_err(db("更新仓库申请失败"))?;
        write_audit(
            &tx,
            "",
            &actor.user_id,
            "repo.reject",
            request_id,
            Some(&format!("{{\"reason\":\"{}\"}}", json_escape(reason))),
            now,
        )?;
        tx.commit().map_err(db("提交驳回事务失败"))?;
        Ok(())
    }

    // ---------------- 仓库扩容申请与审批 ----------------

    /// 提交扩容申请。
    ///
    /// 申请人只表达「要扩容」，不填任何数字：增量由服务端固定按
    /// `REPO_QUOTA_EXPANSION_STEP_BYTES`（10 GiB）计算。同一仓库同时只允许一条
    /// 待审批申请，避免连点两下就把上限抬两档。
    pub fn request_repo_expansion(
        &self,
        actor: &Session,
        repo_id: &str,
        reason: &str,
    ) -> Result<RepoQuotaRequestInfo> {
        if reason.chars().count() > 200 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "扩容说明不超过 200 个字符",
            ));
        }
        self.expire_stale_requests()?;
        // 成员门 + 仓库状态门：不是成员就不能替别人的仓库要空间。
        let repo = self.require_repo(actor, repo_id)?;
        // 扩容是拥有者的事：只读 / 读写成员只能看容量，不能申请加空间。
        // 系统身份（内置 Web 管理端代理管理员）不在这里受限，审批与代提另走管理通道。
        if !actor.is_system()
            && self.member_role(&repo.repo_id, &actor.user_id)? != RepoMemberRole::Owner
        {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "只有仓库拥有者可以申请扩容，成员只能查看容量",
            ));
        }
        let pending: Option<String> = self
            .conn
            .query_row(
                "SELECT request_id FROM repo_quota_requests
                 WHERE repo_id = ?1 AND status = 'pending'",
                params![repo.repo_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询扩容申请失败"))?;
        if pending.is_some() {
            return Err(Error::new(
                ErrorCode::RepoCreationExists,
                "该仓库已有待审批的扩容申请，请等待管理员处理",
            ));
        }

        let increment_bytes = core_types::REPO_QUOTA_EXPANSION_STEP_BYTES;
        let quota_before_bytes = self.effective_quota(&repo.repo_id)?.bytes;
        let now = now_secs();
        let request_id = new_id("eq");
        let expires_at = now + REPO_REQUEST_TTL_SECONDS;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "INSERT INTO repo_quota_requests
               (request_id, repo_id, requester_user_id, increment_bytes, quota_before_bytes,
                reason, status, submitted_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
            params![
                request_id,
                repo.repo_id,
                actor.user_id,
                increment_bytes as i64,
                quota_before_bytes as i64,
                reason,
                now,
                expires_at
            ],
        )
        .map_err(db("写入扩容申请失败"))?;
        write_audit(
            &tx,
            &repo.repo_id,
            &actor.user_id,
            "repo.expand.request",
            &repo.name,
            Some(&format!(
                "{{\"request_id\":\"{request_id}\",\"increment\":{increment_bytes}}}"
            )),
            now,
        )?;
        tx.commit().map_err(db("提交扩容申请事务失败"))?;

        Ok(RepoQuotaRequestInfo {
            request_id,
            repo_id: repo.repo_id,
            repo_name: repo.name,
            requester_user_id: actor.user_id.clone(),
            increment_bytes,
            quota_before_bytes,
            reason: if reason.is_empty() {
                None
            } else {
                Some(reason.to_string())
            },
            state: core_types::RepoCreationStatus::Pending,
            submitted_at: now,
            decided_at: None,
            decided_by: None,
            decision_note: None,
            expires_at,
        })
    }

    /// 列出扩容申请。管理员看全部，普通用户只看自己的。
    pub fn list_repo_quota_requests(
        &self,
        actor: &Session,
        filter: Option<core_types::RepoCreationStatus>,
        limit: usize,
    ) -> Result<Vec<RepoQuotaRequestInfo>> {
        self.expire_stale_requests()?;
        let filter = filter.map(creation_status_str).unwrap_or("");
        let mine = if actor.role == Role::Admin {
            ""
        } else {
            actor.user_id.as_str()
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT q.request_id, q.repo_id, COALESCE(r.name, ''), q.requester_user_id,
                        q.increment_bytes, q.quota_before_bytes, q.reason, q.status,
                        q.submitted_at, q.decided_at, q.decided_by, q.decision_note, q.expires_at
                 FROM repo_quota_requests q
                 LEFT JOIN repos r ON r.repo_id = q.repo_id
                 WHERE (?1 = '' OR q.status = ?1)
                   AND (?2 = '' OR q.requester_user_id = ?2)
                 ORDER BY q.submitted_at DESC LIMIT ?3",
            )
            .map_err(db("查询扩容申请失败"))?;
        let rows = stmt
            .query_map(params![filter, mine, limit.max(1) as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, i64>(12)?,
                ))
            })
            .map_err(db("查询扩容申请失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (
                request_id,
                repo_id,
                repo_name,
                requester_user_id,
                increment_bytes,
                quota_before_bytes,
                reason,
                status,
                submitted_at,
                decided_at,
                decided_by,
                decision_note,
                expires_at,
            ) = row.map_err(db("查询扩容申请失败"))?;
            out.push(RepoQuotaRequestInfo {
                request_id,
                repo_id,
                repo_name,
                requester_user_id,
                increment_bytes: increment_bytes.max(0) as u64,
                quota_before_bytes: quota_before_bytes.max(0) as u64,
                reason,
                state: parse_creation_status(&status)?,
                submitted_at,
                decided_at,
                decided_by,
                decision_note,
                expires_at,
            });
        }
        Ok(out)
    }

    /// 审批扩容申请：把仓库上限提到「当前生效值 + 固定增量」。
    ///
    /// 审批人真正要判断的是机器磁盘还剩多少，所以这里先探测数据卷：
    /// 连这一次增量都放不下时直接拒绝审批——批了也写不进去。
    pub fn approve_repo_quota_request(
        &self,
        actor: &Session,
        request_id: &str,
        decision_note: &str,
    ) -> Result<EffectiveQuota> {
        self.require_admin(actor, "审批扩容申请")?;
        self.expire_stale_requests()?;
        let (repo_id, state, increment_bytes): (String, String, i64) = self
            .conn
            .query_row(
                "SELECT repo_id, status, increment_bytes FROM repo_quota_requests
                 WHERE request_id = ?1",
                params![request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(db("查询扩容申请失败"))?
            .ok_or_else(|| Error::new(ErrorCode::RepoApprovalNotFound, "扩容申请不存在"))?;
        if parse_creation_status(&state)? != core_types::RepoCreationStatus::Pending {
            return Err(Error::new(
                ErrorCode::RepoApprovalNotFound,
                format!("该扩容申请已处理（{state}），不能重复审批"),
            ));
        }
        let repo = self.find_repo(&repo_id)?;
        let increment = if increment_bytes > 0 {
            increment_bytes as u64
        } else {
            core_types::REPO_QUOTA_EXPANSION_STEP_BYTES
        };
        let capacity = self.storage_capacity()?;
        if capacity.available_bytes < increment {
            return Err(Error::new(
                ErrorCode::QuotaExceeded,
                format!(
                    "数据盘可用空间不足：可用 {}，本次扩容需要 {}；请先清理空间再审批",
                    human_bytes(capacity.available_bytes),
                    human_bytes(increment)
                ),
            ));
        }

        // 目标值按「审批这一刻」的生效上限重算：期间管理员若调整过配额，
        // 申请单上的快照只用于展示，不参与计算。
        let target = self
            .effective_quota(&repo.repo_id)?
            .bytes
            .saturating_add(increment);
        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "UPDATE repos SET quota_bytes = ?2, updated_at = ?3 WHERE repo_id = ?1",
            params![repo.repo_id, target as i64, now],
        )
        .map_err(db("更新仓库配额失败"))?;
        tx.execute(
            "UPDATE repo_quota_requests
             SET status = 'approved', decided_at = ?2, decided_by = ?3, decision_note = ?4
             WHERE request_id = ?1",
            params![request_id, now, actor.user_id, decision_note],
        )
        .map_err(db("更新扩容申请失败"))?;
        write_audit(
            &tx,
            &repo.repo_id,
            &actor.user_id,
            "repo.expand.approve",
            &repo.name,
            Some(&format!(
                "{{\"request_id\":\"{request_id}\",\"quota\":{target}}}"
            )),
            now,
        )?;
        tx.commit().map_err(db("提交扩容审批事务失败"))?;
        Ok(core_types::resolve_effective_quota(target, 0))
    }

    /// 驳回扩容申请（仅管理员，必须写原因）。
    pub fn reject_repo_quota_request(
        &self,
        actor: &Session,
        request_id: &str,
        reason: &str,
    ) -> Result<()> {
        self.require_admin(actor, "驳回扩容申请")?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::new(ErrorCode::InvalidArgument, "驳回必须填写原因"));
        }
        self.expire_stale_requests()?;
        let (repo_id, state): (String, String) = self
            .conn
            .query_row(
                "SELECT repo_id, status FROM repo_quota_requests WHERE request_id = ?1",
                params![request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db("查询扩容申请失败"))?
            .ok_or_else(|| Error::new(ErrorCode::RepoApprovalNotFound, "扩容申请不存在"))?;
        if parse_creation_status(&state)? != core_types::RepoCreationStatus::Pending {
            return Err(Error::new(
                ErrorCode::RepoApprovalNotFound,
                format!("该扩容申请已处理（{state}），不能重复处理"),
            ));
        }
        let now = now_secs();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "UPDATE repo_quota_requests
             SET status = 'rejected', decided_at = ?2, decided_by = ?3, decision_note = ?4
             WHERE request_id = ?1",
            params![request_id, now, actor.user_id, reason],
        )
        .map_err(db("更新扩容申请失败"))?;
        write_audit(
            &tx,
            &repo_id,
            &actor.user_id,
            "repo.expand.reject",
            request_id,
            Some(&format!("{{\"reason\":\"{}\"}}", json_escape(reason))),
            now,
        )?;
        tx.commit().map_err(db("提交驳回事务失败"))?;
        Ok(())
    }

    /// 数据卷容量：管理端审批扩容时展示的「机器磁盘可用空间」。
    pub fn storage_capacity(&self) -> Result<StorageCapacity> {
        disk_capacity(&self.root)
    }

    // ---------------- PAT ----------------

    /// 创建访问令牌。明文只返回这一次，库里只存 blake3 摘要。
    pub fn create_pat(
        &self,
        actor: &Session,
        name: &str,
        scopes: &[String],
        expires_at: Option<i64>,
    ) -> Result<(String, String)> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "令牌名称不能为空，且不超过 64 个字符",
            ));
        }
        let now = now_secs();
        if let Some(exp) = expires_at {
            if exp <= now {
                return Err(Error::new(
                    ErrorCode::InvalidArgument,
                    "过期时间必须晚于当前时间",
                ));
            }
        }
        // 未指定 scope 时给最小权限：默认只读，避免 CI 令牌悄悄带上写能力。
        let mut chosen: Vec<String> = if scopes.is_empty() {
            vec![SCOPE_READ.to_string()]
        } else {
            scopes.to_vec()
        };
        chosen.sort();
        chosen.dedup();
        for scope in &chosen {
            if !ALLOWED_SCOPES.contains(&scope.as_str()) {
                return Err(Error::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "未知的 scope：{scope}（可选 {}）",
                        ALLOWED_SCOPES.join(" / ")
                    ),
                ));
            }
        }
        // 不能签发超出本人权限的令牌，否则普通用户能给自己发一张 admin 令牌。
        if chosen.iter().any(|s| s == SCOPE_ADMIN) && actor.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "只有管理员可以创建带 admin 权限的令牌",
            ));
        }

        let token = format!("{PAT_PREFIX}{}", auth::random_token(32));
        let token_hash = to_hex(&auth::token_digest(&token));
        let token_id = new_id("pat");
        let scopes_json = serde_json::to_string(&chosen)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("序列化 scope 失败：{e}")))?;

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db("开启事务失败"))?;
        tx.execute(
            "INSERT INTO pat_tokens (token_id, user_id, name, token_hash, scopes, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                token_id,
                actor.user_id,
                name,
                token_hash,
                scopes_json,
                now,
                expires_at
            ],
        )
        .map_err(db("写入访问令牌失败"))?;
        write_audit(
            &tx,
            "",
            &actor.user_id,
            "pat.create",
            &token_id,
            Some(&format!("{{\"scopes\":{scopes_json}}}")),
            now,
        )?;
        tx.commit().map_err(db("提交令牌事务失败"))?;

        Ok((token, token_id))
    }

    pub fn list_pats(&self, actor: &Session) -> Result<Vec<PatInfo>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT token_id, name, scopes, created_at, expires_at, revoked_at, last_used_at
                 FROM pat_tokens WHERE user_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(db("查询访问令牌失败"))?;
        let rows = stmt
            .query_map(params![actor.user_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                ))
            })
            .map_err(db("查询访问令牌失败"))?;
        let mut out = Vec::new();
        for row in rows {
            let (token_id, name, scopes, created_at, expires_at, revoked_at, last_used_at) =
                row.map_err(db("查询访问令牌失败"))?;
            out.push(PatInfo {
                token_id,
                name,
                scopes: serde_json::from_str(&scopes).unwrap_or_default(),
                created_at,
                expires_at,
                revoked_at,
                last_used_at,
            });
        }
        Ok(out)
    }

    pub fn revoke_pat(&self, actor: &Session, token_id: &str) -> Result<()> {
        let owner: Option<String> = self
            .conn
            .query_row(
                "SELECT user_id FROM pat_tokens WHERE token_id = ?1",
                params![token_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询访问令牌失败"))?;
        let owner =
            owner.ok_or_else(|| Error::new(ErrorCode::InvalidArgument, "访问令牌不存在"))?;
        if owner != actor.user_id && actor.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "不能撤销他人的访问令牌",
            ));
        }
        // 撤销是幂等的：重复撤销不报错，但保留首次撤销时间。
        let now = now_secs();
        self.conn
            .execute(
                "UPDATE pat_tokens SET revoked_at = COALESCE(revoked_at, ?2) WHERE token_id = ?1",
                params![token_id, now],
            )
            .map_err(db("撤销访问令牌失败"))?;
        self.audit("", &actor.user_id, "pat.revoke", token_id, None)?;
        Ok(())
    }

    /// PAT 鉴权：按摘要查表 → 校验撤销 / 过期 / 账号状态 → 记录使用时间。
    fn authenticate_pat(&self, token: &str) -> Result<Session> {
        let digest = to_hex(&auth::token_digest(token));
        let row: Option<PatRow> = self
            .conn
            .query_row(
                "SELECT token_id, user_id, expires_at, revoked_at, scopes
                 FROM pat_tokens WHERE token_hash = ?1",
                params![digest],
                |r| {
                    Ok(PatRow {
                        token_id: r.get(0)?,
                        user_id: r.get(1)?,
                        expires_at: r.get(2)?,
                        revoked_at: r.get(3)?,
                        scopes: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(db("查询访问令牌失败"))?;
        let PatRow {
            token_id,
            user_id,
            expires_at,
            revoked_at,
            scopes,
        } = row.ok_or_else(|| {
            // 与密码错误同码：不向调用方泄露「令牌格式对但值错」这类信息。
            Error::new(ErrorCode::InvalidCredentials, "访问令牌无效")
        })?;
        if revoked_at.is_some() {
            return Err(Error::new(ErrorCode::PatRevoked, "该访问令牌已被撤销"));
        }
        if expires_at.is_some_and(|exp| exp <= now_secs()) {
            return Err(Error::new(ErrorCode::TokenExpired, "该访问令牌已过期"));
        }

        let (username, display_name, status, role) = self
            .conn
            .query_row(
                "SELECT username, display_name, status, role FROM users WHERE user_id = ?1",
                params![user_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(db("查询用户失败"))?
            .ok_or_else(|| Error::new(ErrorCode::Unauthenticated, "令牌对应的用户不存在"))?;
        if !parse_account_status(&status)?.may_login() {
            return Err(Error::new(ErrorCode::AccountDisabled, "账号已不可用"));
        }

        self.conn
            .execute(
                "UPDATE pat_tokens SET last_used_at = ?2 WHERE token_id = ?1",
                params![token_id, now_secs()],
            )
            .map_err(db("记录令牌使用时间失败"))?;

        Ok(Session {
            user_id,
            username,
            display_name,
            role: parse_role(&role)?,
            kind: ActorKind::User,
            scopes: Some(serde_json::from_str(&scopes).unwrap_or_default()),
        })
    }

    /// 在给定事务内插入仓库 + owner 成员 + 初始空版本 r0。
    ///
    /// SHARED_CAS 下没有「每个仓库一个目录」需要预建（设计 8.8 的初始化顺序在这里
    /// 退化为空操作），因此不变量简化为：事务提交成功即代表仓库可用。
    #[allow(clippy::too_many_arguments)]
    fn insert_new_repo(
        &self,
        tx: &rusqlite::Transaction<'_>,
        name: &str,
        owner_user_id: &str,
        quota_bytes: u64,
        from_request: Option<&str>,
        author: &str,
        audit_action: &str,
    ) -> Result<String> {
        let repo_id = new_id("repo");
        let now = now_secs();
        tx.execute(
            "INSERT INTO repos (repo_id, name, head_rev, status, owner_user_id, created_from_request,
                                quota_bytes, created_at, updated_at)
             VALUES (?1, ?2, 0, 'active', ?3, ?4, ?5, ?6, ?6)",
            params![
                repo_id,
                name,
                owner_user_id,
                from_request,
                quota_bytes,
                now
            ],
        )
        .map_err(|e| map_unique(e, "仓库名已存在"))?;
        tx.execute(
            "INSERT INTO repo_members (repo_id, user_id, role, added_by, added_at)
             VALUES (?1, ?2, 'owner', ?2, ?3)",
            params![repo_id, owner_user_id, now],
        )
        .map_err(db("写入 owner 成员失败"))?;
        // 首个 revision 是空树，保证 head_rev=0 与「无提交」都可用。
        let empty_root = tree::write_tree(&self.objects, &[])?;
        tx.execute(
            "INSERT INTO revisions (repo_id, rev, root_tree, parent_rev, author, message, committed_at, change_count)
             VALUES (?1, 0, ?2, 0, ?3, 'empty root', ?4, 0)",
            params![repo_id, to_hex(&empty_root), author, now],
        )
        .map_err(db("写入初始版本失败"))?;
        write_audit(
            tx,
            &repo_id,
            owner_user_id,
            audit_action,
            name,
            from_request.map(|_| "{\"from_request\":true}"),
            now,
        )?;
        Ok(repo_id)
    }

    // ---------------- 容量治理：清理与 GC ----------------

    /// 全库反向引用表：对象哈希 → 引用它的 `(repo_id, path)` 列表。
    ///
    /// 一张表同时喂给三个调用方：`EstimatePurge` 算独占 / 共享、`Purge` 做预检、GC 判可达。
    /// 「可达」在本设计里等价于「至少存在一个不在已裁剪路径下的引用」——所以 GC 不需要
    /// 在遍历时剪枝，只要做集合运算，天然与 `EstimatePurge` 的口径一致，预估和实际释放量
    /// 不会两套算法各算一套。
    ///
    /// 粒度：按 `DISTINCT repo_id, root_tree` 遍历，内容完全相同的版本只走一遍；
    /// 规模与「历史文件数」同阶，M2 量级完全够用，M3 可按 `first_seen_rev` 分批增量标记。
    fn reference_map(&self) -> Result<BTreeMap<Hash, Vec<(String, String)>>> {
        let mut out: BTreeMap<Hash, Vec<(String, String)>> = BTreeMap::new();
        let mut visited: std::collections::BTreeSet<(String, Hash, String)> =
            std::collections::BTreeSet::new();

        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT repo_id, root_tree FROM revisions")
            .map_err(db("读取版本失败"))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db("读取版本失败"))?;
        for row in rows {
            let (repo_id, root_hex) = row.map_err(db("读取版本失败"))?;
            let root = vcs_core::from_hex(&root_hex)?;
            self.collect_owners(&repo_id, &root, "", &mut visited, &mut out)?;
        }

        // 同一个对象会被多个版本、多条路径引用：去重后规模才是「引用数」而不是「出现次数」。
        for refs in out.values_mut() {
            refs.sort();
            refs.dedup();
        }
        Ok(out)
    }

    fn collect_owners(
        &self,
        repo_id: &str,
        tree: &Hash,
        prefix: &str,
        visited: &mut std::collections::BTreeSet<(String, Hash, String)>,
        out: &mut BTreeMap<Hash, Vec<(String, String)>>,
    ) -> Result<()> {
        // 访问标记必须带 repo_id：跨仓库共享的同一棵子树仍要各记一次归属，
        // 否则「A 仓库用过的对象」会被当成只有 B 在用，GC 就可能误删。
        if !visited.insert((repo_id.to_string(), *tree, prefix.to_string())) {
            return Ok(());
        }
        // 树对象本身也算目录的内容：目录被裁剪掉，它的树对象就该回收。
        out.entry(*tree)
            .or_default()
            .push((repo_id.to_string(), prefix.to_string()));

        for entry in tree::read_tree(self.objects(), tree)? {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            match entry.kind {
                EntryKind::Dir => self.collect_owners(repo_id, &entry.hash, &path, visited, out)?,
                EntryKind::File | EntryKind::Symlink => {
                    out.entry(entry.hash)
                        .or_default()
                        .push((repo_id.to_string(), path));
                }
            }
        }
        Ok(())
    }

    /// **内容对象**（blob）的物理大小，避免逐对象去磁盘 stat。
    ///
    /// 只取 `kind='blob'`：tree 对象一律不回收。设计 9.4 明确「旧 tree 中残留的悬空哈希不删除
    /// （tree 对象很小）」——如果连 tree 一起删，旧版本的整棵树就再也展不平，
    /// 用户连 `log` 里那条记录的内容清单都看不到。留树、删内容，才能做到
    /// 「元数据永久保留、只有内容字节被裁剪」。
    fn blob_sizes(&self) -> Result<BTreeMap<Hash, u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash, stored_size FROM objects WHERE kind = 'blob'")
            .map_err(db("读取对象索引失败"))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(db("读取对象索引失败"))?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (hex, stored) = row.map_err(db("读取对象索引失败"))?;
            out.insert(vcs_core::from_hex(&hex)?, stored.max(0) as u64);
        }
        Ok(out)
    }

    /// 清理预估（dry-run）。不改任何数据，这是设计里「用户无需理解 GC」的关键一步。
    pub fn estimate_purge(
        &self,
        session: &Session,
        name_or_id: &str,
        paths: &[String],
    ) -> Result<PurgeEstimate> {
        session.require(SCOPE_WRITE, "清理空间")?;
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "当前成员角色为只读，不能清理空间",
            ));
        }
        let prefixes = normalize_prefixes(paths)?;
        self.purge_estimate(&repo.repo_id, repo.head_rev, &prefixes)
    }

    /// 预估的实际计算部分（不含权限校验），供 `EstimatePurge` 与保留策略复用。
    fn purge_estimate(
        &self,
        repo_id: &str,
        head_rev: u64,
        prefixes: &[String],
    ) -> Result<PurgeEstimate> {
        let head_root = self.revision_root_tree(repo_id, head_rev)?;
        let head_flat = tree::flatten_tree(self.objects(), &head_root)?;
        let logical_bytes: u64 = head_flat
            .iter()
            .filter(|e| path_under_any(&e.path, prefixes))
            .map(|e| e.size)
            .sum();

        let owners = self.reference_map()?;
        let sizes = self.blob_sizes()?;
        let mut reclaimable = 0u64;
        let mut shared = 0u64;
        let mut shared_paths: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();

        for (hash, refs) in &owners {
            let mine = refs
                .iter()
                .any(|(r, p)| r == repo_id && path_under_any(p, prefixes));
            if !mine {
                continue;
            }
            let bytes = sizes.get(hash).copied().unwrap_or(0);
            let mut outside = false;
            for (r, p) in refs {
                if r == repo_id && path_under_any(p, prefixes) {
                    continue;
                }
                outside = true;
                if shared_paths.len() < 20 {
                    shared_paths.insert(p.clone());
                }
            }
            if outside {
                shared += bytes;
            } else {
                reclaimable += bytes;
            }
        }

        Ok(PurgeEstimate {
            paths: prefixes.to_vec(),
            logical_bytes,
            reclaimable_bytes: reclaimable,
            shared_bytes: shared,
            affected_revisions: self.count_revisions_touching(repo_id, prefixes)?,
            shared_paths: shared_paths.into_iter().collect(),
        })
    }

    /// 清理：删除提交 + 路径裁剪标记 + 排一个 GC 任务。
    ///
    /// 三步缺一不可——只删路径不写 `purged_paths`，旧版本仍引用内容，GC 一字节都回收不了；
    /// 只写标记不删 HEAD，用户看到的目录还在。`immediate=true` 时立刻跑一次 GC，
    /// 这是「磁盘已满」场景唯一的出路（默认的 7 天宽限期在此毫无意义）。
    pub fn purge(
        &self,
        session: &Session,
        name_or_id: &str,
        paths: &[String],
        immediate: bool,
        reason: &str,
    ) -> Result<PurgeOutcome> {
        // estimate 内部已做成员与写权限校验。
        let estimate = self.estimate_purge(session, name_or_id, paths)?;
        let repo = self.require_repo(session, name_or_id)?;

        let head_root = self.revision_root_tree(&repo.repo_id, repo.head_rev)?;
        let head_flat = tree::flatten_tree(self.objects(), &head_root)?;
        let deletes: Vec<PendingChange> = head_flat
            .iter()
            .filter(|e| path_under_any(&e.path, &estimate.paths))
            .map(|e| PendingChange::file(e.path.clone(), ChangeKind::Delete, None, 0))
            .collect();
        if deletes.is_empty() {
            return Err(Error::new(
                ErrorCode::PathConflict,
                "HEAD 中不存在这些路径，无需清理",
            ));
        }

        let message = format!(
            "清理 {}（{}）",
            estimate.paths.join(", "),
            if immediate {
                "立即释放"
            } else {
                "进入回收站，7 天后释放"
            }
        );
        let commit =
            self.begin_commit(session, &repo.repo_id, repo.head_rev, &deletes, &message, 0)?;
        let outcome = self.finalize_commit(session, &commit.session_id)?;

        let now = now_secs();
        // 宽限期取仓库自己的配置（schema 里一直有 repos.grace_days，早先被硬编码成 7 天而忽略了）。
        let grace_days: i64 = self
            .conn
            .query_row(
                "SELECT grace_days FROM repos WHERE repo_id = ?1",
                params![repo.repo_id],
                |r| r.get(0),
            )
            .map_err(db("查询宽限期失败"))?;
        let state = if immediate { "active" } else { "pending" };
        let grace_until = if immediate {
            None
        } else {
            Some(now + grace_days.max(0) * 86_400)
        };
        let activated_at = if immediate { Some(now) } else { None };
        let mut purge_id = String::new();
        for path in &estimate.paths {
            let id = new_id("pg");
            if purge_id.is_empty() {
                purge_id = id.clone();
            }
            self.conn
                .execute(
                    "INSERT INTO purged_paths
                       (purge_id, repo_id, path_pattern, state, reason, purged_by, created_at, grace_until, activated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        id,
                        repo.repo_id,
                        path,
                        state,
                        reason,
                        session.user_id,
                        now,
                        grace_until,
                        activated_at
                    ],
                )
                .map_err(db("写入清理标记失败"))?;
        }

        let gc_job_id = new_id("gc");
        self.conn
            .execute(
                "INSERT INTO gc_jobs (job_id, repo_id, purge_id, state, progress_percent)
                 VALUES (?1, ?2, ?3, 'queued', 0)",
                params![gc_job_id, repo.repo_id, purge_id],
            )
            .map_err(db("创建 GC 任务失败"))?;

        if immediate {
            // M2 没有独立后台 worker：直接跑完再返回，任务行与异步 worker 完全同构，
            // M3 换成队列时这里只是把同步调用挪进 worker。
            self.run_gc_job(&gc_job_id)?;
        }

        self.audit(
            &repo.repo_id,
            &session.user_id,
            "purge.execute",
            &estimate.paths.join(","),
            Some(&format!(
                "{{\"reclaimable\":{},\"shared\":{},\"immediate\":{},\"rev\":{}}}",
                estimate.reclaimable_bytes, estimate.shared_bytes, immediate, outcome.rev
            )),
        )?;

        Ok(PurgeOutcome {
            purge_id,
            rev: outcome.rev,
            gc_job_id,
            deleted: deletes.len(),
            estimate,
        })
    }

    /// 调整仓库的回收站宽限期（天）。0 表示清理立即生效。
    ///
    /// 这个值决定「用户删了目录之后还有多久能反悔」，因此按写权限校验而不是管理员专属：
    /// owner 在自己的仓库里本来就掌握空间治理的全部动作。
    pub fn set_repo_grace_days(
        &self,
        session: &Session,
        name_or_id: &str,
        days: u32,
    ) -> Result<()> {
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "当前成员角色为只读，不能调整宽限期",
            ));
        }
        self.conn
            .execute(
                "UPDATE repos SET grace_days = ?2, updated_at = ?3 WHERE repo_id = ?1",
                params![repo.repo_id, days as i64, now_secs()],
            )
            .map_err(db("更新宽限期失败"))?;
        self.audit(
            &repo.repo_id,
            &session.user_id,
            "repo.grace_days.set",
            &repo.name,
            Some(&format!("{{\"days\":{days}}}")),
        )?;
        Ok(())
    }

    /// 已执行的清理记录（回收站）。成员可见，写权限才能撤销。
    pub fn list_purged_paths(
        &self,
        session: &Session,
        name_or_id: &str,
    ) -> Result<Vec<PurgedPathInfo>> {
        let repo = self.find_repo(name_or_id)?;
        // 成员门：非成员不该知道这个仓库清理过什么。
        self.member_role(&repo.repo_id, &session.user_id)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT purge_id, path_pattern, state, reason, purged_by,
                        created_at, grace_until, activated_at
                 FROM purged_paths WHERE repo_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(db("查询清理记录失败"))?;
        let rows = stmt
            .query_map(params![repo.repo_id], |r| {
                Ok(PurgedPathInfo {
                    purge_id: r.get(0)?,
                    path: r.get(1)?,
                    state: r.get(2)?,
                    reason: r.get(3)?,
                    purged_by: r.get(4)?,
                    created_at: r.get(5)?,
                    grace_until: r.get(6)?,
                    activated_at: r.get(7)?,
                })
            })
            .map_err(db("查询清理记录失败"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db("读取清理记录失败"))
    }

    /// 撤销一次清理：把路径内容恢复到 HEAD，并把裁剪标记置为 `reverted`。
    ///
    /// 两个前提必须先讲清：
    ///   1. 内容必须还在对象池里。GC 已经把 blob 物理删掉的，**没有任何办法恢复**——
    ///      此时返回 `CONTENT_PURGED` 而不是伪造一个指向空内容的版本。
    ///   2. 恢复是「新提交」而不是回滚历史：revision 只增不改，撤销本身也是一次修订。
    pub fn revert_purge(&self, session: &Session, name_or_id: &str, purge_id: &str) -> Result<u64> {
        session.require(SCOPE_WRITE, "撤销清理")?;
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "当前成员角色为只读，不能撤销清理",
            ));
        }

        let record: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT path_pattern, state FROM purged_paths
                 WHERE purge_id = ?1 AND repo_id = ?2",
                params![purge_id, repo.repo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db("查询清理记录失败"))?;
        let (pattern, state) = record.ok_or_else(|| {
            Error::new(ErrorCode::PathConflict, "清理记录不存在（或不属于该仓库）")
        })?;
        if state == "reverted" {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "这次清理已经撤销过了",
            ));
        }

        let head_root = self.revision_root_tree(&repo.repo_id, repo.head_rev)?;
        let head_flat = tree::flatten_tree(self.objects(), &head_root)?;
        let head_map: BTreeMap<&str, Hash> = head_flat
            .iter()
            .map(|entry| (entry.path.as_str(), entry.hash))
            .collect();
        let prefixes = vec![pattern.clone()];

        // 找到最近一个「还包含该路径」的版本，它就是恢复的内容来源。
        let revisions: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT rev FROM revisions WHERE repo_id = ?1 ORDER BY rev DESC")
                .map_err(db("查询版本失败"))?;
            let rows = stmt
                .query_map(params![repo.repo_id], |r| r.get::<_, i64>(0))
                .map_err(db("查询版本失败"))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db("读取版本失败"))?
        };
        let mut source: Option<Vec<FlatEntry>> = None;
        for rev in revisions {
            if rev as u64 >= repo.head_rev {
                continue;
            }
            let root = self.revision_root_tree(&repo.repo_id, rev as u64)?;
            let flat = tree::flatten_tree(self.objects(), &root)?;
            if flat
                .iter()
                .any(|entry| path_under_any(&entry.path, &prefixes))
            {
                source = Some(flat);
                break;
            }
        }
        let source = source.ok_or_else(|| {
            Error::new(
                ErrorCode::ContentPurged,
                format!("历史版本里也找不到 {pattern} 的内容，无法恢复"),
            )
        })?;

        let mut changes: Vec<PendingChange> = Vec::new();
        for entry in source
            .iter()
            .filter(|entry| path_under_any(&entry.path, &prefixes))
        {
            match head_map.get(entry.path.as_str()) {
                Some(hash) if *hash == entry.hash => continue,
                Some(_) => changes.push(PendingChange::file(
                    entry.path.clone(),
                    ChangeKind::Modify,
                    Some(entry.hash),
                    entry.size,
                )),
                None => changes.push(PendingChange::file(
                    entry.path.clone(),
                    ChangeKind::Add,
                    Some(entry.hash),
                    entry.size,
                )),
            }
        }

        // 先确认内容还在：缺一个字节都不该往下走。
        let missing = changes
            .iter()
            .filter(|change| change.hash.is_some_and(|hash| !self.objects.exists(&hash)))
            .count();
        if missing > 0 {
            return Err(Error::new(
                ErrorCode::ContentPurged,
                format!(
                    "{pattern} 有 {missing} 个对象已被 GC 物理回收，无法恢复（可从旧版本的导出副本手工补回）"
                ),
            ));
        }

        let new_rev = if changes.is_empty() {
            repo.head_rev
        } else {
            let message = format!("撤销清理：{pattern}");
            let commit =
                self.begin_commit(session, &repo.repo_id, repo.head_rev, &changes, &message, 0)?;
            self.finalize_commit(session, &commit.session_id)?.rev
        };

        self.conn
            .execute(
                "UPDATE purged_paths SET state = 'reverted' WHERE purge_id = ?1",
                params![purge_id],
            )
            .map_err(db("更新清理记录失败"))?;
        self.audit(
            &repo.repo_id,
            &session.user_id,
            "purge.revert",
            &pattern,
            Some(&format!(
                "{{\"purge_id\":\"{purge_id}\",\"new_rev\":{new_rev},\"restored\":{}}}",
                changes.len()
            )),
        )?;
        Ok(new_rev)
    }

    // ---------------- 保留策略（设计 9.7） ----------------

    pub fn list_retention_policies(
        &self,
        session: &Session,
        name_or_id: &str,
    ) -> Result<Vec<RetentionPolicyInfo>> {
        let repo = self.find_repo(name_or_id)?;
        self.member_role(&repo.repo_id, &session.user_id)?;
        self.load_retention_policies(&repo.repo_id)
    }

    fn load_retention_policies(&self, repo_id: &str) -> Result<Vec<RetentionPolicyInfo>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT policy_id, path_pattern, keep_months, max_bytes, max_age_days,
                        enabled, dry_run, created_at
                 FROM retention_policies WHERE repo_id = ?1 ORDER BY created_at",
            )
            .map_err(db("查询保留策略失败"))?;
        let rows = stmt
            .query_map(params![repo_id], |r| {
                Ok(RetentionPolicyInfo {
                    policy_id: r.get(0)?,
                    path_pattern: r.get(1)?,
                    keep_months: r.get::<_, i64>(2)?.max(0) as u32,
                    max_bytes: r.get::<_, i64>(3)?.max(0) as u64,
                    max_age_days: r.get::<_, i64>(4)?.max(0) as u32,
                    enabled: r.get::<_, i64>(5)? != 0,
                    dry_run: r.get::<_, i64>(6)? != 0,
                    created_at: r.get(7)?,
                })
            })
            .map_err(db("查询保留策略失败"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db("读取保留策略失败"))
    }

    /// 新建或更新策略。策略会自动删除数据，因此要求 owner / maintainer。
    pub fn upsert_retention_policy(
        &self,
        session: &Session,
        name_or_id: &str,
        input: RetentionPolicyInput,
    ) -> Result<RetentionPolicyInfo> {
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_manage_members() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "只有 owner / maintainer 可以配置保留策略",
            ));
        }
        let pattern = input.path_pattern.trim();
        if pattern.is_empty() {
            return Err(Error::new(ErrorCode::InvalidArgument, "路径模式不能为空"));
        }
        // 正则语法在建策略时就要校验：等到执行时才报错，用户已经以为配置好了。
        regex::Regex::new(pattern).map_err(|e| {
            Error::new(
                ErrorCode::InvalidArgument,
                format!("路径模式不是合法正则：{e}"),
            )
        })?;
        if input.keep_months == 0 && input.max_bytes == 0 && input.max_age_days == 0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "keep_months / max_bytes / max_age_days 至少要设置一个，否则策略没有意义",
            ));
        }

        let now = now_secs();
        let policy_id = match &input.policy_id {
            Some(id) if !id.is_empty() => {
                let exists: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT policy_id FROM retention_policies
                         WHERE policy_id = ?1 AND repo_id = ?2",
                        params![id, repo.repo_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(db("查询保留策略失败"))?;
                if exists.is_none() {
                    return Err(Error::new(ErrorCode::PathConflict, "保留策略不存在"));
                }
                id.clone()
            }
            _ => new_id("rt"),
        };

        self.conn
            .execute(
                "INSERT INTO retention_policies
                   (policy_id, repo_id, path_pattern, keep_months, max_bytes, max_age_days,
                    enabled, dry_run, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(policy_id) DO UPDATE SET
                   path_pattern = excluded.path_pattern,
                   keep_months = excluded.keep_months,
                   max_bytes = excluded.max_bytes,
                   max_age_days = excluded.max_age_days,
                   enabled = excluded.enabled,
                   dry_run = excluded.dry_run",
                params![
                    policy_id,
                    repo.repo_id,
                    pattern,
                    input.keep_months as i64,
                    input.max_bytes as i64,
                    input.max_age_days as i64,
                    if input.enabled { 1 } else { 0 },
                    if input.dry_run { 1 } else { 0 },
                    now
                ],
            )
            .map_err(db("写入保留策略失败"))?;
        self.audit(
            &repo.repo_id,
            &session.user_id,
            "retention.upsert",
            pattern,
            Some(&format!(
                "{{\"policy_id\":\"{policy_id}\",\"keep_months\":{},\"max_bytes\":{},\"max_age_days\":{},\"dry_run\":{}}}",
                input.keep_months, input.max_bytes, input.max_age_days, input.dry_run
            )),
        )?;
        Ok(RetentionPolicyInfo {
            policy_id,
            path_pattern: pattern.to_string(),
            keep_months: input.keep_months,
            max_bytes: input.max_bytes,
            max_age_days: input.max_age_days,
            enabled: input.enabled,
            dry_run: input.dry_run,
            created_at: now,
        })
    }

    pub fn delete_retention_policy(
        &self,
        session: &Session,
        name_or_id: &str,
        policy_id: &str,
    ) -> Result<()> {
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_manage_members() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "只有 owner / maintainer 可以配置保留策略",
            ));
        }
        let affected = self
            .conn
            .execute(
                "DELETE FROM retention_policies WHERE policy_id = ?1 AND repo_id = ?2",
                params![policy_id, repo.repo_id],
            )
            .map_err(db("删除保留策略失败"))?;
        if affected == 0 {
            return Err(Error::new(ErrorCode::PathConflict, "保留策略不存在"));
        }
        self.audit(
            &repo.repo_id,
            &session.user_id,
            "retention.delete",
            policy_id,
            None,
        )?;
        Ok(())
    }

    /// 评估并（可选）执行保留策略。
    ///
    /// 规则语义：`keep_months` / `max_bytes` / `max_age_days` 各自是一条**上限**，
    /// 任何一条被违反，该目录就成为清理候选（并集）。目录按名称倒序当作"从新到旧"，
    /// 这也是按月归档 `202610` 这类命名的自然顺序。
    pub fn apply_retention(
        &self,
        session: &Session,
        name_or_id: &str,
        dry_run: bool,
        immediate: bool,
    ) -> Result<RetentionApplyOutcome> {
        session.require(SCOPE_WRITE, "执行保留策略")?;
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "当前成员角色为只读，不能执行保留策略",
            ));
        }

        let policies: Vec<RetentionPolicyInfo> = self
            .load_retention_policies(&repo.repo_id)?
            .into_iter()
            .filter(|policy| policy.enabled)
            .collect();
        if policies.is_empty() {
            return Ok(RetentionApplyOutcome {
                candidates: Vec::new(),
                reclaimable_bytes: 0,
                new_rev: 0,
            });
        }

        let candidates = self.retention_candidates(&repo.repo_id, repo.head_rev, &policies)?;
        if candidates.is_empty() {
            return Ok(RetentionApplyOutcome {
                candidates,
                reclaimable_bytes: 0,
                new_rev: 0,
            });
        }

        let estimate = self.purge_estimate(&repo.repo_id, repo.head_rev, &candidates)?;
        let all_dry_run = policies.iter().all(|policy| policy.dry_run);
        if !dry_run && all_dry_run {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "所有生效的策略都处于 dry_run：请先在策略里关闭 dry_run 再执行",
            ));
        }
        if dry_run || all_dry_run {
            return Ok(RetentionApplyOutcome {
                candidates,
                reclaimable_bytes: estimate.reclaimable_bytes,
                new_rev: 0,
            });
        }

        let outcome = self.purge(
            session,
            &repo.repo_id,
            &candidates,
            immediate,
            "保留策略自动清理",
        )?;
        Ok(RetentionApplyOutcome {
            candidates,
            reclaimable_bytes: estimate.reclaimable_bytes,
            new_rev: outcome.rev,
        })
    }

    /// 按策略挑出应当清理的顶层目录。
    fn retention_candidates(
        &self,
        repo_id: &str,
        head_rev: u64,
        policies: &[RetentionPolicyInfo],
    ) -> Result<Vec<String>> {
        let head_root = self.revision_root_tree(repo_id, head_rev)?;
        let head_flat = tree::flatten_tree(self.objects(), &head_root)?;
        let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
        for entry in &head_flat {
            let top = top_level(&entry.path);
            if top.is_empty() {
                continue; // 根目录下的散文件不参与按月归档的清理
            }
            *sizes.entry(top).or_default() += entry.size;
        }
        if sizes.is_empty() {
            return Ok(Vec::new());
        }

        // 每个目录最近一次被修改的版本 → 提交时间，供 max_age_days 使用。
        let mut last_rev: BTreeMap<String, i64> = BTreeMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT rev, path FROM revision_changes WHERE repo_id = ?1")
                .map_err(db("读取变更记录失败"))?;
            let rows = stmt
                .query_map(params![repo_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(db("读取变更记录失败"))?;
            for row in rows {
                let (rev, path) = row.map_err(db("读取变更记录失败"))?;
                let top = top_level(&path);
                if top.is_empty() {
                    continue;
                }
                let slot = last_rev.entry(top).or_insert(rev);
                if rev > *slot {
                    *slot = rev;
                }
            }
        }
        let mut committed: BTreeMap<i64, i64> = BTreeMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT rev, committed_at FROM revisions WHERE repo_id = ?1")
                .map_err(db("查询版本失败"))?;
            let rows = stmt
                .query_map(params![repo_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(db("查询版本失败"))?;
            for row in rows {
                let (rev, at) = row.map_err(db("查询版本失败"))?;
                committed.insert(rev, at);
            }
        }

        let now = now_secs();
        let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for policy in policies {
            let Ok(pattern) = regex::Regex::new(&policy.path_pattern) else {
                // 建策略时已校验过；这里保守跳过而不是让整个评估失败。
                continue;
            };
            // 名字倒序 = 从新到旧（按月归档的 202610 这类命名天然可比）。
            let mut matched: Vec<(&String, &u64)> = sizes
                .iter()
                .filter(|(name, _)| pattern.is_match(name))
                .collect();
            matched.sort_by(|a, b| b.0.cmp(a.0));

            if policy.keep_months > 0 {
                for (name, _) in matched.iter().skip(policy.keep_months as usize) {
                    out.insert((*name).clone());
                }
            }
            if policy.max_bytes > 0 {
                let mut accumulated = 0u64;
                for (name, bytes) in &matched {
                    if accumulated + **bytes > policy.max_bytes {
                        out.insert((*name).clone());
                    }
                    accumulated += **bytes;
                }
            }
            if policy.max_age_days > 0 {
                let limit = policy.max_age_days as i64 * 86_400;
                for (name, _) in &matched {
                    let ts = last_rev
                        .get(*name)
                        .and_then(|rev| committed.get(rev))
                        .copied()
                        .unwrap_or(0);
                    if ts > 0 && now - ts > limit {
                        out.insert((*name).clone());
                    }
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// 把宽限期到点的清理记录转正（PENDING → ACTIVE），返回条数。
    fn promote_pending_purges(&self) -> Result<usize> {
        let now = now_secs();
        self.conn
            .execute(
                "UPDATE purged_paths SET state = 'active', activated_at = ?1
                 WHERE state = 'pending' AND (grace_until IS NULL OR grace_until <= ?1)",
                params![now],
            )
            .map_err(db("激活清理标记失败"))
    }

    /// 后台维护：转正到期的清理标记，然后跑一次全库 GC。
    ///
    /// 这是 `vcs serve` 的定时任务调用的入口——不需要 session，因为它没有调用者。
    /// GC 是全局的（共享 CAS），所以只需一轮；`repo_count` 仅用于日志。
    pub fn run_maintenance(&self) -> Result<MaintenanceReport> {
        // 先执行保留策略：它会产生删除提交，随后同一轮的转正 + GC 就能接上。
        let (retention_repos, retention_paths) = self.apply_retention_all()?;
        let promoted = self.promote_pending_purges()?;
        let repo_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .map_err(db("统计仓库数失败"))?;

        let job_id = new_id("gc");
        self.conn
            .execute(
                // repo_id 留空：这是系统级任务，不属于任何一个仓库。
                "INSERT INTO gc_jobs (job_id, repo_id, state, progress_percent)
                 VALUES (?1, '', 'queued', 0)",
                params![job_id],
            )
            .map_err(db("创建维护任务失败"))?;
        let status = self.run_gc_job(&job_id)?;
        Ok(MaintenanceReport {
            promoted_purges: promoted,
            gc_job_id: job_id,
            swept_objects: status.swept_objects,
            reclaimed_bytes: status.reclaimed_bytes,
            repo_count: repo_count.max(0) as usize,
            retention_repos,
            retention_paths,
        })
    }

    /// 对所有仓库执行「非 dry_run」的保留策略。
    ///
    /// 系统身份：没有成员可言，只跳过成员与角色这两道门；仓库状态、配额、out-of-date
    /// 全部照常校验。单个仓库失败不影响其它仓库——定时任务最忌讳「一个坏仓库卡住全库」。
    fn apply_retention_all(&self) -> Result<(usize, usize)> {
        let repo_ids: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT repo_id FROM repos WHERE status = 'active'")
                .map_err(db("查询仓库失败"))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(db("查询仓库失败"))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db("读取仓库失败"))?
        };

        let system = Session::system();
        let mut repos = 0usize;
        let mut paths = 0usize;
        for repo_id in repo_ids {
            match self.apply_retention_auto(&system, &repo_id) {
                Ok(outcome) => {
                    if outcome.new_rev > 0 {
                        repos += 1;
                        paths += outcome.candidates.len();
                    }
                }
                Err(err) => {
                    // 一个仓库失败不该中断整轮维护，逐条记下来即可。
                    eprintln!(
                        "保留策略在仓库 {repo_id} 上失败：{:?}: {}",
                        err.code, err.message
                    );
                }
            }
        }
        Ok((repos, paths))
    }

    /// 自动执行：只考虑「启用且非 dry_run」的策略，不会因为别的策略是 dry_run 而拒绝执行。
    fn apply_retention_auto(
        &self,
        system: &Session,
        repo_id: &str,
    ) -> Result<RetentionApplyOutcome> {
        let policies: Vec<RetentionPolicyInfo> = self
            .load_retention_policies(repo_id)?
            .into_iter()
            .filter(|policy| policy.enabled && !policy.dry_run)
            .collect();
        if policies.is_empty() {
            return Ok(RetentionApplyOutcome {
                candidates: Vec::new(),
                reclaimable_bytes: 0,
                new_rev: 0,
            });
        }
        let repo = self.require_repo(system, repo_id)?;
        let candidates = self.retention_candidates(&repo.repo_id, repo.head_rev, &policies)?;
        if candidates.is_empty() {
            return Ok(RetentionApplyOutcome {
                candidates,
                reclaimable_bytes: 0,
                new_rev: 0,
            });
        }
        let estimate = self.purge_estimate(&repo.repo_id, repo.head_rev, &candidates)?;
        // 自动执行走回收站默认宽限期：无人值守的删除更需要「能反悔」。
        let outcome = self.purge(
            system,
            &repo.repo_id,
            &candidates,
            false,
            "保留策略自动清理",
        )?;
        Ok(RetentionApplyOutcome {
            candidates,
            reclaimable_bytes: estimate.reclaimable_bytes,
            new_rev: outcome.rev,
        })
    }

    /// 手动触发一次整库 GC（共享 CAS 下 GC 天然是全局的）。
    pub fn run_gc(&self, session: &Session, name_or_id: &str) -> Result<GcJobStatus> {
        session.require(SCOPE_WRITE, "执行空间回收")?;
        let repo = self.require_repo(session, name_or_id)?;
        // 系统身份按 owner 处理：它没有成员记录，也不该被成员门挡住。
        let role = if session.is_system() {
            RepoMemberRole::Owner
        } else {
            self.member_role(&repo.repo_id, &session.user_id)?
        };
        if !role.can_write() {
            return Err(Error::new(
                ErrorCode::RepoMemberForbidden,
                "当前成员角色为只读，不能执行空间回收",
            ));
        }
        let job_id = new_id("gc");
        self.conn
            .execute(
                "INSERT INTO gc_jobs (job_id, repo_id, state, progress_percent)
                 VALUES (?1, ?2, 'queued', 0)",
                params![job_id, repo.repo_id],
            )
            .map_err(db("创建 GC 任务失败"))?;
        let status = self.run_gc_job(&job_id)?;
        self.audit(
            &repo.repo_id,
            &session.user_id,
            "gc.run",
            &job_id,
            Some(&format!(
                "{{\"marked\":{},\"swept\":{},\"reclaimed\":{}}}",
                status.marked_objects, status.swept_objects, status.reclaimed_bytes
            )),
        )?;
        Ok(status)
    }

    /// GC 主体：标记 → 清扫两段式。
    fn run_gc_job(&self, job_id: &str) -> Result<GcJobStatus> {
        let now = now_secs();
        self.conn
            .execute(
                "UPDATE gc_jobs SET state = 'marking', started_at = ?2, progress_percent = 5
                 WHERE job_id = ?1",
                params![job_id, now],
            )
            .map_err(db("更新 GC 任务失败"))?;

        // 宽限期到点的清理在这里转正：GC 是唯一会推进 PENDING → ACTIVE 的地方。
        self.promote_pending_purges()?;

        let mut purges: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT repo_id, path_pattern FROM purged_paths WHERE state = 'active'")
                .map_err(db("读取清理标记失败"))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(db("读取清理标记失败"))?;
            for row in rows {
                let (repo_id, pattern) = row.map_err(db("读取清理标记失败"))?;
                purges.entry(repo_id).or_default().push(pattern);
            }
        }

        let owners = self.reference_map()?;
        let sizes = self.blob_sizes()?;
        // 竞态防护不需要时间窗：`objects` 表的行只由 `finalize_commit` 在写版本的**同一事务**里
        // 插入，所以「表里有行」必然意味着「某个已提交版本引用它」。正在 PushObjects 但还没
        // Finalize 的对象只存在于磁盘、没有索引行，本轮标记和清扫都碰不到它们。
        // （早先按 `created_at >= mark_started` 兜底是错的：秒级精度会把同一秒内提交的
        //   正常内容也当成"并发对象"永久保护起来，导致 GC 一字节都收不回。）
        let mut marked: Vec<Hash> = Vec::new();
        for hash in sizes.keys() {
            let reachable = match owners.get(hash) {
                Some(refs) => refs.iter().any(|(repo_id, path)| {
                    let skip = purges.get(repo_id).map(|v| v.as_slice()).unwrap_or(&[]);
                    !path_under_any(path, skip)
                }),
                // 索引里有、引用图里没有：孤儿对象。提交失败、旧版删除都会留下它，
                // 正是 GC 要收的东西。
                None => false,
            };
            if reachable {
                continue;
            }
            marked.push(*hash);
        }

        let mut mark_stmt = self
            .conn
            .prepare("UPDATE objects SET pending_delete = 1 WHERE hash = ?1")
            .map_err(db("标记对象失败"))?;
        for hash in &marked {
            mark_stmt
                .execute(params![to_hex(hash)])
                .map_err(db("标记对象失败"))?;
        }
        drop(mark_stmt);

        self.conn
            .execute(
                "UPDATE gc_jobs SET state = 'sweeping', progress_percent = 60, marked_objects = ?2
                 WHERE job_id = ?1",
                params![job_id, marked.len() as i64],
            )
            .map_err(db("更新 GC 任务失败"))?;

        let pending: Vec<(Hash, u64)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT hash, stored_size FROM objects WHERE pending_delete = 1")
                .map_err(db("读取待删对象失败"))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(db("读取待删对象失败"))?;
            let mut out = Vec::new();
            for row in rows {
                let (hex, stored) = row.map_err(db("读取待删对象失败"))?;
                out.push((vcs_core::from_hex(&hex)?, stored.max(0) as u64));
            }
            out
        };

        let mut swept = 0u64;
        let mut reclaimed = 0u64;
        for (hash, stored) in &pending {
            // 文件可能已经被其他路径手动删掉；这里的语义是「确保不存在」，不追求清理现场。
            let _ = self.objects.remove(hash);
            self.conn
                .execute("DELETE FROM objects WHERE hash = ?1", params![to_hex(hash)])
                .map_err(db("删除对象索引失败"))?;
            swept += 1;
            reclaimed += stored;
        }

        self.conn
            .execute(
                "UPDATE gc_jobs SET state = 'done', progress_percent = 100, swept_objects = ?2,
                   reclaimed_bytes = ?3, finished_at = ?4
                 WHERE job_id = ?1",
                params![job_id, swept as i64, reclaimed as i64, now_secs()],
            )
            .map_err(db("更新 GC 任务失败"))?;

        self.gc_status_raw(job_id)
    }

    pub fn gc_status(
        &self,
        session: &Session,
        name_or_id: &str,
        job_id: &str,
    ) -> Result<GcJobStatus> {
        let repo = self.require_repo(session, name_or_id)?;
        let owner: Option<String> = self
            .conn
            .query_row(
                "SELECT repo_id FROM gc_jobs WHERE job_id = ?1",
                params![job_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询 GC 任务失败"))?;
        match owner {
            Some(id) if id == repo.repo_id => self.gc_status_raw(job_id),
            Some(_) => Err(Error::new(
                ErrorCode::PermissionDenied,
                "不能查看其它仓库的 GC 任务",
            )),
            None => Err(Error::new(ErrorCode::InvalidSession, "GC 任务不存在")),
        }
    }

    fn gc_status_raw(&self, job_id: &str) -> Result<GcJobStatus> {
        self.conn
            .query_row(
                "SELECT job_id, state, progress_percent, marked_objects, swept_objects,
                        reclaimed_bytes, error
                 FROM gc_jobs WHERE job_id = ?1",
                params![job_id],
                |r| {
                    Ok(GcJobStatus {
                        job_id: r.get(0)?,
                        state: r.get(1)?,
                        progress_percent: r.get::<_, i64>(2)? as u32,
                        marked_objects: r.get::<_, i64>(3)? as u64,
                        swept_objects: r.get::<_, i64>(4)? as u64,
                        reclaimed_bytes: r.get::<_, i64>(5)? as u64,
                        error: r.get(6)?,
                    })
                },
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询 GC 任务失败：{e}")))
    }

    /// 可清理目录候选：HEAD 的顶层目录 + 各自独占大小。
    ///
    /// 用于 `QUOTA_EXCEEDED` 时直接给用户「清理 202610 可释放 12.4 GB」的建议。
    pub fn list_purge_candidates(
        &self,
        session: &Session,
        name_or_id: &str,
        limit: usize,
    ) -> Result<Vec<PurgeCandidate>> {
        let repo = self.require_repo(session, name_or_id)?;
        let head_root = self.revision_root_tree(&repo.repo_id, repo.head_rev)?;
        let head_flat = tree::flatten_tree(self.objects(), &head_root)?;

        let mut logical: BTreeMap<String, u64> = BTreeMap::new();
        for entry in &head_flat {
            let dir = top_level(&entry.path);
            *logical.entry(dir).or_default() += entry.size;
        }

        let owners = self.reference_map()?;
        let sizes = self.blob_sizes()?;
        let mut unique: BTreeMap<String, u64> = BTreeMap::new();
        for (hash, refs) in &owners {
            // 独占 = 全部引用都在同一个顶层目录里（同仓库）。
            let first = &refs[0];
            let shared = refs
                .iter()
                .any(|(r, p)| r != &first.0 || top_level(p) != top_level(&first.1));
            if shared || first.0 != repo.repo_id {
                continue;
            }
            let dir = top_level(&first.1);
            if dir.is_empty() {
                continue;
            }
            *unique.entry(dir).or_default() += sizes.get(hash).copied().unwrap_or(0);
        }

        let mut out = Vec::new();
        for (dir, logical_bytes) in logical {
            if dir.is_empty() {
                continue;
            }
            let (first_seen, last_modified) = self.revision_span(&repo.repo_id, &dir)?;
            out.push(PurgeCandidate {
                path: dir.clone(),
                logical_bytes,
                unique_bytes: unique.get(&dir).copied().unwrap_or(0),
                first_seen_rev: first_seen,
                last_modified_rev: last_modified,
            });
        }
        // 独占字节多的排前面：用户最该先清理的就是它们。
        out.sort_by(|a, b| {
            b.unique_bytes
                .cmp(&a.unique_bytes)
                .then(a.path.cmp(&b.path))
        });
        out.truncate(limit.max(1));
        Ok(out)
    }

    fn count_revisions_touching(&self, repo_id: &str, prefixes: &[String]) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare("SELECT rev, path FROM revision_changes WHERE repo_id = ?1")
            .map_err(db("读取变更记录失败"))?;
        let rows = stmt
            .query_map(params![repo_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(db("读取变更记录失败"))?;
        let mut revs = std::collections::BTreeSet::new();
        for row in rows {
            let (rev, path) = row.map_err(db("读取变更记录失败"))?;
            if path_under_any(&path, prefixes) {
                revs.insert(rev);
            }
        }
        Ok(revs.len() as u64)
    }

    fn revision_span(&self, repo_id: &str, prefix: &str) -> Result<(u64, u64)> {
        let (first, last): (Option<i64>, Option<i64>) = self
            .conn
            .query_row(
                "SELECT MIN(rev), MAX(rev) FROM revision_changes
                 WHERE repo_id = ?1 AND (path = ?2 OR path LIKE ?3)",
                params![repo_id, prefix, format!("{prefix}/%")],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db("读取变更记录失败"))?;
        Ok((
            first.unwrap_or(0).max(0) as u64,
            last.unwrap_or(0).max(0) as u64,
        ))
    }

    // ---------------- 审计 ----------------

    fn audit(
        &self,
        repo_id: &str,
        actor: &str,
        action: &str,
        target: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        write_audit(
            &self.conn,
            repo_id,
            actor,
            action,
            target,
            detail,
            now_secs(),
        )
    }
}

// ---------------- 内部辅助 ----------------

/// `ldap_config` 表的一行（bind 密码仍是密文）。
struct LdapConfigRow {
    host: String,
    port: i64,
    tls_mode: String,
    base_dn: String,
    bind_dn: String,
    bind_password_enc: String,
    user_filter: String,
    group_filter: Option<String>,
    attr_username: String,
    attr_display_name: String,
    enabled: i64,
}

/// 仓库申请查询的中间结构（12 个字段，用元组会被 clippy 判为过于复杂）。
struct RepoRequestRow {
    request_id: String,
    requester_user_id: String,
    repo_name: String,
    reason: Option<String>,
    requested_quota_bytes: i64,
    status: String,
    submitted_at: i64,
    decided_at: Option<i64>,
    decided_by: Option<String>,
    decision_note: Option<String>,
    expires_at: i64,
    approved_repo_id: Option<String>,
}

/// PAT 鉴权查询的中间结构。
struct PatRow {
    token_id: String,
    user_id: String,
    expires_at: Option<i64>,
    revoked_at: Option<i64>,
    scopes: String,
}

/// 登录查询的中间结构，避免一长串元组把函数签名撑爆。
struct LoginRow {
    user_id: String,
    username: String,
    display_name: String,
    status: String,
    role: String,
    source: String,
    password_hash: Option<String>,
    token_version: i64,
}

#[derive(Clone, Copy)]
enum ObjectKindTag {
    Blob,
    Tree,
}

/// 登记一个对象；返回本次**新增**的物理字节数（已存在则为 0）。
fn index_object(
    tx: &Connection,
    store: &FileObjectStore,
    hash: &Hash,
    size: u64,
    kind: ObjectKindTag,
    rev: u64,
    now: i64,
) -> Result<u64> {
    let stored = store.stored_size(hash).unwrap_or(0);
    // 我们只在「压缩更小」时才压缩，因此 stored == size + 1 说明是按原样存的。
    let compression = if stored <= size { "zstd" } else { "none" };
    let inserted = tx
        .execute(
        "INSERT INTO objects (hash, kind, size_bytes, stored_size, compression, first_seen_rev, created_at, pending_delete)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
         ON CONFLICT(hash) DO NOTHING",
        params![
            to_hex(hash),
            match kind {
                ObjectKindTag::Blob => "blob",
                ObjectKindTag::Tree => "tree",
            },
            size as i64,
            stored as i64,
            compression,
            rev as i64,
            now
        ],
    )
    .map_err(db("写入对象索引失败"))?;
    Ok(if inserted > 0 { stored } else { 0 })
}

fn write_audit(
    conn: &Connection,
    repo_id: &str,
    actor: &str,
    action: &str,
    target: &str,
    detail: Option<&str>,
    at: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO audit_log (id, repo_id, actor, action, target, detail, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![new_id("audit"), repo_id, actor, action, target, detail, at],
    )
    .map_err(db("写入审计日志失败"))?;
    Ok(())
}

/// 提交会话有效期：2 小时。超时后客户端必须重新发起，避免会话表无限增长。
const SESSION_TTL_SECONDS: i64 = 2 * 60 * 60;

fn count_changes(changes: &[SessionChange]) -> (usize, usize, usize) {
    let mut added = 0;
    let mut modified = 0;
    let mut deleted = 0;
    for c in changes {
        match c.kind {
            ChangeKind::Add => added += 1,
            ChangeKind::Modify => modified += 1,
            ChangeKind::Delete => deleted += 1,
            _ => {}
        }
    }
    (added, modified, deleted)
}

/// 树 → 路径索引。两侧都用它做差异比较，避免到处重复 flatten 的样板代码。
fn flatten_map(
    store: &FileObjectStore,
    tree: &Hash,
) -> Result<BTreeMap<String, (Hash, u64, EntryKind)>> {
    Ok(tree::flatten_tree(store, tree)?
        .into_iter()
        .map(|e| (e.path, (e.hash, e.size, e.kind)))
        .collect())
}

/// 磁盘扫描结果 vs 服务端版本 → 变更集。
fn diff_maps(
    base: &BTreeMap<String, (Hash, u64, EntryKind)>,
    next: &BTreeMap<String, (Hash, u64, EntryKind)>,
) -> Vec<PendingChange> {
    let mut out = Vec::new();
    for (path, (hash, size, entry_kind)) in next {
        match base.get(path) {
            None => out.push(PendingChange {
                path: path.clone(),
                kind: ChangeKind::Add,
                entry_kind: *entry_kind,
                hash: Some(*hash),
                size: *size,
            }),
            Some((old, _, _)) if old != hash => out.push(PendingChange {
                path: path.clone(),
                kind: ChangeKind::Modify,
                entry_kind: *entry_kind,
                hash: Some(*hash),
                size: *size,
            }),
            Some(_) => {}
        }
    }
    for (path, (hash, _, _)) in base {
        if !next.contains_key(path) {
            let _ = hash;
            out.push(PendingChange {
                path: path.clone(),
                kind: ChangeKind::Delete,
                entry_kind: EntryKind::File,
                hash: None,
                size: 0,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// 按扫描结果读取条目字节（符号链接读链接目标）。
fn read_entry_bytes(root: &Path, rel: &str, kind: EntryKind) -> Result<Vec<u8>> {
    let path = root.join(rel);
    match kind {
        EntryKind::Symlink => {
            let target = fs::read_link(&path).map_err(|e| io("读取符号链接失败", &path, e))?;
            Ok(target.to_string_lossy().as_bytes().to_vec())
        }
        _ => fs::read(&path).map_err(|e| io("读取文件失败", &path, e)),
    }
}

/// 第一遍扫描：递归采集目录内容，只计算地址与大小，不写任何对象。
///
/// 之所以拆成两遍：配额预检必须在写 CAS 之前完成。如果一边扫一边写，
/// 被配额拒绝的提交也会在盘上留下没人引用的对象。
fn scan_dir(
    root: &Path,
    prefix: &str,
    out: &mut BTreeMap<String, (Hash, u64, EntryKind)>,
) -> Result<()> {
    let entries = fs::read_dir(root).map_err(|e| io("读取目录失败", root, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io("读取目录失败", root, e))?;
        let name = entry.file_name().to_string_lossy().to_string();
        // 版本控制元目录不进版本库。
        if prefix.is_empty() && (name == ".vcs" || name == ".svn" || name == ".git") {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| io("读取条目类型失败", &path, e))?;

        if file_type.is_dir() {
            scan_dir(&path, &rel, out)?;
            continue;
        }

        if file_type.is_symlink() {
            let target = fs::read_link(&path).map_err(|e| io("读取符号链接失败", &path, e))?;
            let data = target.to_string_lossy().as_bytes().to_vec();
            let hash = hash_bytes(&data);
            out.insert(rel, (hash, data.len() as u64, EntryKind::Symlink));
            continue;
        }

        let data = fs::read(&path).map_err(|e| io("读取文件失败", &path, e))?;
        let hash = hash_bytes(&data);
        out.insert(rel, (hash, data.len() as u64, EntryKind::File));
    }
    Ok(())
}

/// 本次提交预计新增的物理字节数：同一份内容只算一次，已在对象池中的算 0。
fn estimate_new_bytes(
    scanned: &BTreeMap<String, (Hash, u64, EntryKind)>,
    store: &FileObjectStore,
) -> u64 {
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0u64;
    for (hash, size, _) in scanned.values() {
        if !seen.insert(*hash) {
            continue;
        }
        if !store.exists(hash) {
            total += size;
        }
    }
    total
}

/// 老库升级：把「软 / 硬两条线」合并成单条 `quota_bytes`。
///
/// `CREATE TABLE IF NOT EXISTS` 不会改已存在的表，所以必须在这里显式补列并回填：
/// 旧库真正参与判定的是 `quota_hard_bytes`，它就是那条唯一的线；
/// 全局默认同理，从 `default_quota_hard_bytes` 复制过来。
/// 旧列不删（SQLite 的 DROP COLUMN 版本依赖太强），代码不再读写它们。
fn migrate_single_quota(conn: &Connection) -> Result<()> {
    // 已经是新库（列存在）就不用再迁移，也避免把管理员后来显式设的 0 覆盖回去。
    if table_has_column(conn, "repos", "quota_bytes")? {
        return Ok(());
    }
    conn.execute_batch("ALTER TABLE repos ADD COLUMN quota_bytes BIGINT NOT NULL DEFAULT 0;")
        .map_err(|e| Error::new(ErrorCode::IoError, format!("升级 repos 配额列失败：{e}")))?;
    // 只有老库才有 quota_hard_bytes，它才是旧版本真正参与判定的那条线。
    conn.execute(
        "UPDATE repos SET quota_bytes = quota_hard_bytes WHERE quota_hard_bytes > 0",
        [],
    )
    .map_err(|e| Error::new(ErrorCode::IoError, format!("回填单仓库配额失败：{e}")))?;

    // 全局默认同理。schema 建表时已经写了 '0'，所以这里要用 upsert 覆盖，
    // 不能 INSERT OR IGNORE —— 否则旧值永远搬不过来。
    conn.execute(
        "INSERT INTO system_settings (key, value, updated_at, updated_by)
         SELECT 'default_quota_bytes', value, updated_at, updated_by
         FROM system_settings
         WHERE key = 'default_quota_hard_bytes' AND value <> '0'
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
           updated_at = excluded.updated_at, updated_by = excluded.updated_by",
        [],
    )
    .map_err(|e| Error::new(ErrorCode::IoError, format!("迁移全局默认配额失败：{e}")))?;
    Ok(())
}

/// 老库升级：全局身份从「只读 / 可写 / 管理员」收敛为「普通用户 / 管理员」。
///
/// SQLite 改不了已存在的 CHECK 约束，只能重建表：
/// `users.role` 与 `ldap_group_roles.role` 里的 `readonly` / `writer` 一律落成 `user`，
/// `admin` 保持不变。重建期间关掉外键（`pat_tokens.user_id` 指向 `users`），
/// 否则 DROP 老表会把令牌一起级联删掉。
fn migrate_user_roles(conn: &Connection) -> Result<()> {
    let ddl: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'users'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| Error::new(ErrorCode::IoError, format!("读取 users 表结构失败：{e}")))?;
    // 新库（或已经迁移过）的约束里只有 'user' / 'admin'，不需要动。
    let Some(ddl) = ddl else { return Ok(()) };
    if !ddl.contains("'readonly'") {
        return Ok(());
    }

    conn.pragma_update(None, "foreign_keys", "OFF")
        .map_err(|e| Error::new(ErrorCode::IoError, format!("临时关闭外键失败：{e}")))?;
    let outcome = conn.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE users_upgrade (
           user_id        TEXT PRIMARY KEY,
           username       TEXT NOT NULL UNIQUE,
           display_name   TEXT NOT NULL,
           source         TEXT NOT NULL CHECK (source IN ('local', 'ldap')),
           status         TEXT NOT NULL DEFAULT 'pending'
                          CHECK (status IN ('pending', 'active', 'disabled', 'rejected', 'expired')),
           role           TEXT NOT NULL DEFAULT 'user'
                          CHECK (role IN ('user', 'admin')),
           password_hash  TEXT,
           ldap_dn        TEXT,
           token_version  INTEGER NOT NULL DEFAULT 1,
           failed_logins  INTEGER NOT NULL DEFAULT 0,
           locked_until   BIGINT,
           created_at     BIGINT NOT NULL,
           approved_by    TEXT,
           approved_at    BIGINT,
           last_login_at  BIGINT
         );
         INSERT INTO users_upgrade
           (user_id, username, display_name, source, status, role, password_hash, ldap_dn,
            token_version, failed_logins, locked_until, created_at, approved_by, approved_at,
            last_login_at)
         SELECT user_id, username, display_name, source, status,
                CASE WHEN role = 'admin' THEN 'admin' ELSE 'user' END,
                password_hash, ldap_dn, token_version, failed_logins, locked_until,
                created_at, approved_by, approved_at, last_login_at
         FROM users;
         DROP TABLE users;
         ALTER TABLE users_upgrade RENAME TO users;
         CREATE INDEX IF NOT EXISTS idx_users_status ON users (status, source);
         CREATE INDEX IF NOT EXISTS idx_users_ldap_dn ON users (ldap_dn);
         CREATE TABLE ldap_group_roles_upgrade (
           group_dn   TEXT PRIMARY KEY,
           role       TEXT NOT NULL CHECK (role IN ('user', 'admin')),
           updated_at BIGINT NOT NULL
         );
         INSERT INTO ldap_group_roles_upgrade (group_dn, role, updated_at)
         SELECT group_dn, CASE WHEN role = 'admin' THEN 'admin' ELSE 'user' END, updated_at
         FROM ldap_group_roles;
         DROP TABLE ldap_group_roles;
         ALTER TABLE ldap_group_roles_upgrade RENAME TO ldap_group_roles;
         COMMIT;",
    );
    // 失败时先把事务回滚，否则连接会停在未结束的事务里。
    if outcome.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
    }
    // 无论成败都要把外键开关恢复，后续业务依赖它。
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| Error::new(ErrorCode::IoError, format!("恢复外键失败：{e}")))?;
    outcome.map_err(|e| Error::new(ErrorCode::IoError, format!("升级全局身份字段失败：{e}")))?;
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| Error::new(ErrorCode::IoError, format!("读取 {table} 结构失败：{e}")))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| Error::new(ErrorCode::IoError, format!("读取 {table} 结构失败：{e}")))?;
    while let Some(row) = rows
        .next()
        .map_err(|e| Error::new(ErrorCode::IoError, format!("读取 {table} 结构失败：{e}")))?
    {
        // PRAGMA table_info 的第 1 列是列名。
        let name: String = row
            .get(1)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取 {table} 列名失败：{e}")))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_or_create_secret(root: &Path) -> Result<Vec<u8>> {
    let path = root.join("server.secret");
    if let Ok(existing) = fs::read(&path) {
        if existing.len() >= 32 {
            return Ok(existing);
        }
    }
    let secret = auth::random_token(48).into_bytes();
    fs::write(&path, &secret).map_err(|e| io("写入签名密钥失败", &path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(secret)
}

/// 规范化清理前缀：走与提交一致的路径校验，空路径直接拒掉。
fn normalize_prefixes(paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Err(Error::new(
            ErrorCode::PathConflict,
            "至少需要一个要清理的路径",
        ));
    }
    let mut out: Vec<String> = Vec::new();
    for path in paths {
        let normalized = core_types::normalize_path(path)?;
        if normalized.is_empty() {
            return Err(Error::new(ErrorCode::PathConflict, "不能清理仓库根目录"));
        }
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out.sort();
    Ok(out)
}

/// `path` 是否落在任一前缀之下（含与某一前缀完全相等）。
///
/// 只做字面前缀匹配，不做通配：M2 的清理入口是用户在目录树上点选，
/// 通配 / 正则留给 `retention_policies`（设计 9.7）。这里刻意不分配字符串，
/// 因为它在可达性判定里会被调用到「对象数 × 引用数」量级。
fn path_under_any(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        path == p.as_str()
            || (path.len() > p.len()
                && path.starts_with(p.as_str())
                && path.as_bytes()[p.len()] == b'/')
    })
}

/// 路径的顶层目录名；根目录下的散文件返回空串。
fn top_level(path: &str) -> String {
    match path.split_once('/') {
        Some((head, _)) => head.to_string(),
        None => String::new(),
    }
}

fn new_id(prefix: &str) -> String {
    // base64url 会带 - 和 _，拼进 ID 后视觉上很脏；只留字母数字。
    let raw = auth::random_token(12);
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect();
    format!("{prefix}_{cleaned}")
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 字节数的可读形式，只用于错误提示与审计文案。
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn validate_username(username: &str) -> Result<()> {
    let ok = (3..=32).contains(&username.chars().count())
        && username.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.'
        })
        && username
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if !ok {
        return Err(Error::new(
            ErrorCode::InvalidCredentials,
            "用户名需 3–32 位，仅允许小写字母、数字与 _ - .，且以字母或数字开头",
        ));
    }
    Ok(())
}

fn validate_repo_name(name: &str) -> Result<()> {
    let ok = (2..=64).contains(&name.chars().count())
        && name.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.'
        })
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !ok {
        return Err(Error::new(
            ErrorCode::RepoNameInvalid,
            "仓库名需 2–64 位，仅允许小写字母、数字与 - _ .，且以字母或数字开头",
        ));
    }
    Ok(())
}

fn describe_password_issues(issues: &[core_types::PasswordPolicyError]) -> String {
    use core_types::PasswordPolicyError::*;
    issues
        .iter()
        .map(|i| match i {
            TooShort { min, actual } => format!("至少 {min} 位（当前 {actual} 位）"),
            MissingDigit => "缺少数字".to_string(),
            MissingUppercase => "缺少大写字母".to_string(),
            MissingLowercase => "缺少小写字母".to_string(),
            MissingSpecial => "缺少特殊字符".to_string(),
        })
        .collect::<Vec<_>>()
        .join("、")
}

fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::User => "user",
    }
}

fn parse_role(s: &str) -> Result<Role> {
    match s {
        "admin" => Ok(Role::Admin),
        // 老库里的 'readonly' / 'writer' 都是普通用户：读写权限现在按仓库授予，
        // 全局身份只剩「普通用户 / 管理员」两种。
        "user" | "readonly" | "writer" => Ok(Role::User),
        other => Err(Error::new(
            ErrorCode::InvalidCredentials,
            format!("未知角色：{other}"),
        )),
    }
}

fn parse_account_status(s: &str) -> Result<core_types::AccountStatus> {
    match s {
        "pending" => Ok(core_types::AccountStatus::Pending),
        "active" => Ok(core_types::AccountStatus::Active),
        "disabled" => Ok(core_types::AccountStatus::Disabled),
        "rejected" => Ok(core_types::AccountStatus::Rejected),
        "expired" => Ok(core_types::AccountStatus::Expired),
        other => Err(Error::new(
            ErrorCode::InvalidCredentials,
            format!("未知账号状态：{other}"),
        )),
    }
}

fn kind_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
    }
}

fn parse_kind(s: &str) -> Result<EntryKind> {
    match s {
        "file" => Ok(EntryKind::File),
        "dir" => Ok(EntryKind::Dir),
        "symlink" => Ok(EntryKind::Symlink),
        other => Err(Error::new(
            ErrorCode::IoError,
            format!("未知条目类型：{other}"),
        )),
    }
}

/// 把某个用户在某个仓库里未完成的提交会话全部作废，返回影响行数。
///
/// 成员被移除 / 降级为只读 / 仓库被禁用时都要调用：只靠 `FinalizeCommit` 的二次校验
/// 虽然能拦住结果，但用户是在点了提交之后才失败，白等一次上传。
fn abort_sessions_of(tx: &rusqlite::Transaction<'_>, repo_id: &str, user_id: &str) -> Result<i64> {
    let affected = tx
        .execute(
            "UPDATE upload_sessions SET state = 'aborted'
             WHERE repo_id = ?1 AND client_id = ?2 AND state IN ('open', 'pushing')",
            params![repo_id, user_id],
        )
        .map_err(db("中止在途提交失败"))?;
    Ok(affected as i64)
}

fn repo_status_str(status: RepoStatus) -> &'static str {
    match status {
        RepoStatus::Active => "active",
        RepoStatus::Disabled => "disabled",
        RepoStatus::Archived => "archived",
    }
}

fn member_role_str(role: RepoMemberRole) -> &'static str {
    match role {
        RepoMemberRole::Owner => "owner",
        RepoMemberRole::Maintainer => "maintainer",
        RepoMemberRole::Writer => "writer",
        RepoMemberRole::Reader => "reader",
    }
}

fn parse_member_role(s: &str) -> Result<RepoMemberRole> {
    match s {
        "owner" => Ok(RepoMemberRole::Owner),
        "maintainer" => Ok(RepoMemberRole::Maintainer),
        "writer" => Ok(RepoMemberRole::Writer),
        "reader" => Ok(RepoMemberRole::Reader),
        other => Err(Error::new(
            ErrorCode::RepoMemberNotFound,
            format!("未知成员角色：{other}"),
        )),
    }
}

fn parse_repo_status(s: &str) -> Result<RepoStatus> {
    match s {
        "active" => Ok(RepoStatus::Active),
        "disabled" => Ok(RepoStatus::Disabled),
        "archived" => Ok(RepoStatus::Archived),
        other => Err(Error::new(
            ErrorCode::RepoNotFound,
            format!("未知仓库状态：{other}"),
        )),
    }
}

fn change_kind_str(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Add => "add",
        ChangeKind::Modify => "modify",
        ChangeKind::Delete => "delete",
        ChangeKind::Rename => "rename",
        ChangeKind::Copy => "copy",
    }
}

fn parse_change_kind(s: &str) -> Result<ChangeKind> {
    match s {
        "add" => Ok(ChangeKind::Add),
        "modify" => Ok(ChangeKind::Modify),
        "delete" => Ok(ChangeKind::Delete),
        "rename" => Ok(ChangeKind::Rename),
        "copy" => Ok(ChangeKind::Copy),
        other => Err(Error::new(
            ErrorCode::IoError,
            format!("未知变更类型：{other}"),
        )),
    }
}

fn db(action: &'static str) -> impl Fn(rusqlite::Error) -> Error {
    move |e| Error::new(ErrorCode::IoError, format!("{action}：{e}"))
}

// ---------------- 用户体系常量 ----------------

/// 「全库第一个账号」的引导名额，**必须在 `BEGIN IMMEDIATE` 事务内判定**。
///
/// 自助注册与 LDAP 首次登录共用这一处：谁先在事务里看到空用户表，谁拿走
/// 管理员名额，另一条入口立刻退化为普通流程。除了用户表，还检查引导审计
/// 记录——即便有人手工删掉首个用户，也不能让第二个来抢注的人再拿一次管理员。
fn first_user_slot(tx: &Connection) -> Result<bool> {
    let users: i64 = tx
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .map_err(db("统计用户失败"))?;
    if users > 0 {
        return Ok(false);
    }
    let claimed: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action IN
             ('user.bootstrap', 'user.bootstrap_rescue',
              'user.first_admin.register', 'user.first_admin.ldap')",
            [],
            |r| r.get(0),
        )
        .map_err(db("查询引导记录失败"))?;
    Ok(claimed == 0)
}

fn role_rank(role: Role) -> u8 {
    match role {
        Role::User => 1,
        Role::Admin => 2,
    }
}

/// PAT 前缀：`authenticate` 靠它区分 JWT 与访问令牌，不必试错解析。
const PAT_PREFIX: &str = "vcs_pat_";

const SCOPE_READ: &str = "read";
const SCOPE_WRITE: &str = "write";
const SCOPE_ADMIN: &str = "admin";
const ALLOWED_SCOPES: [&str; 3] = [SCOPE_READ, SCOPE_WRITE, SCOPE_ADMIN];

/// 注册申请有效期：7 天。超期未审批自动 EXPIRED，用户需要重新申请。
const REGISTRATION_TTL_SECONDS: i64 = 7 * 86_400;
/// 仓库创建申请有效期：7 天。
const REPO_REQUEST_TTL_SECONDS: i64 = 7 * 86_400;

fn reg_status_str(status: core_types::RegistrationStatus) -> &'static str {
    match status {
        core_types::RegistrationStatus::Pending => "pending",
        core_types::RegistrationStatus::Approved => "approved",
        core_types::RegistrationStatus::Rejected => "rejected",
        core_types::RegistrationStatus::Expired => "expired",
    }
}

fn parse_reg_status(value: &str) -> Result<core_types::RegistrationStatus> {
    match value {
        "pending" => Ok(core_types::RegistrationStatus::Pending),
        "approved" => Ok(core_types::RegistrationStatus::Approved),
        "rejected" => Ok(core_types::RegistrationStatus::Rejected),
        "expired" => Ok(core_types::RegistrationStatus::Expired),
        other => Err(Error::new(
            ErrorCode::IoError,
            format!("未知的注册状态：{other}"),
        )),
    }
}

fn creation_status_str(status: core_types::RepoCreationStatus) -> &'static str {
    match status {
        core_types::RepoCreationStatus::Pending => "pending",
        core_types::RepoCreationStatus::Approved => "approved",
        core_types::RepoCreationStatus::Rejected => "rejected",
        core_types::RepoCreationStatus::Expired => "expired",
    }
}

fn parse_creation_status(value: &str) -> Result<core_types::RepoCreationStatus> {
    match value {
        "pending" => Ok(core_types::RepoCreationStatus::Pending),
        "approved" => Ok(core_types::RepoCreationStatus::Approved),
        "rejected" => Ok(core_types::RepoCreationStatus::Rejected),
        "expired" => Ok(core_types::RepoCreationStatus::Expired),
        other => Err(Error::new(
            ErrorCode::IoError,
            format!("未知的仓库申请状态：{other}"),
        )),
    }
}

/// 审计 detail 里的字符串转义：用户填写的理由可能带引号，直接拼 JSON 会写出坏数据。
fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

fn map_unique_as(e: rusqlite::Error, code: ErrorCode, hint: &str) -> Error {
    let text = e.to_string();
    if text.contains("UNIQUE") {
        Error::new(code, hint.to_string())
    } else {
        Error::new(ErrorCode::IoError, text)
    }
}

fn map_unique(e: rusqlite::Error, hint: &str) -> Error {
    let text = e.to_string();
    if text.contains("UNIQUE") {
        Error::new(ErrorCode::RepoNameInvalid, hint.to_string())
    } else {
        Error::new(ErrorCode::IoError, text)
    }
}

fn io(action: &str, path: &Path, err: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::IoError,
        format!("{action}：{}（{err}）", path.display()),
    )
}
