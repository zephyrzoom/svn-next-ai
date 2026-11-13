-- svn-next 服务端元数据 schema（M0）
-- 存储后端固定为本地文件系统 CAS + 单机 SQLite，不引入 Postgres / S3 等外部存储；
-- 全仓库采用单一共享 CAS（SHARED_CAS），不提供按月份分区隔离布局。
-- 数值统一用 BIGINT 以免 32 位溢出。
-- 时间戳一律为 Unix 秒（BIGINT），JSON 字段用 TEXT 存储。
-- 原则：revision 元数据永久保留，CAS 内容字节可按路径裁剪回收。

PRAGMA foreign_keys = ON;

-- ---------------- 仓库与版本 ----------------

CREATE TABLE IF NOT EXISTS repos (
  repo_id               TEXT PRIMARY KEY,
  name                  TEXT NOT NULL UNIQUE,
  head_rev              BIGINT NOT NULL DEFAULT 0,
  status                TEXT NOT NULL DEFAULT 'active'
                        CHECK (status IN ('active', 'disabled', 'archived')),
  owner_user_id         TEXT NOT NULL,  -- 审批通过的申请人，或直通创建的管理员
  created_from_request  TEXT,           -- 关联的 repo_creation_requests.request_id
  quota_bytes           BIGINT NOT NULL DEFAULT 0,  -- 配额上限（唯一一条线）；0 表示继承全局默认配额
  grace_days            INTEGER NOT NULL DEFAULT 7, -- 回收站宽限期
  created_at            BIGINT NOT NULL,
  updated_at            BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_repos_owner ON repos (owner_user_id, status);

-- ---------------- 服务端级配置 ----------------

-- 全局默认配额：所有未单独设置配额的仓库继承这里的值；仅管理员可改，变更写 audit_log。
CREATE TABLE IF NOT EXISTS system_settings (
  key         TEXT PRIMARY KEY,
  value       TEXT NOT NULL,
  updated_at  BIGINT NOT NULL,
  updated_by  TEXT NOT NULL
);

-- 预置键：default_quota_bytes，值为 0 表示尚未配置。
-- 两级都未配置时由 vcs-core 内置兜底常量兜底，保证始终存在上限。
INSERT OR IGNORE INTO system_settings (key, value, updated_at, updated_by)
VALUES ('default_quota_bytes', '0', 0, 'bootstrap');

-- ---------------- 仓库创建申请 ----------------

-- 未审批的申请只是一张单据：不分配 repo_id、不创建 CAS 目录、不能调用 RepoService。
-- 审批通过后在单事务内插入 repos(active) + repo_members 的 owner 行 + 更新本申请 + 审计日志。
-- requested_quota_bytes 由服务端固定写成 10 GiB（vcs-core::REPO_INITIAL_QUOTA_BYTES），
-- 申请人与审批人都不填容量：建库申请只回答「要不要建」。
CREATE TABLE IF NOT EXISTS repo_creation_requests (
  request_id            TEXT PRIMARY KEY,
  requester_user_id     TEXT NOT NULL,
  repo_name             TEXT NOT NULL,
  repo_id_hint          TEXT,
  reason                TEXT,
  requested_quota_bytes BIGINT NOT NULL DEFAULT 0,
  status                TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
  submitted_at          BIGINT NOT NULL,
  decided_at            BIGINT,
  decided_by            TEXT,
  decision_note         TEXT,
  expires_at            BIGINT NOT NULL,
  approved_repo_id      TEXT
);

CREATE INDEX IF NOT EXISTS idx_repo_req_requester ON repo_creation_requests (requester_user_id, status);
CREATE INDEX IF NOT EXISTS idx_repo_req_pending ON repo_creation_requests (status, submitted_at);

-- ---------------- 仓库扩容申请 ----------------

-- 已建仓库要加空间时提交的单据：容量不由申请人填写，服务端固定按
-- +10 GiB（vcs-core::REPO_QUOTA_EXPANSION_STEP_BYTES）计算。
-- 管理员审批时参照服务器数据卷的可用空间（管理端 /api/storage），
-- 通过后在单事务内把 repos.quota_bytes 提到 quota_before_bytes + increment_bytes，
-- 更新本申请并写审计日志。同一仓库同时只允许一条待审批扩容申请。
CREATE TABLE IF NOT EXISTS repo_quota_requests (
  request_id         TEXT PRIMARY KEY,
  repo_id            TEXT NOT NULL,
  requester_user_id  TEXT NOT NULL,
  increment_bytes    BIGINT NOT NULL,  -- 固定增量，当前为 10 GiB
  quota_before_bytes BIGINT NOT NULL,  -- 提交时的生效上限，审批台用来展示「加了多少」
  reason             TEXT,
  status             TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
  submitted_at       BIGINT NOT NULL,
  decided_at         BIGINT,
  decided_by         TEXT,
  decision_note      TEXT,
  expires_at         BIGINT NOT NULL,
  FOREIGN KEY (repo_id) REFERENCES repos (repo_id)
);

CREATE INDEX IF NOT EXISTS idx_repo_quota_req_repo ON repo_quota_requests (repo_id, status);
CREATE INDEX IF NOT EXISTS idx_repo_quota_req_state ON repo_quota_requests (status, submitted_at);

-- ---------------- 仓库成员 ----------------

-- 成员资格是使用仓库的第一道门：非成员一律不能调用 RepoService。
-- owner 也作为一行存在；repos.owner_user_id 是审批来源的冗余索引，二者在同一事务写入。
CREATE TABLE IF NOT EXISTS repo_members (
  repo_id   TEXT NOT NULL,
  user_id   TEXT NOT NULL,
  role      TEXT NOT NULL CHECK (role IN ('owner', 'maintainer', 'writer', 'reader')),
  added_by  TEXT NOT NULL,
  added_at  BIGINT NOT NULL,
  PRIMARY KEY (repo_id, user_id),
  FOREIGN KEY (repo_id) REFERENCES repos (repo_id)
);

CREATE INDEX IF NOT EXISTS idx_repo_members_user ON repo_members (user_id, repo_id);

-- 元数据永久保留：即使内容被清理，log / 作者 / 时间 / 变更列表依然完整。
CREATE TABLE IF NOT EXISTS revisions (
  repo_id       TEXT NOT NULL,
  rev           BIGINT NOT NULL,
  root_tree     TEXT NOT NULL,
  parent_rev    BIGINT NOT NULL DEFAULT 0,
  author        TEXT NOT NULL,
  message       TEXT NOT NULL DEFAULT '',
  committed_at  BIGINT NOT NULL,
  change_count  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (repo_id, rev),
  FOREIGN KEY (repo_id) REFERENCES repos (repo_id)
);

CREATE TABLE IF NOT EXISTS revision_changes (
  repo_id    TEXT NOT NULL,
  rev        BIGINT NOT NULL,
  seq        INTEGER NOT NULL,
  path       TEXT NOT NULL,
  kind       TEXT NOT NULL CHECK (kind IN ('add', 'modify', 'delete', 'rename', 'copy')),
  old_hash   TEXT,
  new_hash   TEXT,
  from_path  TEXT,
  PRIMARY KEY (repo_id, rev, seq)
);

CREATE INDEX IF NOT EXISTS idx_rev_changes_path ON revision_changes (repo_id, path);

-- ---------------- CAS 对象 ----------------

-- 内容字节本身存放在 objects/ 目录，本表只是索引与统计。
-- 全仓库单一共享 CAS：所有目录与月份共用同一对象池，跨路径全局去重。
-- 回收空间依赖 purged_paths 路径级裁剪 + GC 可达性标记，不依赖物理分区。
CREATE TABLE IF NOT EXISTS objects (
  hash            TEXT PRIMARY KEY,
  kind            TEXT NOT NULL CHECK (kind IN ('blob', 'tree')),
  size_bytes      BIGINT NOT NULL,   -- 解压后
  stored_size     BIGINT NOT NULL,   -- 实际磁盘占用（压缩后）
  compression     TEXT NOT NULL DEFAULT 'zstd',
  first_seen_rev  BIGINT NOT NULL DEFAULT 0,
  created_at      BIGINT NOT NULL,
  pending_delete  INTEGER NOT NULL DEFAULT 0  -- GC 两段式：先标记，再物理删除
);

CREATE INDEX IF NOT EXISTS idx_objects_pending ON objects (pending_delete);
CREATE INDEX IF NOT EXISTS idx_objects_kind ON objects (kind);
CREATE INDEX IF NOT EXISTS idx_objects_first_seen ON objects (first_seen_rev);

-- tree 也是 CAS 对象，这里提供独立视图便于可达性遍历。
CREATE VIEW IF NOT EXISTS trees AS
  SELECT hash, size_bytes, created_at
  FROM objects
  WHERE kind = 'tree';

CREATE VIEW IF NOT EXISTS v_cas_usage AS
  SELECT SUM(stored_size) AS used_bytes,
         COUNT(*)         AS object_count
  FROM objects
  WHERE pending_delete = 0;

-- ---------------- 容量治理 ----------------

-- 路径级裁剪标记：GC 可达性遍历时跳过 ACTIVE 的路径，
-- 不砍版本号、不影响其它目录的历史版本。
CREATE TABLE IF NOT EXISTS purged_paths (
  purge_id      TEXT PRIMARY KEY,
  repo_id       TEXT NOT NULL,
  path_pattern  TEXT NOT NULL,
  state         TEXT NOT NULL DEFAULT 'pending'
                CHECK (state IN ('pending', 'active', 'reverted')),
  reason        TEXT,
  purged_by     TEXT NOT NULL,
  created_at    BIGINT NOT NULL,
  grace_until   BIGINT,             -- NULL 表示立即生效（immediate=true）
  activated_at  BIGINT,
  FOREIGN KEY (repo_id) REFERENCES repos (repo_id)
);

CREATE INDEX IF NOT EXISTS idx_purged_paths_repo ON purged_paths (repo_id, state);

CREATE TABLE IF NOT EXISTS gc_jobs (
  job_id            TEXT PRIMARY KEY,
  repo_id           TEXT NOT NULL,
  purge_id          TEXT,
  state             TEXT NOT NULL DEFAULT 'queued'
                    CHECK (state IN ('queued', 'marking', 'sweeping', 'done', 'failed')),
  progress_percent  INTEGER NOT NULL DEFAULT 0,
  marked_objects    BIGINT NOT NULL DEFAULT 0,
  swept_objects     BIGINT NOT NULL DEFAULT 0,
  reclaimed_bytes   BIGINT NOT NULL DEFAULT 0,
  error             TEXT,
  started_at        BIGINT,
  finished_at       BIGINT
);

-- EstimatePurge / GetQuota 的预计算缓存。
-- 共享 CAS 下引用会跨目录外溢，unique_bytes 必须由 GC 标记阶段的反向引用集合精确计算，
-- 不能简单用目录逻辑大小代替。
CREATE TABLE IF NOT EXISTS quota_usage (
  repo_id            TEXT NOT NULL,
  path               TEXT NOT NULL,
  logical_bytes      BIGINT NOT NULL DEFAULT 0,
  unique_bytes       BIGINT NOT NULL DEFAULT 0,
  object_count       BIGINT NOT NULL DEFAULT 0,
  first_seen_rev     BIGINT NOT NULL DEFAULT 0,
  last_modified_rev  BIGINT NOT NULL DEFAULT 0,
  computed_at        BIGINT NOT NULL,
  PRIMARY KEY (repo_id, path)
);

CREATE TABLE IF NOT EXISTS retention_policies (
  policy_id     TEXT PRIMARY KEY,
  repo_id       TEXT NOT NULL,
  path_pattern  TEXT NOT NULL,      -- 例如 ^20\d{4}$
  keep_months   INTEGER,
  max_bytes     BIGINT,
  max_age_days  INTEGER,
  enabled       INTEGER NOT NULL DEFAULT 1,
  dry_run       INTEGER NOT NULL DEFAULT 1,
  created_at    BIGINT NOT NULL
);

-- ---------------- 提交、权限、审计 ----------------

-- 两阶段提交会话：对象先落 CAS，Finalize 才在单事务内分配版本号。
CREATE TABLE IF NOT EXISTS upload_sessions (
  session_id           TEXT PRIMARY KEY,
  repo_id              TEXT NOT NULL,
  client_id            TEXT NOT NULL,
  base_rev             BIGINT NOT NULL,
  message              TEXT NOT NULL DEFAULT '',
  changeset            TEXT NOT NULL DEFAULT '[]',  -- JSON: Change[]
  received_hashes      TEXT NOT NULL DEFAULT '[]',  -- JSON: 断点续传用
  estimated_new_bytes  BIGINT NOT NULL DEFAULT 0,
  state                TEXT NOT NULL DEFAULT 'open'
                       CHECK (state IN ('open', 'pushing', 'finalizing', 'committed', 'aborted', 'expired')),
  created_at           BIGINT NOT NULL,
  expires_at           BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sessions_expiry ON upload_sessions (state, expires_at);

-- 路径级可选细粒度授权；调用者必须先通过 repo_members 成员门。
-- 成员对自己的路径持有 purge 权限即可自助清理，无需全局管理员。
CREATE TABLE IF NOT EXISTS acl (
  repo_id      TEXT NOT NULL,
  subject      TEXT NOT NULL,
  path_prefix  TEXT NOT NULL,
  permission   TEXT NOT NULL CHECK (permission IN ('read', 'write', 'purge', 'admin')),
  PRIMARY KEY (repo_id, subject, path_prefix, permission)
);

CREATE TABLE IF NOT EXISTS locks (
  repo_id      TEXT NOT NULL,
  path         TEXT NOT NULL,
  owner        TEXT NOT NULL,
  token        TEXT NOT NULL,
  leased_until BIGINT NOT NULL,
  created_at   BIGINT NOT NULL,
  PRIMARY KEY (repo_id, path)
);

CREATE TABLE IF NOT EXISTS audit_log (
  id       TEXT PRIMARY KEY,
  repo_id  TEXT NOT NULL,
  actor    TEXT NOT NULL,
  action   TEXT NOT NULL,
  target   TEXT,
  detail   TEXT,        -- JSON
  at       BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_repo_time ON audit_log (repo_id, at);

-- ---------------- 用户体系 ----------------

-- 用户字段固定三项：username（用户名）、password_hash（密码哈希）、display_name（姓名）。
-- LDAP 用户不存密码，本地用户存 Argon2id 哈希（含随机 salt）。
-- 本地密码须 >= 8 位且同时含数字、大写、小写、特殊字符；服务端在 SetPassword 强制校验。
-- role 只有 'user'（普通用户）与 'admin'（管理员）两种：读写权限不是全局的，而是
-- 由 repo_members.role 逐仓库授予（owner / maintainer / writer / reader）。
-- token_version 在禁用账号或改角色时递增，JWT 携带该版本号实现即时失效。
CREATE TABLE IF NOT EXISTS users (
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

CREATE INDEX IF NOT EXISTS idx_users_status ON users (status, source);
CREATE INDEX IF NOT EXISTS idx_users_ldap_dn ON users (ldap_dn);

-- 自助注册申请：pending → approved / rejected / expired，审批记录写入 decided_*。
-- 密码在注册时即由本人在 users.password_hash 设好；审批只负责放行，不再发放激活链接。
CREATE TABLE IF NOT EXISTS registration_requests (
  request_id          TEXT PRIMARY KEY,
  username            TEXT NOT NULL,
  display_name        TEXT NOT NULL,  -- 姓名
  reason              TEXT,
  status              TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
  submitted_at        BIGINT NOT NULL,
  decided_at          BIGINT,
  decided_by          TEXT,
  decision_note       TEXT,
  expires_at          BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_reg_pending ON registration_requests (status, submitted_at);

-- LDAP 连接配置：bind 密码以主密钥加密后存储，读取接口不回传明文。
-- user_filter 中的 {username} 占位符在拼接前必须做 LDAP 转义，防注入。
CREATE TABLE IF NOT EXISTS ldap_config (
  config_id          TEXT PRIMARY KEY,
  host               TEXT NOT NULL,
  port               INTEGER NOT NULL DEFAULT 636,
  tls_mode           TEXT NOT NULL DEFAULT 'ldaps'
                     CHECK (tls_mode IN ('ldaps', 'starttls')),
  base_dn            TEXT NOT NULL,
  bind_dn            TEXT NOT NULL,
  bind_password_enc  TEXT NOT NULL,
  user_filter        TEXT NOT NULL,
  group_filter       TEXT,
  attr_username      TEXT NOT NULL DEFAULT 'uid',
  attr_display_name  TEXT NOT NULL DEFAULT 'cn',  -- 姓名
  enabled            INTEGER NOT NULL DEFAULT 1,
  updated_at         BIGINT NOT NULL,
  updated_by         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ldap_group_roles (
  group_dn   TEXT PRIMARY KEY,
  role       TEXT NOT NULL CHECK (role IN ('user', 'admin')),
  updated_at BIGINT NOT NULL
);

-- PAT：明文只在创建时返回一次，库里只存哈希。
CREATE TABLE IF NOT EXISTS pat_tokens (
  token_id     TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL,
  name         TEXT NOT NULL,
  token_hash   TEXT NOT NULL,
  scopes       TEXT NOT NULL DEFAULT '[]',
  created_at   BIGINT NOT NULL,
  expires_at   BIGINT,
  revoked_at   BIGINT,
  last_used_at BIGINT,
  FOREIGN KEY (user_id) REFERENCES users (user_id)
);

CREATE INDEX IF NOT EXISTS idx_pat_user ON pat_tokens (user_id);
