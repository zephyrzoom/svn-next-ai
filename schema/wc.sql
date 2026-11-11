-- svn-next 工作副本 wc.db schema（M0）
-- 位置：<working-copy>/.vcs/wc.db
-- 配套目录：<working-copy>/.vcs/pristine/ 按哈希缓存基线内容
-- 原则：status/diff/revert 完全离线；每个条目自带 base_rev 以支持混合版本工作副本。

CREATE TABLE IF NOT EXISTS meta (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
);

-- 预置键：
--   format_version, repo_id, repo_url, wc_uuid,
--   last_contact_rev, head_rev, auth_subject

-- ---------------- 条目 ----------------

CREATE TABLE IF NOT EXISTS entries (
  path         TEXT PRIMARY KEY,          -- 工作副本内规范化相对路径，以 / 分隔
  kind         TEXT NOT NULL CHECK (kind IN ('file', 'dir', 'symlink')),
  base_rev     BIGINT NOT NULL,           -- 混合版本：各条目可不同
  base_hash    TEXT,                     -- 基线内容哈希，指向 pristine
  base_props   BLOB,
  state        TEXT NOT NULL DEFAULT 'normal'
               CHECK (state IN ('normal', 'added', 'modified', 'deleted',
                                'replaced', 'conflicted', 'ignored', 'incomplete')),
  depth        TEXT NOT NULL DEFAULT 'infinity'
               CHECK (depth IN ('infinity', 'immediates', 'files', 'empty')),
  mtime        BIGINT NOT NULL DEFAULT 0,
  size         BIGINT NOT NULL DEFAULT 0,
  checksum     TEXT,                     -- 上次计算的本地哈希，用于 status 快路径
  lock_token   TEXT,
  conflict_id  TEXT,
  present      INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_entries_state ON entries (state);
CREATE INDEX IF NOT EXISTS idx_entries_base_rev ON entries (base_rev);
CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries (path);

-- ---------------- 基线缓存 ----------------

CREATE TABLE IF NOT EXISTS pristine (
  hash         TEXT PRIMARY KEY,
  size_bytes   BIGINT NOT NULL,
  stored_size  BIGINT NOT NULL,
  ref_count    INTEGER NOT NULL DEFAULT 1,
  path_hint    TEXT
);

-- ---------------- 冲突 ----------------

-- 冲突不写标记文件：保存 base/mine/theirs 三个哈希，供 GUI 三栏解决。
CREATE TABLE IF NOT EXISTS conflicts (
  id           TEXT PRIMARY KEY,
  path         TEXT NOT NULL,
  kind         TEXT NOT NULL CHECK (kind IN ('text', 'binary', 'tree')),
  base_hash    TEXT,
  mine_hash    TEXT,
  theirs_hash  TEXT,
  detail       TEXT,                     -- JSON：树冲突类型等
  created_at   BIGINT NOT NULL,
  resolved_at  BIGINT
);

CREATE INDEX IF NOT EXISTS idx_conflicts_path ON conflicts (path);

-- ---------------- 本地状态辅助 ----------------

-- status 快路径：先比 mtime/size，仅可疑条目重算哈希。
CREATE TABLE IF NOT EXISTS local_changes (
  path         TEXT PRIMARY KEY,
  reason       TEXT NOT NULL,
  detected_at  BIGINT NOT NULL
);

-- 服务端 purged_paths 的本地缓存：检出旧版本遇到已清理内容时
-- 返回 CONTENT_PURGED 并给出「该内容已按容量策略清理」的友好提示。
CREATE TABLE IF NOT EXISTS wc_purged_paths (
  path_pattern TEXT PRIMARY KEY,
  synced_at    BIGINT NOT NULL
);

-- 原子/可恢复的工作副本操作（checkout、update 中断后继续）。
CREATE TABLE IF NOT EXISTS pending_ops (
  op_id       TEXT PRIMARY KEY,
  kind        TEXT NOT NULL,
  path        TEXT NOT NULL,
  payload     TEXT,
  state       TEXT NOT NULL DEFAULT 'pending'
              CHECK (state IN ('pending', 'running', 'done', 'failed')),
  created_at  BIGINT NOT NULL
);
