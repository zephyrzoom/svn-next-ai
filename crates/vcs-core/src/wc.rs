//! 工作副本（working copy）。
//!
//! 结构：`<wc>/.vcs/wc.db`（SQLite）+ `<wc>/.vcs/pristine/<ab>/<cdef…>`（基线缓存）。
//!
//! 两个关键性质：
//!   1. **完全离线**：`status` / `diff` 只读本地 wc.db 与 pristine，不访问服务端；
//!   2. **混合版本**：每个条目自带 `base_rev`，检出部分路径后不会把整份工作副本
//!      的版本号一起改写，这是 update 单个目录、以及路径级权限能落地的前提。
//!
//! `status` 走 mtime/size 快路径：内容没动过的文件连哈希都不算。
//! 只有 mtime 或 size 变化的条目才重算 blake3，避免每次刷新把整个工作副本读一遍。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::hash::{hash_bytes, object_rel_path, to_hex};
use crate::{ChangeKind, EntryKind, Error, ErrorCode, FlatEntry, Hash, ObjectStore, Result};

const WC_SCHEMA: &str = include_str!("../../../schema/wc.sql");
const VCS_DIR: &str = ".vcs";
pub const WC_FORMAT_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WcState {
    Normal,
    Added,
    Modified,
    Deleted,
    Conflicted,
    Unversioned,
}

impl WcState {
    /// 与 `wc.sql` 中 `entries.state` 的取值保持一致。
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Conflicted => "conflicted",
            Self::Unversioned => "ignored",
        }
    }

    pub fn marker(&self) -> char {
        match self {
            Self::Normal => ' ',
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Conflicted => 'C',
            Self::Unversioned => '?',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    pub state: WcState,
    pub base_rev: u64,
}

pub struct WorkingCopy {
    root: PathBuf,
    conn: Connection,
}

impl WorkingCopy {
    /// 新建工作副本目录结构。已存在则直接打开，避免重复检出时报错。
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let meta = root.join(VCS_DIR);
        fs::create_dir_all(meta.join("pristine"))
            .map_err(|e| io("创建工作副本目录失败", &meta, e))?;
        let conn =
            Connection::open(meta.join("wc.db")).map_err(|e| io("打开 wc.db 失败", &meta, e))?;
        conn.execute_batch(WC_SCHEMA)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("初始化 wc.db 失败：{e}")))?;
        let wc = Self { root, conn };
        wc.set_meta("format_version", WC_FORMAT_VERSION)?;
        wc.set_meta("wc_uuid", &crate::auth::random_token(16))?;
        Ok(wc)
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let db = root.join(VCS_DIR).join("wc.db");
        if !db.exists() {
            return Err(Error::new(
                ErrorCode::InvalidSession,
                format!("{} 不是有效的工作副本（缺少 .vcs/wc.db）", root.display()),
            ));
        }
        let conn = Connection::open(&db).map_err(|e| io("打开 wc.db 失败", &db, e))?;
        Ok(Self { root, conn })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("写 meta 失败：{e}")))?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读 meta 失败：{e}")))
    }

    /// 当前工作副本记录的版本号（最近一次检出/更新的基准）。
    pub fn base_rev(&self) -> Result<u64> {
        Ok(self
            .get_meta("head_rev")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    /// 工作副本里最旧的基线版本。
    ///
    /// 混合版本下 `update` 必须从这个版本取差异，而不是从 `head_rev`：例如某个文件
    /// 因冲突被 `revert` 回旧基线后，它仍停在旧版本上，只看 head 会以为「已经最新」，
    /// 那部分差异就永远拉不下来。取所有条目 `base_rev` 与 head 的最小值最稳妥——
    /// 多算的差异是幂等的，漏算的差异会永久留在工作副本里。
    pub fn oldest_rev(&self) -> Result<u64> {
        let head = self.base_rev()?;
        let min_entry: Option<i64> = self
            .conn
            .query_row("SELECT MIN(base_rev) FROM entries", [], |r| r.get(0))
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目版本失败：{e}")))?;
        Ok(min_entry
            .map(|v| (v.max(0) as u64).min(head))
            .unwrap_or(head))
    }

    pub fn read_pristine(&self, hash: &Hash) -> Result<Vec<u8>> {
        let path = pristine_path(&self.root, hash);
        fs::read(&path).map_err(|_| {
            Error::new(
                ErrorCode::ObjectMissing,
                format!("基线内容未缓存：{}", to_hex(hash)),
            )
        })
    }

    /// 检出：把给定版本的完整树铺到磁盘，并写入 wc.db 与 pristine 缓存。
    ///
    /// 调用方负责先把 store 里缺的对象补齐（M1 中 store 即本地服务端存储）。
    pub fn checkout(
        &mut self,
        repo_id: &str,
        rev: u64,
        files: &[FlatEntry],
        store: &dyn ObjectStore,
    ) -> Result<usize> {
        // 事务会可变借用 self.conn，因此先把只读路径摘出来。
        let wc_root = self.root.clone();
        let tx = self
            .conn
            .transaction()
            .map_err(|e| Error::new(ErrorCode::IoError, format!("开启事务失败：{e}")))?;
        tx.execute("DELETE FROM entries", [])
            .map_err(|e| Error::new(ErrorCode::IoError, format!("清理旧条目失败：{e}")))?;

        let now = now_secs();
        let mut written = 0usize;

        for entry in files {
            let data = store.get(&entry.hash)?;
            let target = wc_root.join(&entry.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| io("创建目录失败", parent, e))?;
            }
            write_entry(&target, entry.kind, &data)?;
            write_pristine(&wc_root, &entry.hash, &data)?;

            let meta = fs::symlink_metadata(&target).map_err(|e| io("读取条目失败", &target, e))?;
            tx.execute(
                "INSERT INTO entries (path, kind, base_rev, base_hash, state, mtime, size, checksum)
                 VALUES (?1, ?2, ?3, ?4, 'normal', ?5, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET
                   kind = excluded.kind, base_rev = excluded.base_rev, base_hash = excluded.base_hash,
                   state = 'normal', mtime = excluded.mtime, size = excluded.size,
                   checksum = excluded.checksum",
                params![
                    entry.path,
                    kind_str(entry.kind),
                    rev as i64,
                    to_hex(&entry.hash),
                    mtime_nanos(&meta),
                    data.len() as i64,
                    to_hex(&entry.hash),
                ],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("写 entries 失败：{e}")))?;

            tx.execute(
                "INSERT INTO pristine (hash, size_bytes, stored_size, ref_count, path_hint)
                 VALUES (?1, ?2, ?2, 1, ?3)
                 ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
                params![to_hex(&entry.hash), data.len() as i64, entry.path],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("写 pristine 失败：{e}")))?;

            written += 1;
        }

        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('repo_id', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![repo_id],
        )
        .map_err(|e| Error::new(ErrorCode::IoError, format!("写 repo_id 失败：{e}")))?;
        for (k, v) in [
            ("head_rev", rev.to_string()),
            ("last_contact_rev", rev.to_string()),
            ("checked_out_at", now.to_string()),
        ] {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("写 meta 失败：{e}")))?;
        }

        tx.commit()
            .map_err(|e| Error::new(ErrorCode::IoError, format!("提交检出事务失败：{e}")))?;
        Ok(written)
    }

    /// 离线状态比对：mtime/size 没变就跳过哈希，变了才算内容。
    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        self.status_with(false)
    }

    /// `force = true` 时忽略快路径，对所有条目重算哈希。
    pub fn status_with(&self, force: bool) -> Result<Vec<StatusEntry>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, kind, base_rev, base_hash, mtime, size FROM entries ORDER BY path",
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;

        let mut out = Vec::new();
        let mut known = BTreeSet::new();
        // 冲突优先于一切状态：`Conflicted` 必须稳定地出现在 status 里，
        // 直到 `revert` 或用户显式解决；否则用户会以为「没有待办」而提交失败。
        let conflicted = self.conflicted_paths()?;

        for row in rows {
            let (path, _kind, base_rev, base_hash, mtime, size) =
                row.map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;
            known.insert(path.clone());
            let target = self.root.join(&path);

            if conflicted.contains(&path) {
                out.push(StatusEntry {
                    path,
                    state: WcState::Conflicted,
                    base_rev: base_rev as u64,
                });
                continue;
            }

            let meta = match fs::symlink_metadata(&target) {
                Ok(m) => m,
                Err(_) => {
                    out.push(StatusEntry {
                        path,
                        state: WcState::Deleted,
                        base_rev: base_rev as u64,
                    });
                    continue;
                }
            };

            // 快路径：纳秒级 mtime 与 size 都没变，才敢跳过哈希。
            if !force && mtime_nanos(&meta) == mtime && meta.len() as i64 == size {
                continue;
            }

            let current = fs::read(&target).unwrap_or_default();
            let current_hash = hash_bytes(&current);
            let unchanged = base_hash
                .as_deref()
                .map(|h| h == to_hex(&current_hash))
                .unwrap_or(false);

            if !unchanged {
                out.push(StatusEntry {
                    path,
                    state: WcState::Modified,
                    base_rev: base_rev as u64,
                });
            }
        }

        // 工作副本里存在、但不在版本库中的文件：未纳入版本控制。
        for path in self.walk_working_tree()? {
            if known.contains(&path) {
                continue;
            }
            // add/add 这类树冲突可能根本没有 `entries` 行（本地未受控文件挡路），
            // 但冲突本身必须露出来，所以在这里单独覆盖一次。
            out.push(StatusEntry {
                path: path.clone(),
                state: if conflicted.contains(&path) {
                    WcState::Conflicted
                } else {
                    WcState::Unversioned
                },
                base_rev: 0,
            });
        }

        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// 遍历工作副本中的文件（跳过 `.vcs/`），路径统一为 `/` 分隔。
    fn walk_working_tree(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        collect(&self.root, "", &mut out)?;
        out.sort();
        Ok(out)
    }

    /// 离线 diff：逐条比对 pristine 基线（或空）与磁盘当前内容。
    ///
    /// 返回 `(路径, 差异文本)`；没有差异的条目不会出现在结果里。
    pub fn diff(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, base_hash FROM entries ORDER BY path")
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;

        let mut out = Vec::new();
        let mut known = BTreeSet::new();
        for row in rows {
            let (path, base_hash) =
                row.map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;
            known.insert(path.clone());

            let old = match base_hash.as_deref().map(crate::hash::from_hex) {
                Some(Ok(h)) => self.read_pristine(&h).unwrap_or_default(),
                _ => Vec::new(),
            };
            let new = fs::read(self.root.join(&path)).unwrap_or_default();
            if old == new {
                continue;
            }
            out.push((path, unified_diff(&old, &new)));
        }

        for path in self.walk_working_tree()? {
            if known.contains(&path) {
                continue;
            }
            let new = fs::read(self.root.join(&path)).unwrap_or_default();
            out.push((path, unified_diff(&[], &new)));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

// ---------------- 更新、冲突与回退 ----------------

/// 服务端某次更新需要应用到本地的单条变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteChange {
    pub path: String,
    pub kind: ChangeKind,
    pub entry_kind: EntryKind,
    pub hash: Option<Hash>,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub rev: u64,
    pub updated: usize,
    pub added: usize,
    pub removed: usize,
    /// 内容冲突：本地与服务端都改了同一文件，保留本地内容，三哈希入库。
    pub conflicts: usize,
    /// 树冲突：delete/modify、add/add、未受控文件挡路等，不猜用户意图。
    pub tree_conflicts: usize,
}

/// 一条未解决的冲突。内容不在这里，只记三方哈希。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRecord {
    pub path: String,
    /// `text`（modify/modify）或 `tree`（delete/modify、add/add 等）。
    pub kind: String,
    pub base_hash: Option<Hash>,
    pub mine_hash: Option<Hash>,
    pub theirs_hash: Option<Hash>,
}

/// 用户对一条冲突的选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Mine,
    Theirs,
    Custom,
    Delete,
}

/// 本地待提交的变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalChange {
    pub path: String,
    pub kind: ChangeKind,
    pub entry_kind: EntryKind,
    pub hash: Option<Hash>,
    pub size: u64,
}

impl WorkingCopy {
    /// 把服务端的版本差异合并到工作副本。
    ///
    /// 冲突不写标记文件：base / mine / theirs 三个哈希进 `conflicts` 表，本地文件保持
    /// 用户当前内容不动。GUI 三栏与 CLI 都从这里取数据，不在工作目录里制造 `.mine` 之类的垃圾。
    pub fn apply_update(
        &mut self,
        repo_id: &str,
        target_rev: u64,
        changes: &[RemoteChange],
        store: &dyn ObjectStore,
    ) -> Result<UpdateOutcome> {
        let mut outcome = UpdateOutcome {
            rev: target_rev,
            ..Default::default()
        };
        let wc_root = self.root.clone();

        for change in changes {
            let path = crate::normalize_path(&change.path)?;
            let target = wc_root.join(&path);
            let existing = self.entry(&path)?;

            // 从最旧版本取差异时会带出「本地其实已经更新过」的条目：
            // 它们的 base_rev 已 >= 目标版本，再应用一次只会制造假冲突，直接跳过。
            if let Some(rev) = self.entry_base_rev(&path)? {
                if rev >= target_rev {
                    continue;
                }
            }
            // 内容已经等于目标内容（上次更新应用过、或 revert 回到同一版本）：
            // 视为已应用，顺手把 base_rev 推到目标版本，避免下次 update 反复重算这段差异。
            if let (Some(hash), Some((Some(base_hash), _))) = (&change.hash, &existing) {
                if to_hex(hash) == *base_hash {
                    self.set_entry_rev(&path, target_rev)?;
                    continue;
                }
            }

            match change.kind {
                ChangeKind::Delete => {
                    let Some((base_hash, _kind)) = existing else {
                        continue;
                    };
                    let local_hash = read_local_hash(&target);
                    let unchanged = local_hash
                        .as_ref()
                        .map(|h| Some(to_hex(h)) == base_hash)
                        .unwrap_or(true); // 本地已删：视为干净
                    if unchanged {
                        if target.exists() {
                            remove_path(&target)?;
                        }
                        self.delete_entry(&path)?;
                        outcome.removed += 1;
                    } else {
                        // delete/modify：服务端删了，本地改过 → 保留本地文件并标记冲突
                        self.record_conflict(
                            &path,
                            "tree",
                            base_hash.as_deref(),
                            local_hash.as_ref().map(to_hex).as_deref(),
                            None,
                            Some("{\"type\":\"delete/modify\"}"),
                        )?;
                        outcome.tree_conflicts += 1;
                    }
                }

                ChangeKind::Add => {
                    let hash = change.hash.ok_or_else(|| {
                        Error::new(ErrorCode::ObjectMissing, format!("{path} 缺少内容地址"))
                    })?;
                    // 本地已有同名条目或未受控文件挡路：不覆盖，记树冲突。
                    if existing.is_some() || target.exists() {
                        if let Ok(data) = store.get(&hash) {
                            write_pristine(&wc_root, &hash, &data)?;
                        }
                        self.record_conflict(
                            &path,
                            "tree",
                            None,
                            read_local_hash(&target).as_ref().map(to_hex).as_deref(),
                            Some(&to_hex(&hash)),
                            Some("{\"type\":\"add/add\"}"),
                        )?;
                        outcome.tree_conflicts += 1;
                        continue;
                    }
                    let data = store.get(&hash)?;
                    self.materialize(&path, change.entry_kind, &data, target_rev, &hash)?;
                    outcome.added += 1;
                }

                ChangeKind::Modify => {
                    let hash = change.hash.ok_or_else(|| {
                        Error::new(ErrorCode::ObjectMissing, format!("{path} 缺少内容地址"))
                    })?;
                    let base_hash = existing.as_ref().and_then(|(h, _)| h.clone());
                    let local_hash = read_local_hash(&target);

                    let local_clean = match (&local_hash, &base_hash) {
                        (Some(local), Some(base)) => to_hex(local) == *base,
                        // 文件不见了：本地删除 vs 服务端修改
                        (None, _) => false,
                        // 没有基线记录（不该出现）：保守当作有本地改动
                        (Some(_), None) => false,
                    };

                    if local_clean {
                        let data = store.get(&hash)?;
                        self.materialize(&path, change.entry_kind, &data, target_rev, &hash)?;
                        outcome.updated += 1;
                    } else {
                        // 冲突时也要把服务端内容存进 pristine：三栏合并必须在**离线**状态下
                        // 能读到 theirs，否则用户每次合并都要联网重新取一遍。
                        if let Ok(data) = store.get(&hash) {
                            write_pristine(&wc_root, &hash, &data)?;
                        }
                        self.record_conflict(
                            &path,
                            "text",
                            base_hash.as_deref(),
                            local_hash.as_ref().map(to_hex).as_deref(),
                            Some(&to_hex(&hash)),
                            Some("{\"type\":\"modify/modify\"}"),
                        )?;
                        outcome.conflicts += 1;
                    }
                }

                ChangeKind::Rename | ChangeKind::Copy => {
                    return Err(Error::new(
                        ErrorCode::PathConflict,
                        "rename / copy 变更需要显式用户确认，M2 不做自动应用",
                    ));
                }
            }
        }

        // 未变更的条目保留各自 base_rev（混合版本），整体 head 推进到目标版本。
        self.set_meta("head_rev", &target_rev.to_string())?;
        self.set_meta("last_contact_rev", &target_rev.to_string())?;
        self.set_meta("repo_id", repo_id)?;
        Ok(outcome)
    }

    /// 回退：从 pristine 恢复本地内容并清除冲突标记，完全离线。
    ///
    /// `paths` 为空表示全部条目。未纳入版本控制的文件不会被删除——那属于用户自己的东西。
    pub fn revert(&mut self, paths: &[String]) -> Result<usize> {
        let targets: Vec<String> = if paths.is_empty() {
            self.all_paths()?
        } else {
            paths
                .iter()
                .map(|p| crate::normalize_path(p))
                .collect::<Result<Vec<_>>>()?
        };

        let mut restored = 0usize;
        for path in targets {
            let Some((base_hash, kind)) = self.entry(&path)? else {
                continue;
            };
            let target = self.root.join(&path);
            match base_hash.as_deref().map(crate::from_hex) {
                Some(Ok(hash)) => {
                    let data = self.read_pristine(&hash)?;
                    write_entry(&target, parse_kind(&kind)?, &data)?;
                    let meta = fs::symlink_metadata(&target)
                        .map_err(|e| io("读取条目失败", &target, e))?;
                    self.conn
                        .execute(
                            "UPDATE entries SET state = 'normal', mtime = ?2, size = ?3,
                               checksum = ?4, conflict_id = NULL WHERE path = ?1",
                            params![path, mtime_nanos(&meta), data.len() as i64, to_hex(&hash)],
                        )
                        .map_err(|e| {
                            Error::new(ErrorCode::IoError, format!("更新条目失败：{e}"))
                        })?;
                }
                _ => {
                    // 没有基线内容（理论上不存在）：至少把状态恢复成 normal。
                    self.conn
                        .execute(
                            "UPDATE entries SET state = 'normal', conflict_id = NULL WHERE path = ?1",
                            params![path],
                        )
                        .map_err(|e| Error::new(ErrorCode::IoError, format!("更新条目失败：{e}")))?;
                }
            }
            self.conn
                .execute("DELETE FROM conflicts WHERE path = ?1", params![path])
                .map_err(|e| Error::new(ErrorCode::IoError, format!("清除冲突失败：{e}")))?;
            restored += 1;
        }
        Ok(restored)
    }

    /// 一条未解决的冲突（三方哈希）。
    ///
    /// 冲突不落标记文件，只在 `conflicts` 表里存 base / mine / theirs 三个内容地址；
    /// 内容本身在 pristine（base 与 theirs）与工作目录（mine）里。
    pub fn conflict_records(&self) -> Result<Vec<ConflictRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, kind, base_hash, mine_hash, theirs_hash FROM conflicts
                 WHERE resolved_at IS NULL ORDER BY path",
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询冲突失败：{e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询冲突失败：{e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let (path, kind, base, mine, theirs) =
                row.map_err(|e| Error::new(ErrorCode::IoError, format!("读取冲突失败：{e}")))?;
            out.push(ConflictRecord {
                path,
                kind,
                base_hash: base.as_deref().map(crate::from_hex).transpose()?,
                mine_hash: mine.as_deref().map(crate::from_hex).transpose()?,
                theirs_hash: theirs.as_deref().map(crate::from_hex).transpose()?,
            });
        }
        Ok(out)
    }

    pub fn conflict_record(&self, path: &str) -> Result<Option<ConflictRecord>> {
        Ok(self
            .conflict_records()?
            .into_iter()
            .find(|record| record.path == path))
    }

    /// 解决一条冲突。
    ///
    /// 四种选择对应四种真实意图，不做自动合并：
    ///   - `Mine`：保留磁盘现状（文件已被删掉则等价于接受删除）；
    ///   - `Theirs`：用服务端内容覆盖（`theirs_hash` 为空表示对方删除了该路径）；
    ///   - `Custom`：写入用户手工合并的结果；
    ///   - `Delete`：删除文件与条目，等于「这条路径我不要了」。
    ///
    /// 解决后条目状态回到 `normal`，但**不刷新 mtime/size**——这样下一次扫描仍会
    /// 发现内容与基线不同，从而正确地显示为 `MODIFY` 待提交。
    pub fn resolve_conflict(
        &mut self,
        path: &str,
        choice: ConflictChoice,
        content: Option<&[u8]>,
    ) -> Result<()> {
        let record = self.conflict_record(path)?.ok_or_else(|| {
            Error::new(ErrorCode::TreeConflict, format!("{path} 没有未解决的冲突"))
        })?;
        let target = self.root.join(path);

        match choice {
            ConflictChoice::Mine => {
                if !target.exists() {
                    self.delete_entry(path)?;
                }
            }
            ConflictChoice::Theirs => match record.theirs_hash {
                Some(hash) => {
                    let data = self.read_pristine(&hash)?;
                    self.write_resolved(path, &data)?;
                }
                None => {
                    remove_path(&target)?;
                    self.delete_entry(path)?;
                }
            },
            ConflictChoice::Custom => {
                let data = content
                    .ok_or_else(|| Error::new(ErrorCode::PathConflict, "自定义合并结果不能为空"))?;
                self.write_resolved(path, data)?;
            }
            ConflictChoice::Delete => {
                remove_path(&target)?;
                self.delete_entry(path)?;
            }
        }

        self.conn
            .execute("DELETE FROM conflicts WHERE path = ?1", params![path])
            .map_err(|e| Error::new(ErrorCode::IoError, format!("清除冲突失败：{e}")))?;
        self.conn
            .execute(
                "UPDATE entries SET conflict_id = NULL, state = 'normal' WHERE path = ?1",
                params![path],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("更新条目失败：{e}")))?;
        Ok(())
    }

    fn write_resolved(&self, path: &str, data: &[u8]) -> Result<()> {
        let target = self.root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io("创建目录失败", parent, e))?;
        }
        write_entry(&target, EntryKind::File, data)
    }

    /// 把「干净条目」的基线版本推进到 `rev`，返回受影响条数。
    ///
    /// 只有当调用方**确认这次 update 覆盖了整个仓库**时才应该调用：它断言「没出现在
    /// 变更集里的路径，在 rev 上与本地一致」。不推进的话，`oldest_rev()` 会被这些
    /// 从未被差异触及的条目一直钉在旧版本上，每次 update 都要重复计算同一段差异。
    ///
    /// 冲突中的条目（`state = 'conflicted'` 或有 `conflict_id`）一律跳过：
    /// 它们的内容是用户本地版本，与基线不符，冒然推进会谎报「已是最新」。
    pub fn advance_normal_bases(&mut self, rev: u64) -> Result<usize> {
        let affected = self
            .conn
            .execute(
                "UPDATE entries SET base_rev = ?1
                 WHERE state = 'normal' AND conflict_id IS NULL AND base_rev < ?1",
                params![rev as i64],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("推进基线版本失败：{e}")))?;
        Ok(affected)
    }

    /// 是否还有未解决的冲突。有冲突时不允许提交：宁可挡住，也不猜用户想留哪一边。
    pub fn has_conflicts(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM conflicts WHERE resolved_at IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询冲突失败：{e}")))?;
        Ok(count > 0)
    }

    pub fn conflicts(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, kind FROM conflicts WHERE resolved_at IS NULL ORDER BY path")
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询冲突失败：{e}")))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询冲突失败：{e}")))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取冲突失败：{e}")))
    }

    /// 未解决冲突的路径集合，供 `status` 判定 `Conflicted` 态。
    fn conflicted_paths(&self) -> Result<BTreeSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM conflicts WHERE resolved_at IS NULL")
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询冲突失败：{e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| Error::new(ErrorCode::IoError, format!("查询冲突失败：{e}")))?;
        let mut out = BTreeSet::new();
        for row in rows {
            out.insert(
                row.map_err(|e| Error::new(ErrorCode::IoError, format!("读取冲突失败：{e}")))?,
            );
        }
        Ok(out)
    }

    /// 采集本地待提交的变更（含内容哈希）。以全量校验为准，不依赖 mtime 快路径。
    ///
    /// 语义与 `import` 一致：**整个工作副本** 就是一次提交的内容，工作副本里新增的
    /// 文件（`Unversioned`）会被当作 `Add` 一起提交。这样「改完就提交」不需要额外的
    /// `add`/`rm` 步骤；代价是用户随手放进目录的临时文件也会进版本库，所以 CLI 在
    /// 提交前会把新增列表打印出来。
    pub fn local_changes(&self) -> Result<Vec<LocalChange>> {
        let mut out = Vec::new();
        for entry in self.status_with(true)? {
            let target = self.root.join(&entry.path);
            match entry.state {
                WcState::Modified => {
                    let data = fs::read(&target).unwrap_or_default();
                    out.push(LocalChange {
                        path: entry.path,
                        kind: ChangeKind::Modify,
                        entry_kind: EntryKind::File,
                        hash: Some(hash_bytes(&data)),
                        size: data.len() as u64,
                    });
                }
                WcState::Added | WcState::Unversioned => {
                    let data = fs::read(&target).unwrap_or_default();
                    out.push(LocalChange {
                        path: entry.path,
                        kind: ChangeKind::Add,
                        entry_kind: EntryKind::File,
                        hash: Some(hash_bytes(&data)),
                        size: data.len() as u64,
                    });
                }
                WcState::Deleted => out.push(LocalChange {
                    path: entry.path,
                    kind: ChangeKind::Delete,
                    entry_kind: EntryKind::File,
                    hash: None,
                    size: 0,
                }),
                // 冲突未解决时不产出变更，由调用方先拒绝提交。
                _ => {}
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// 提交成功后的本地收尾：刷新基线、清掉已删除条目、推进 head。
    pub fn mark_committed(
        &mut self,
        rev: u64,
        changes: &[LocalChange],
        store: &dyn ObjectStore,
    ) -> Result<()> {
        let wc_root = self.root.clone();
        for change in changes {
            match change.kind {
                ChangeKind::Add | ChangeKind::Modify => {
                    let Some(hash) = change.hash else { continue };
                    let target = wc_root.join(&change.path);
                    let meta = fs::symlink_metadata(&target)
                        .map_err(|e| io("读取条目失败", &target, e))?;
                    // 新内容进入 pristine，下一次离线 diff 才有正确基线。
                    if let Ok(data) = store.get(&hash) {
                        write_pristine(&wc_root, &hash, &data)?;
                    }
                    self.conn
                        .execute(
                            "INSERT INTO entries (path, kind, base_rev, base_hash, state, mtime, size, checksum)
                             VALUES (?1, 'file', ?2, ?3, 'normal', ?4, ?5, ?3)
                             ON CONFLICT(path) DO UPDATE SET
                               base_rev = excluded.base_rev, base_hash = excluded.base_hash,
                               state = 'normal', mtime = excluded.mtime, size = excluded.size,
                               checksum = excluded.checksum",
                            params![
                                change.path,
                                rev as i64,
                                to_hex(&hash),
                                mtime_nanos(&meta),
                                change.size as i64
                            ],
                        )
                        .map_err(|e| Error::new(ErrorCode::IoError, format!("更新条目失败：{e}")))?;
                    self.conn
                        .execute(
                            "INSERT INTO pristine (hash, size_bytes, stored_size, ref_count, path_hint)
                             VALUES (?1, ?2, ?2, 1, ?3)
                             ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
                            params![to_hex(&hash), change.size as i64, change.path],
                        )
                        .map_err(|e| Error::new(ErrorCode::IoError, format!("写 pristine 失败：{e}")))?;
                }
                ChangeKind::Delete => {
                    self.delete_entry(&change.path)?;
                }
                _ => {}
            }
        }
        self.set_meta("head_rev", &rev.to_string())?;
        self.set_meta("last_contact_rev", &rev.to_string())?;
        Ok(())
    }

    // ---------------- 内部辅助 ----------------

    fn entry(&self, path: &str) -> Result<Option<(Option<String>, String)>> {
        self.conn
            .query_row(
                "SELECT base_hash, kind FROM entries WHERE path = ?1",
                params![path],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))
    }

    /// 某条目当前的基线版本；条目不在版本控制下返回 None。
    fn entry_base_rev(&self, path: &str) -> Result<Option<u64>> {
        let rev: Option<i64> = self
            .conn
            .query_row(
                "SELECT base_rev FROM entries WHERE path = ?1",
                params![path],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目版本失败：{e}")))?;
        Ok(rev.map(|v| v.max(0) as u64))
    }

    /// 仅推进条目的基线版本号（内容已确认一致时使用）。
    fn set_entry_rev(&self, path: &str, rev: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE entries SET base_rev = ?2 WHERE path = ?1",
                params![path, rev as i64],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("更新条目版本失败：{e}")))?;
        Ok(())
    }

    fn all_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM entries ORDER BY path")
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::new(ErrorCode::IoError, format!("读取条目失败：{e}")))
    }

    fn delete_entry(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM entries WHERE path = ?1", params![path])
            .map_err(|e| Error::new(ErrorCode::IoError, format!("删除条目失败：{e}")))?;
        Ok(())
    }

    /// 把服务端内容落到磁盘并登记条目（新增 / 覆盖）。
    fn materialize(
        &self,
        path: &str,
        kind: EntryKind,
        data: &[u8],
        rev: u64,
        hash: &Hash,
    ) -> Result<()> {
        let target = self.root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io("创建目录失败", parent, e))?;
        }
        write_entry(&target, kind, data)?;
        write_pristine(&self.root, hash, data)?;
        let meta = fs::symlink_metadata(&target).map_err(|e| io("读取条目失败", &target, e))?;
        self.conn
            .execute(
                "INSERT INTO entries (path, kind, base_rev, base_hash, state, mtime, size, checksum)
                 VALUES (?1, ?2, ?3, ?4, 'normal', ?5, ?6, ?4)
                 ON CONFLICT(path) DO UPDATE SET
                   kind = excluded.kind, base_rev = excluded.base_rev,
                   base_hash = excluded.base_hash, state = 'normal',
                   mtime = excluded.mtime, size = excluded.size,
                   checksum = excluded.checksum, conflict_id = NULL",
                params![
                    path,
                    kind_str(kind),
                    rev as i64,
                    to_hex(hash),
                    mtime_nanos(&meta),
                    data.len() as i64
                ],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("更新条目失败：{e}")))?;
        self.conn
            .execute(
                "INSERT INTO pristine (hash, size_bytes, stored_size, ref_count, path_hint)
                 VALUES (?1, ?2, ?2, 1, ?3)
                 ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
                params![to_hex(hash), data.len() as i64, path],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("写 pristine 失败：{e}")))?;
        Ok(())
    }

    fn record_conflict(
        &self,
        path: &str,
        kind: &str,
        base: Option<&str>,
        mine: Option<&str>,
        theirs: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        let id = format!("cf-{}", crate::auth::random_token(8));
        self.conn
            .execute(
                "INSERT INTO conflicts (id, path, kind, base_hash, mine_hash, theirs_hash, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, path, kind, base, mine, theirs, detail, now_secs()],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("写入冲突失败：{e}")))?;
        self.conn
            .execute(
                "UPDATE entries SET state = 'conflicted', conflict_id = ?2 WHERE path = ?1",
                params![path, id],
            )
            .map_err(|e| Error::new(ErrorCode::IoError, format!("标记冲突失败：{e}")))?;
        Ok(())
    }
}

/// 读取磁盘上某条目的内容哈希；不存在返回 None。
fn read_local_hash(path: &Path) -> Option<Hash> {
    fs::read(path).ok().map(|data| hash_bytes(&data))
}

fn remove_path(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("删除文件失败", path, e)),
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

fn pristine_path(root: &Path, hash: &Hash) -> PathBuf {
    root.join(VCS_DIR)
        .join("pristine")
        .join(object_rel_path(hash))
}

fn write_pristine(root: &Path, hash: &Hash, data: &[u8]) -> Result<()> {
    let path = pristine_path(root, hash);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io("创建 pristine 目录失败", parent, e))?;
    }
    let tmp = path.with_extension("part");
    fs::write(&tmp, data).map_err(|e| io("写 pristine 失败", &tmp, e))?;
    fs::rename(&tmp, &path).map_err(|e| io("提交 pristine 失败", &path, e))?;
    Ok(())
}

fn kind_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
    }
}

fn write_entry(path: &Path, kind: EntryKind, data: &[u8]) -> Result<()> {
    match kind {
        EntryKind::Dir => {
            fs::create_dir_all(path).map_err(|e| io("创建目录失败", path, e))?;
            Ok(())
        }
        EntryKind::File => fs::write(path, data).map_err(|e| io("写入文件失败", path, e)),
        EntryKind::Symlink => {
            if path.exists() || fs::symlink_metadata(path).is_ok() {
                let _ = fs::remove_file(path);
            }
            #[cfg(unix)]
            {
                let target = String::from_utf8_lossy(data).to_string();
                std::os::unix::fs::symlink(&target, path)
                    .map_err(|e| io("创建符号链接失败", path, e))
            }
            #[cfg(not(unix))]
            {
                // Windows 无符号链接权限时退化为普通文件，保证检出不会整体失败。
                fs::write(path, data).map_err(|e| io("写入文件失败", path, e))
            }
        }
    }
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| io("遍历工作副本失败", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io("遍历工作副本失败", dir, e))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if prefix.is_empty() && name == VCS_DIR {
            continue;
        }
        if name.ends_with(".part") {
            continue;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let file_type = entry
            .file_type()
            .map_err(|e| io("读取条目类型失败", &entry.path(), e))?;
        if file_type.is_dir() {
            collect(&entry.path(), &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 纳秒级 mtime。
///
/// 秒级精度会让「同一秒内、字节数不变的改写」被快路径吞掉——这是真实踩到的坑：
/// 改一行金额但长度不变，status 报干净，用户以为提交成功了。纳秒精度直接消除这个窗口；
/// 对时间戳精度不足的文件系统（FAT 等），用户可以用 `status --full` 强制全量哈希。
fn mtime_nanos(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn io(action: &str, path: &Path, err: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::IoError,
        format!("{action}：{}（{err}）", path.display()),
    )
}

// ---------------- 行级 diff ----------------

/// 上下文行数，与 SVN/Git 的默认观感一致。
const CONTEXT: usize = 3;
/// LCS 的动态规划上限，超过就退化为整文件替换说明，避免大文件卡死 CLI。
const MAX_DIFF_CELLS: usize = 4_000_000;

/// 生成统一格式 diff。二进制内容只给出「大小变化」结论，不做逐行比较。
pub fn unified_diff(old: &[u8], new: &[u8]) -> String {
    if old.is_empty() && new.is_empty() {
        return String::new();
    }
    if is_binary(old) || is_binary(new) {
        return format!(
            "二进制文件不同（{} 字节 → {} 字节）\n",
            old.len(),
            new.len()
        );
    }
    let old_text = String::from_utf8_lossy(old).to_string();
    let new_text = String::from_utf8_lossy(new).to_string();
    let a = split_lines(&old_text);
    let b = split_lines(&new_text);

    if a.len().saturating_mul(b.len()) > MAX_DIFF_CELLS {
        return format!(
            "文件较大，跳过行级对比（{} 行 → {} 行）\n",
            a.len(),
            b.len()
        );
    }

    let ops = lcs_ops(&a, &b);
    let mut out = String::new();
    let mut i = 0usize;
    while i < ops.len() {
        // 找到下一个变化点，前后各带 CONTEXT 行上下文
        if matches!(ops[i], Op::Equal(_)) {
            i += 1;
            continue;
        }
        let start = i.saturating_sub(CONTEXT);
        let mut end = i;
        while end < ops.len() {
            if matches!(ops[end], Op::Equal(_)) {
                // 连续的相等行超过 2*CONTEXT 就该断开成新的 hunk
                let mut run = 0;
                while end + run < ops.len() && matches!(ops[end + run], Op::Equal(_)) {
                    run += 1;
                }
                if run > CONTEXT * 2 || end + run >= ops.len() {
                    break;
                }
                end += run;
            } else {
                end += 1;
            }
        }
        let end = (end + CONTEXT).min(ops.len());

        let hunk = &ops[start..end];
        // 起始行号 = hunk 之前被消费掉的对应侧行数 + 1。
        let old_start = ops[..start]
            .iter()
            .filter(|o| !matches!(o, Op::Insert(_)))
            .count()
            + 1;
        let new_start = ops[..start]
            .iter()
            .filter(|o| !matches!(o, Op::Delete(_)))
            .count()
            + 1;
        let old_len = hunk.iter().filter(|o| !matches!(o, Op::Insert(_))).count();
        let new_len = hunk.iter().filter(|o| !matches!(o, Op::Delete(_))).count();

        out.push_str(&format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@\n"
        ));
        for op in hunk {
            match op {
                Op::Equal(line) => out.push_str(&format!(" {line}")),
                Op::Delete(line) => out.push_str(&format!("-{line}")),
                Op::Insert(line) => out.push_str(&format!("+{line}")),
            }
        }
        i = end;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Equal(String),
    Delete(String),
    Insert(String),
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n').map(str::to_string).collect()
}

fn lcs_ops(a: &[String], b: &[String]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Equal(a[i].clone()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Delete(a[i].clone()));
            i += 1;
        } else {
            ops.push(Op::Insert(b[j].clone()));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Delete(a[i].clone()));
        i += 1;
    }
    while j < m {
        ops.push(Op::Insert(b[j].clone()));
        j += 1;
    }
    ops
}

fn is_binary(data: &[u8]) -> bool {
    // 前 8 KiB 出现 NUL 即按二进制处理，与 git 的启发式一致。
    data.iter().take(8192).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryObjectStore;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "vcs-wc-{tag}-{}-{}",
                std::process::id(),
                crate::auth::random_token(4)
            ));
            fs::create_dir_all(&base).unwrap();
            Self(base)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn seed_store(store: &MemoryObjectStore, content: &[u8]) -> Hash {
        let h = hash_bytes(content);
        store.put(&h, content).unwrap();
        h
    }

    #[test]
    fn checkout_writes_files_and_records_base() {
        let dir = TempDir::new("checkout");
        let store = MemoryObjectStore::new();
        let h = seed_store(&store, b"hello\n");
        let files = vec![FlatEntry {
            path: "202610/a.txt".into(),
            kind: EntryKind::File,
            hash: h,
            size: 6,
        }];

        let mut wc = WorkingCopy::create(dir.0.join("wc")).unwrap();
        wc.checkout("repo-1", 7, &files, &store).unwrap();

        assert_eq!(fs::read(dir.0.join("wc/202610/a.txt")).unwrap(), b"hello\n");
        assert_eq!(wc.base_rev().unwrap(), 7);
        assert_eq!(wc.get_meta("repo_id").unwrap().as_deref(), Some("repo-1"));
        assert!(wc.status().unwrap().is_empty());
    }

    #[test]
    fn status_detects_modification_after_mtime_change() {
        let dir = TempDir::new("status");
        let store = MemoryObjectStore::new();
        let h = seed_store(&store, b"v1\n");
        let files = vec![FlatEntry {
            path: "a.txt".into(),
            kind: EntryKind::File,
            hash: h,
            size: 3,
        }];

        let mut wc = WorkingCopy::create(dir.0.join("wc")).unwrap();
        wc.checkout("r", 1, &files, &store).unwrap();

        let target = dir.0.join("wc/a.txt");
        fs::write(&target, b"v2 longer\n").unwrap();
        // 明确改写 mtime，避免同一秒内写入被快路径误判为未修改
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(later)
            .unwrap();

        let status = wc.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].state, WcState::Modified);
        assert_eq!(status[0].path, "a.txt");
    }

    #[test]
    fn status_catches_same_size_edit_within_the_same_second() {
        // 回归用例：曾经用秒级 mtime 做快路径，「100」改成「200」这种等长改写
        // 会被判定为未修改，用户以为提交成功、实际什么都没提交。
        let dir = TempDir::new("racysame");
        let store = MemoryObjectStore::new();
        let h = seed_store(&store, b"100\n");
        let files = vec![FlatEntry {
            path: "amount.txt".into(),
            kind: EntryKind::File,
            hash: h,
            size: 4,
        }];

        let mut wc = WorkingCopy::create(dir.0.join("wc")).unwrap();
        wc.checkout("r", 1, &files, &store).unwrap();
        fs::write(dir.0.join("wc/amount.txt"), b"200\n").unwrap();

        let status = wc.status().unwrap();
        assert_eq!(status.len(), 1, "等长改写被快路径吞掉了：{status:?}");
        assert_eq!(status[0].state, WcState::Modified);
    }

    #[test]
    fn status_force_bypasses_fast_path() {
        let dir = TempDir::new("force");
        let store = MemoryObjectStore::new();
        let h = seed_store(&store, b"same\n");
        let files = vec![FlatEntry {
            path: "a.txt".into(),
            kind: EntryKind::File,
            hash: h,
            size: 5,
        }];
        let mut wc = WorkingCopy::create(dir.0.join("wc")).unwrap();
        wc.checkout("r", 1, &files, &store).unwrap();
        // 内容与 mtime 都不变时，强制模式也只做一次全量哈希，不产生误报。
        assert!(wc.status_with(true).unwrap().is_empty());
    }

    #[test]
    fn status_reports_deleted_and_unversioned() {
        let dir = TempDir::new("status2");
        let store = MemoryObjectStore::new();
        let h = seed_store(&store, b"x\n");
        let files = vec![FlatEntry {
            path: "gone.txt".into(),
            kind: EntryKind::File,
            hash: h,
            size: 2,
        }];

        let mut wc = WorkingCopy::create(dir.0.join("wc")).unwrap();
        wc.checkout("r", 1, &files, &store).unwrap();
        fs::remove_file(dir.0.join("wc/gone.txt")).unwrap();
        fs::write(dir.0.join("wc/new.txt"), b"n").unwrap();

        let status = wc.status().unwrap();
        let states: Vec<(String, WcState)> =
            status.into_iter().map(|s| (s.path, s.state)).collect();
        assert!(states.contains(&("gone.txt".to_string(), WcState::Deleted)));
        assert!(states.contains(&("new.txt".to_string(), WcState::Unversioned)));
    }

    #[test]
    fn diff_is_available_offline_from_pristine() {
        let dir = TempDir::new("diff");
        let store = MemoryObjectStore::new();
        let h = seed_store(&store, b"a\nb\nc\n");
        let files = vec![FlatEntry {
            path: "doc.txt".into(),
            kind: EntryKind::File,
            hash: h,
            size: 6,
        }];

        let mut wc = WorkingCopy::create(dir.0.join("wc")).unwrap();
        wc.checkout("r", 1, &files, &store).unwrap();
        fs::write(dir.0.join("wc/doc.txt"), b"a\nB\nc\n").unwrap();

        let diff = wc.diff().unwrap();
        assert_eq!(diff.len(), 1);
        assert!(diff[0].1.contains("-b\n"), "{}", diff[0].1);
        assert!(diff[0].1.contains("+B\n"), "{}", diff[0].1);
    }

    #[test]
    fn unified_diff_marks_binary() {
        let text = unified_diff(&[0u8, 1, 2], &[0u8, 1, 2, 3]);
        assert!(text.contains("二进制文件不同"));
    }

    #[test]
    fn unified_diff_is_empty_for_identical_content() {
        assert!(unified_diff(b"same\n", b"same\n").is_empty());
    }

    #[test]
    fn unified_diff_hunks_have_correct_ranges() {
        let old = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = b"1\n2\n3\n4\n5\n6\n7\n8\n9\nTEN\n";
        let text = unified_diff(old, new);
        assert!(text.contains("@@ -7,4 +7,4 @@"), "{text}");
        assert!(text.contains("-10\n"), "{text}");
        assert!(text.contains("+TEN\n"), "{text}");
    }

    // ---------------- update / revert / 提交收尾 ----------------

    /// 建一个含若干文件的工作副本，返回 (目录, 工作副本)。
    fn wc_with(files: &[(&str, &[u8])], rev: u64) -> (TempDir, WorkingCopy, MemoryObjectStore) {
        let dir = TempDir::new("upd");
        let store = MemoryObjectStore::new();
        let entries: Vec<FlatEntry> = files
            .iter()
            .map(|(path, content)| FlatEntry {
                path: (*path).to_string(),
                kind: EntryKind::File,
                hash: seed_store(&store, content),
                size: content.len() as u64,
            })
            .collect();
        let mut wc = WorkingCopy::create(dir.0.join("wc")).unwrap();
        wc.checkout("repo-1", rev, &entries, &store).unwrap();
        (dir, wc, store)
    }

    fn remote(path: &str, kind: ChangeKind, hash: Option<Hash>, size: u64) -> RemoteChange {
        RemoteChange {
            path: path.into(),
            kind,
            entry_kind: EntryKind::File,
            hash,
            size,
        }
    }

    #[test]
    fn apply_update_rewrites_downloads_and_removes_clean_entries() {
        let (dir, mut wc, store) = wc_with(&[("keep.txt", b"k\n"), ("old.txt", b"o\n")], 3);
        let added = seed_store(&store, b"n\n");
        let modified = seed_store(&store, b"o2\n");

        let outcome = wc
            .apply_update(
                "repo-1",
                4,
                &[
                    remote("new.txt", ChangeKind::Add, Some(added), 2),
                    remote("old.txt", ChangeKind::Modify, Some(modified), 3),
                    remote("keep.txt", ChangeKind::Delete, None, 0),
                ],
                &store,
            )
            .unwrap();

        assert_eq!(outcome.added, 1);
        assert_eq!(outcome.updated, 1);
        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.conflicts, 0);
        assert_eq!(outcome.tree_conflicts, 0);
        assert_eq!(fs::read(dir.0.join("wc/new.txt")).unwrap(), b"n\n");
        assert_eq!(fs::read(dir.0.join("wc/old.txt")).unwrap(), b"o2\n");
        assert!(!dir.0.join("wc/keep.txt").exists());
        // 整体推进到目标版本，且更新后立刻干净（pristine 已同步）。
        assert_eq!(wc.base_rev().unwrap(), 4);
        assert!(wc.status().unwrap().is_empty());
        assert!(!wc.has_conflicts().unwrap());
    }

    #[test]
    fn apply_update_keeps_local_edit_and_records_text_conflict() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"base\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"local\n").unwrap();
        let theirs = seed_store(&store, b"server\n");

        let outcome = wc
            .apply_update(
                "repo-1",
                2,
                &[remote("a.txt", ChangeKind::Modify, Some(theirs), 7)],
                &store,
            )
            .unwrap();

        assert_eq!(outcome.conflicts, 1);
        assert_eq!(outcome.updated, 0);
        // 本地内容不许被覆盖：宁可留着让用户看，也不猜。
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"local\n");
        assert_eq!(
            wc.conflicts().unwrap(),
            vec![("a.txt".to_string(), "text".to_string())]
        );
        // status 必须把它显示成冲突，否则用户以为没待办。
        let status = wc.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].state, WcState::Conflicted);
        assert!(wc.has_conflicts().unwrap());
    }

    #[test]
    fn apply_update_reports_tree_conflict_for_delete_modify() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"base\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"local edit\n").unwrap();

        let outcome = wc
            .apply_update(
                "repo-1",
                2,
                &[remote("a.txt", ChangeKind::Delete, None, 0)],
                &store,
            )
            .unwrap();

        assert_eq!(outcome.tree_conflicts, 1);
        assert_eq!(outcome.removed, 0);
        // 服务端删了、本地改过 → 文件留在原处等用户决定。
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"local edit\n");
        assert_eq!(
            wc.conflicts().unwrap(),
            vec![("a.txt".to_string(), "tree".to_string())]
        );
    }

    #[test]
    fn apply_update_reports_add_add_conflict_for_unversioned_file() {
        let (dir, mut wc, store) = wc_with(&[], 1);
        // 用户自己建了个同名文件，服务端恰好也要新增它。
        fs::write(dir.0.join("wc/mine.txt"), b"mine\n").unwrap();
        let theirs = seed_store(&store, b"theirs\n");

        let outcome = wc
            .apply_update(
                "repo-1",
                2,
                &[remote("mine.txt", ChangeKind::Add, Some(theirs), 7)],
                &store,
            )
            .unwrap();

        assert_eq!(outcome.tree_conflicts, 1);
        assert_eq!(outcome.added, 0);
        assert_eq!(fs::read(dir.0.join("wc/mine.txt")).unwrap(), b"mine\n");
        // 该路径没有 entries 行，冲突仍必须在 status 里出现。
        let status = wc.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].state, WcState::Conflicted);
    }

    #[test]
    fn apply_update_keeps_mixed_base_rev_for_untouched_entries() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"a\n")], 1);
        let newer = seed_store(&store, b"a2\n");
        wc.apply_update(
            "repo-1",
            2,
            &[remote("a.txt", ChangeKind::Modify, Some(newer), 3)],
            &store,
        )
        .unwrap();

        let rev: i64 = wc
            .conn
            .query_row(
                "SELECT base_rev FROM entries WHERE path = 'a.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rev, 2);
        // 混合版本：head 已到 2，但未被本次更新触及的条目保留旧 base_rev。
        let (dir2, mut wc2, store2) = wc_with(&[("a.txt", b"a\n"), ("b.txt", b"b\n")], 1);
        let only_a = seed_store(&store2, b"a2\n");
        wc2.apply_update(
            "repo-1",
            2,
            &[remote("a.txt", ChangeKind::Modify, Some(only_a), 3)],
            &store2,
        )
        .unwrap();
        let b_rev: i64 = wc2
            .conn
            .query_row(
                "SELECT base_rev FROM entries WHERE path = 'b.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b_rev, 1);
        assert_eq!(fs::read(dir2.0.join("wc/b.txt")).unwrap(), b"b\n");
        let _ = dir;
    }

    #[test]
    fn revert_restores_pristine_content_and_clears_conflict() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"base\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"local\n").unwrap();
        let theirs = seed_store(&store, b"server\n");
        wc.apply_update(
            "repo-1",
            2,
            &[remote("a.txt", ChangeKind::Modify, Some(theirs), 7)],
            &store,
        )
        .unwrap();
        assert!(wc.has_conflicts().unwrap());

        let restored = wc.revert(&["a.txt".to_string()]).unwrap();
        assert_eq!(restored, 1);
        // 回到 baseline，而不是服务端的最新内容：revert 的语义是「撤销我的改动」。
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"base\n");
        assert!(!wc.has_conflicts().unwrap());
        assert!(wc.status().unwrap().is_empty());
    }

    #[test]
    fn revert_all_skips_unversioned_files() {
        let (dir, mut wc, _store) = wc_with(&[("a.txt", b"base\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"changed\n").unwrap();
        fs::write(dir.0.join("wc/scratch.tmp"), b"keep me\n").unwrap();

        wc.revert(&[]).unwrap();

        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"base\n");
        // 未纳入版本控制的文件是用户自己的东西，revert 不该删。
        assert_eq!(
            fs::read(dir.0.join("wc/scratch.tmp")).unwrap(),
            b"keep me\n"
        );
    }

    #[test]
    fn local_changes_capture_add_modify_and_delete() {
        let (dir, wc, _store) = wc_with(
            &[
                ("mod.txt", b"v1\n"),
                ("del.txt", b"x\n"),
                ("same.txt", b"s\n"),
            ],
            1,
        );
        fs::write(dir.0.join("wc/mod.txt"), b"v2\n").unwrap();
        fs::remove_file(dir.0.join("wc/del.txt")).unwrap();
        fs::write(dir.0.join("wc/new.txt"), b"new\n").unwrap();

        let changes = wc.local_changes().unwrap();
        let kinds: Vec<(&str, ChangeKind)> =
            changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("del.txt", ChangeKind::Delete),
                ("mod.txt", ChangeKind::Modify),
                ("new.txt", ChangeKind::Add),
            ]
        );
        // 未改动的文件不产出变更，未受控文件按 Add 处理（整目录提交语义）。
        assert!(changes.iter().all(|c| c.path != "same.txt"));
    }

    #[test]
    fn mark_committed_refreshes_baseline_so_status_is_clean() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"v1\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"v2\n").unwrap();

        let changes = wc.local_changes().unwrap();
        assert_eq!(changes.len(), 1);
        let hash = changes[0].hash.unwrap();
        store.put(&hash, b"v2\n").unwrap();

        wc.mark_committed(4, &changes, &store).unwrap();

        assert_eq!(wc.base_rev().unwrap(), 4);
        assert!(wc.status().unwrap().is_empty());
        // 基线刷新后离线 diff 也该是空的；否则用户会反复看到同一份「待提交」差异。
        assert!(wc.diff().unwrap().is_empty());
    }

    #[test]
    fn mark_committed_drops_deleted_entries() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"v1\n")], 1);
        fs::remove_file(dir.0.join("wc/a.txt")).unwrap();

        let changes = wc.local_changes().unwrap();
        assert_eq!(changes[0].kind, ChangeKind::Delete);

        wc.mark_committed(2, &changes, &store).unwrap();

        assert!(wc.status().unwrap().is_empty());
        assert!(wc.diff().unwrap().is_empty());
    }

    #[test]
    fn advancing_bases_skips_conflicted_entries() {
        let (_dir, mut wc, store) = wc_with(&[("a.txt", b"a\n"), ("b.txt", b"b\n")], 1);
        let theirs = seed_store(&store, b"a2\n");
        // a.txt 本地也改过 → 冲突；b.txt 干净。
        fs::write(_dir.0.join("wc/a.txt"), b"local\n").unwrap();
        wc.apply_update(
            "repo-1",
            3,
            &[remote("a.txt", ChangeKind::Modify, Some(theirs), 3)],
            &store,
        )
        .unwrap();
        assert!(wc.has_conflicts().unwrap());

        let affected = wc.advance_normal_bases(3).unwrap();
        assert_eq!(affected, 1, "只应推进 b.txt");
        // 冲突条目必须停在旧基线：推进它等于谎报「已是最新」。
        let record = wc.conflict_record("a.txt").unwrap().unwrap();
        assert_eq!(record.base_hash, Some(seed_store(&store, b"a\n")));
        assert_eq!(wc.oldest_rev().unwrap(), 1);
    }

    #[test]
    fn conflict_keeps_theirs_content_available_offline() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"base\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"mine\n").unwrap();
        let theirs = seed_store(&store, b"theirs\n");
        wc.apply_update(
            "repo-1",
            2,
            &[remote("a.txt", ChangeKind::Modify, Some(theirs), 7)],
            &store,
        )
        .unwrap();

        // 三栏合并要求在离线状态读到服务端内容：冲突时必须顺手缓存进 pristine。
        let cached = wc.read_pristine(&theirs).unwrap();
        assert_eq!(cached, b"theirs\n");
        let record = wc.conflict_record("a.txt").unwrap().unwrap();
        assert_eq!(record.kind, "text");
        assert_eq!(record.theirs_hash, Some(theirs));
        assert!(record.mine_hash.is_some());
        assert_eq!(wc.conflict_records().unwrap().len(), 1);
    }

    #[test]
    fn resolving_with_theirs_and_custom_updates_the_file() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"base\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"mine\n").unwrap();
        let theirs = seed_store(&store, b"theirs\n");
        wc.apply_update(
            "repo-1",
            2,
            &[remote("a.txt", ChangeKind::Modify, Some(theirs), 7)],
            &store,
        )
        .unwrap();

        wc.resolve_conflict("a.txt", ConflictChoice::Theirs, None)
            .unwrap();
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"theirs\n");
        assert!(!wc.has_conflicts().unwrap());
        // 解决后仍要显示为「待提交」：内容与基线不同。
        let status = wc.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].state, WcState::Modified);

        // 再来一次，用 Custom 写入手工合并结果。
        fs::write(dir.0.join("wc/a.txt"), b"mine again\n").unwrap();
        let theirs2 = seed_store(&store, b"theirs2\n");
        wc.apply_update(
            "repo-1",
            3,
            &[remote("a.txt", ChangeKind::Modify, Some(theirs2), 8)],
            &store,
        )
        .unwrap();
        wc.resolve_conflict("a.txt", ConflictChoice::Custom, Some(b"merged\n"))
            .unwrap();
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"merged\n");
        assert!(!wc.has_conflicts().unwrap());
    }

    #[test]
    fn resolving_with_mine_keeps_local_bytes_and_delete_drops_entry() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"base\n")], 1);
        fs::write(dir.0.join("wc/a.txt"), b"mine\n").unwrap();
        let theirs = seed_store(&store, b"theirs\n");
        wc.apply_update(
            "repo-1",
            2,
            &[remote("a.txt", ChangeKind::Modify, Some(theirs), 7)],
            &store,
        )
        .unwrap();

        wc.resolve_conflict("a.txt", ConflictChoice::Mine, None)
            .unwrap();
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"mine\n");
        assert!(!wc.has_conflicts().unwrap());

        // Delete 分支：文件与条目一起消失，status 也不再报这条路径。
        fs::write(dir.0.join("wc/a.txt"), b"changed\n").unwrap();
        let theirs2 = seed_store(&store, b"theirs2\n");
        wc.apply_update(
            "repo-1",
            3,
            &[remote("a.txt", ChangeKind::Modify, Some(theirs2), 8)],
            &store,
        )
        .unwrap();
        wc.resolve_conflict("a.txt", ConflictChoice::Delete, None)
            .unwrap();
        assert!(!dir.0.join("wc/a.txt").exists());
        assert!(wc.status().unwrap().is_empty());
        // 再解决一次应当拒绝：冲突已经不在了。
        let err = wc
            .resolve_conflict("a.txt", ConflictChoice::Mine, None)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::TreeConflict);
    }

    #[test]
    fn oldest_rev_tracks_the_mixed_version_floor() {
        let (_dir, mut wc, store) = wc_with(&[("a.txt", b"a\n"), ("b.txt", b"b\n")], 3);
        let newer = seed_store(&store, b"a2\n");
        wc.apply_update(
            "repo-1",
            5,
            &[remote("a.txt", ChangeKind::Modify, Some(newer), 3)],
            &store,
        )
        .unwrap();

        // head 到了 5，但 b.txt 仍停在 3：update 必须从 3 取差异，否则 b.txt 永远拉不新。
        assert_eq!(wc.base_rev().unwrap(), 5);
        assert_eq!(wc.oldest_rev().unwrap(), 3);
    }

    #[test]
    fn reapplying_the_same_changes_is_a_noop() {
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"a\n")], 1);
        let newer = seed_store(&store, b"a2\n");
        let change = [remote("a.txt", ChangeKind::Modify, Some(newer), 3)];

        wc.apply_update("repo-1", 2, &change, &store).unwrap();
        // 客户端从最旧版本取差异时会重复带来已应用的条目，重复应用不能制造假冲突。
        let again = wc.apply_update("repo-1", 2, &change, &store).unwrap();

        assert_eq!(again.updated, 0);
        assert_eq!(again.conflicts, 0);
        assert_eq!(again.tree_conflicts, 0);
        assert!(!wc.has_conflicts().unwrap());
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"a2\n");
    }

    #[test]
    fn content_equal_to_target_advances_base_without_conflict() {
        // 场景：Add 的条目本地已存在且内容一致（上次更新已铺好），
        // 从更旧版本重算差异时不能再报 add/add。
        let (dir, mut wc, store) = wc_with(&[("a.txt", b"a\n")], 1);
        let same = seed_store(&store, b"a\n");
        // 手工把条目版本退回去，模拟「基线落后但内容已是目标内容」。
        wc.set_entry_rev("a.txt", 0).unwrap();

        let outcome = wc
            .apply_update(
                "repo-1",
                1,
                &[remote("a.txt", ChangeKind::Add, Some(same), 2)],
                &store,
            )
            .unwrap();

        assert_eq!(outcome.tree_conflicts, 0);
        assert_eq!(outcome.added, 0);
        assert!(!wc.has_conflicts().unwrap());
        assert_eq!(fs::read(dir.0.join("wc/a.txt")).unwrap(), b"a\n");
        assert_eq!(wc.oldest_rev().unwrap(), 1);
    }
}
