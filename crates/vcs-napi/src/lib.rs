//! svn-next 的 Node 原生模块（napi-rs）。
//!
//! **存在的唯一理由**：让 Electron 复用 `vcs-core` 的版本语义，而不是在 JS 里重写一遍。
//! 哈希、树编码、`wc.db`、pristine 基线、离线 diff、冲突元数据全都在 `vcs-core` 里，
//! 这个 crate 只做三件事：类型转换、JSON 序列化、错误码透传——不放任何业务判断。
//!
//! 约定：
//!   - 结构化数据一律用 JSON 字符串进出（`serde_json`）；
//!   - **对象内容一律走磁盘上的 spool 目录**（CAS 布局：`objects/ab/cdef…`，首字节是
//!     存储标记），由 native 侧 `FileObjectStore` 流式读取。这样几 GB 的检出 / 更新也
//!     不会把整库或单个大文件读进内存——把 Buffer 数组传进来是上一版的做法，已废弃；
//!   - **动盘的调用一律异步**：`AsyncTask` 把工作扔进 libuv 线程池，主线程只负责参数打包
//!     与结果反序列化。这些接口过去是同步的，于是「提交 / 更新一个几 GB 的文件」等于让
//!     Electron 主进程被几十秒的哈希与写盘钉住，窗口拖不动、按钮点不动，用户看到的就是
//!     「点一下卡一下」。异步之后主线程始终空闲，界面照常响应；
//!   - 代价是 JS 侧全部拿到 Promise：同一份工作副本上的调用必须由调用方串行
//!     （`desktop/src/main/wc.ts` 里按目录排队），否则两个连接会同时写同一个 `wc.db`；
//!   - `vcs_core::Error` 统一转成 `"OutOfDate: 本地版本 r1 已落后…"` 这种可读文本，
//!     JS 侧只负责展示，真正的判定永远发生在 Rust。
#![deny(clippy::all)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use napi::bindgen_prelude::{AsyncTask, Buffer};
use napi::{Env, Task};
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use vcs_core::{
    from_hex, hash_bytes, hash_file, to_hex, ChangeKind, ConflictChoice, EntryKind,
    FileObjectStore, FlatEntry, Hash, ObjectReader, ObjectStore, RemoteChange, WcState,
    WorkingCopy,
};

fn to_napi(err: vcs_core::Error) -> napi::Error {
    napi::Error::from_reason(format!("{:?}: {}", err.code, err.message))
}

fn bad(msg: impl Into<String>) -> napi::Error {
    napi::Error::from_reason(msg.into())
}

/// 打开工作副本：目录里已有 `.vcs/wc.db` 就沿用，否则新建。
///
/// **只有检出（`wc_checkout`）可以用这个**：检出本来就要在空目录里铺一份。
fn open_or_create(dir: &str) -> napi::Result<WorkingCopy> {
    let path = Path::new(dir);
    let existed = path.join(".vcs").join("wc.db").exists();
    if existed {
        WorkingCopy::open(path).map_err(to_napi)
    } else {
        WorkingCopy::create(path).map_err(to_napi)
    }
}

/// 只打开**已经存在**的工作副本；`.vcs/wc.db` 不在就报错，磁盘上什么都不会创建。
///
/// 除了检出，其余所有操作都必须走这里。曾经的 `open` 会「没有就新建」，于是用户把
/// 检出目录整个删掉之后，界面只要点一次「扫描」，被删的目录连同 `.vcs/wc.db` 就会被
/// 原样建回来——一份空的工作副本凭空出现，比报错更难解释。
fn open_existing(dir: &str) -> napi::Result<WorkingCopy> {
    WorkingCopy::open(Path::new(dir)).map_err(to_napi)
}

fn state_str(state: WcState) -> &'static str {
    match state {
        // 未纳入版本控制的文件在桌面端就是「待新增」：与 CLI 的全目录提交语义一致。
        WcState::Added | WcState::Unversioned => "ADD",
        WcState::Modified => "MODIFY",
        WcState::Deleted => "DELETE",
        WcState::Conflicted => "CONFLICT",
        WcState::Normal => "NORMAL",
    }
}

fn change_kind_str(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Add => "ADD",
        ChangeKind::Modify => "MODIFY",
        ChangeKind::Delete => "DELETE",
        ChangeKind::Rename => "RENAME",
        ChangeKind::Copy => "COPY",
    }
}

fn parse_change_kind(kind: &str) -> napi::Result<ChangeKind> {
    match kind {
        "ADD" => Ok(ChangeKind::Add),
        "MODIFY" => Ok(ChangeKind::Modify),
        "DELETE" => Ok(ChangeKind::Delete),
        other => Err(bad(format!("未知的变更类型：{other}"))),
    }
}

fn parse_entry_kind(kind: &str) -> napi::Result<EntryKind> {
    match kind {
        "file" => Ok(EntryKind::File),
        "dir" => Ok(EntryKind::Dir),
        "symlink" => Ok(EntryKind::Symlink),
        other => Err(bad(format!("未知的条目类型：{other}"))),
    }
}

/// 打开下载暂存目录（CAS 布局）作为对象源。
///
/// 桌面端把网络上的分片直接落成 `<spool>/objects/ab/cdef…`，这里用 `FileObjectStore`
/// 打开它：`open()` 是流式的，检出 / 更新按块读，内存与对象大小无关。
fn spool_store(spool_dir: &str) -> napi::Result<FileObjectStore> {
    FileObjectStore::open(spool_dir).map_err(to_napi)
}

/// 以工作目录里的文件为内容源（哈希 → 路径由调用方给出）。
///
/// 只用于 `mark_committed` 写 pristine：提交刚成功，磁盘上的字节就是刚提交的那份，
/// 没必要再从网络或内存里搬一遍。同样走流式读。
struct WorkingDirectoryStore {
    paths: HashMap<String, PathBuf>,
}

impl ObjectStore for WorkingDirectoryStore {
    fn get(&self, hash: &Hash) -> vcs_core::Result<Vec<u8>> {
        let path = self.paths.get(&to_hex(hash)).ok_or_else(|| {
            vcs_core::Error::new(vcs_core::ErrorCode::ObjectMissing, "本地没有这份内容")
        })?;
        std::fs::read(path)
            .map_err(|e| vcs_core::Error::new(vcs_core::ErrorCode::IoError, format!("读取 {e}")))
    }

    fn open(&self, hash: &Hash) -> vcs_core::Result<ObjectReader> {
        let path = self.paths.get(&to_hex(hash)).ok_or_else(|| {
            vcs_core::Error::new(vcs_core::ErrorCode::ObjectMissing, "本地没有这份内容")
        })?;
        let file = std::fs::File::open(path)
            .map_err(|e| vcs_core::Error::new(vcs_core::ErrorCode::IoError, format!("读取 {e}")))?;
        Ok(Box::new(file))
    }

    fn put(&self, _hash: &Hash, _data: &[u8]) -> vcs_core::Result<()> {
        Err(vcs_core::Error::new(
            vcs_core::ErrorCode::InvalidSession,
            "工作目录存储是只读的",
        ))
    }

    fn exists(&self, hash: &Hash) -> bool {
        self.paths.contains_key(&to_hex(hash))
    }

    fn remove(&self, _hash: &Hash) -> vcs_core::Result<()> {
        Err(vcs_core::Error::new(
            vcs_core::ErrorCode::InvalidSession,
            "工作目录存储是只读的",
        ))
    }

    fn stored_size(&self, hash: &Hash) -> vcs_core::Result<u64> {
        let path = self.paths.get(&to_hex(hash)).ok_or_else(|| {
            vcs_core::Error::new(vcs_core::ErrorCode::ObjectMissing, "本地没有这份内容")
        })?;
        std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| vcs_core::Error::new(vcs_core::ErrorCode::IoError, format!("读取 {e}")))
    }
}

#[derive(Serialize)]
struct StatusJson {
    path: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct ChangeJson {
    path: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    size: u64,
}

#[derive(Deserialize)]
struct ChangeIn {
    path: String,
    kind: String,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Serialize)]
struct DiffJson {
    path: String,
    text: String,
}

#[derive(Deserialize)]
struct EntryIn {
    path: String,
    hash: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WcInfoJson {
    repo_id: String,
    rev: u64,
    oldest_rev: u64,
    /// 空数组 = 全量检出。
    sparse_rules: Vec<String>,
}

#[derive(Serialize)]
struct SparseJson {
    added: usize,
    removed: usize,
    #[serde(rename = "prunedDirs")]
    pruned_dirs: usize,
    /// 因里面还有未纳入版本控制的文件、被原样保留的目录（相对工作副本根）。
    #[serde(rename = "keptDirs")]
    kept_dirs: Vec<String>,
}

#[derive(Serialize)]
struct UpdateJson {
    rev: u64,
    updated: usize,
    added: usize,
    removed: usize,
    conflicts: usize,
    tree_conflicts: usize,
}

#[derive(Serialize)]
struct ConflictJson {
    path: String,
    kind: String,
}

fn working_copy_info(wc: &WorkingCopy) -> napi::Result<(String, u64, u64)> {
    let repo_id = wc
        .get_meta("repo_id")
        .map_err(to_napi)?
        .ok_or_else(|| bad("工作副本没有关联仓库，请先检出"))?;
    let rev = wc.base_rev().map_err(to_napi)?;
    // 混合版本：head 已经推进，但某些条目（例如刚 revert 过的）仍停在旧基线。
    // update 必须从最旧的那个版本取差异，只看 head 会漏掉这些条目。
    let oldest = wc.oldest_rev().map_err(to_napi)?;
    Ok((repo_id, rev, oldest))
}

/** JS 传来的条目统一在这里转成 core 的 FlatEntry；检出与规则调整共用。 */
fn parse_entries(entries: &[EntryIn]) -> napi::Result<Vec<FlatEntry>> {
    entries
        .iter()
        .map(|entry| {
            Ok(FlatEntry {
                path: entry.path.clone(),
                kind: match &entry.kind {
                    Some(kind) => parse_entry_kind(kind)?,
                    None => EntryKind::File,
                },
                hash: from_hex(&entry.hash).map_err(to_napi)?,
                size: entry.size,
            })
        })
        .collect()
}

fn parse_sparse_rules(text: &str) -> napi::Result<vcs_core::SparseRules> {
    let raw: Vec<String> =
        serde_json::from_str(text).map_err(|e| bad(format!("解析稀疏规则失败：{e}")))?;
    vcs_core::SparseRules::from_paths(raw).map_err(to_napi)
}

// ---------------- 异步任务 ----------------
//
// 一个操作 = 一个「参数包」（下面的 `*Op`）+ 一个 `Task` 实现。
// 参数包只装纯数据（String / u32 / Vec<u8>），因此可以安全地搬进 libuv 线程池；
// `compute` 里跑完整的 Rust 逻辑，`resolve` 只把结果交回 JS 线程。
//
// 为什么不统一成「一种任务返回 JSON」：返回值类型本来就不同（字符串 / 计数 / 无返回值 /
// 带 Buffer 的冲突三元组），硬凑成 JSON 会把 Buffer 编成 base64，白折腾一层编解码。

/// 返回 JSON 字符串的操作：状态、差异、变更、冲突列表、检出信息、更新结果、哈希。
enum JsonOp {
    HashFile {
        path: String,
    },
    Info {
        dir: String,
    },
    Status {
        dir: String,
    },
    Diff {
        dir: String,
    },
    LocalChanges {
        dir: String,
    },
    Conflicts {
        dir: String,
    },
    ApplyUpdate {
        dir: String,
        repo_id: String,
        target_rev: u32,
        changes: String,
        spool_dir: String,
    },
    ApplySparseRules {
        dir: String,
        rules: String,
        rev: u32,
        entries: String,
        spool_dir: String,
    },
}

impl JsonOp {
    fn run(&self) -> napi::Result<String> {
        match self {
            JsonOp::HashFile { path } => {
                let (hash, size) =
                    hash_file(Path::new(path)).map_err(|e| bad(format!("读取文件失败：{e}")))?;
                Ok(format!(
                    "{{\"hash\":\"{}\",\"size\":{}}}",
                    to_hex(&hash),
                    size
                ))
            }
            JsonOp::Info { dir } => {
                let wc = open_existing(dir)?;
                let (repo_id, rev, oldest) = working_copy_info(&wc)?;
                let rules = wc.sparse_rules().map_err(to_napi)?;
                serde_json::to_string(&WcInfoJson {
                    repo_id,
                    rev,
                    oldest_rev: oldest,
                    sparse_rules: rules.paths,
                })
                .map_err(|e| bad(format!("序列化工作副本信息失败：{e}")))
            }
            JsonOp::Status { dir } => {
                let wc = open_existing(dir)?;
                let entries = wc.status().map_err(to_napi)?;
                let out: Vec<StatusJson> = entries
                    .into_iter()
                    .filter(|entry| entry.state != WcState::Normal)
                    .map(|entry| StatusJson {
                        path: entry.path,
                        kind: state_str(entry.state),
                    })
                    .collect();
                serde_json::to_string(&out).map_err(|e| bad(format!("序列化状态失败：{e}")))
            }
            JsonOp::Diff { dir } => {
                let wc = open_existing(dir)?;
                let diffs = wc.diff().map_err(to_napi)?;
                let out: Vec<DiffJson> = diffs
                    .into_iter()
                    .map(|(path, text)| DiffJson { path, text })
                    .collect();
                serde_json::to_string(&out).map_err(|e| bad(format!("序列化差异失败：{e}")))
            }
            JsonOp::LocalChanges { dir } => {
                let wc = open_existing(dir)?;
                let changes = wc.local_changes().map_err(to_napi)?;
                let out: Vec<ChangeJson> = changes
                    .into_iter()
                    .map(|change| ChangeJson {
                        path: change.path,
                        kind: change_kind_str(change.kind),
                        hash: change.hash.map(|hash| to_hex(&hash)),
                        size: change.size,
                    })
                    .collect();
                serde_json::to_string(&out).map_err(|e| bad(format!("序列化变更失败：{e}")))
            }
            JsonOp::Conflicts { dir } => {
                let wc = open_existing(dir)?;
                let conflicts = wc.conflicts().map_err(to_napi)?;
                let out: Vec<ConflictJson> = conflicts
                    .into_iter()
                    .map(|(path, kind)| ConflictJson { path, kind })
                    .collect();
                serde_json::to_string(&out).map_err(|e| bad(format!("序列化冲突失败：{e}")))
            }
            JsonOp::ApplyUpdate {
                dir,
                repo_id,
                target_rev,
                changes,
                spool_dir,
            } => {
                let incoming: Vec<ChangeIn> =
                    serde_json::from_str(changes).map_err(|e| bad(format!("解析变更失败：{e}")))?;
                let store = spool_store(spool_dir)?;
                let mut remote = Vec::with_capacity(incoming.len());
                for change in &incoming {
                    remote.push(RemoteChange {
                        path: change.path.clone(),
                        kind: parse_change_kind(&change.kind)?,
                        entry_kind: EntryKind::File,
                        hash: match &change.hash {
                            Some(hex) => Some(from_hex(hex).map_err(to_napi)?),
                            None => None,
                        },
                        size: change.size,
                    });
                }
                let mut wc = open_existing(dir)?;
                let outcome = wc
                    .apply_update(repo_id, *target_rev as u64, &remote, &store)
                    .map_err(to_napi)?;
                let json = UpdateJson {
                    rev: outcome.rev,
                    updated: outcome.updated,
                    added: outcome.added,
                    removed: outcome.removed,
                    conflicts: outcome.conflicts,
                    tree_conflicts: outcome.tree_conflicts,
                };
                serde_json::to_string(&json).map_err(|e| bad(format!("序列化更新结果失败：{e}")))
            }
            JsonOp::ApplySparseRules {
                dir,
                rules,
                rev,
                entries,
                spool_dir,
            } => {
                let rules = parse_sparse_rules(rules)?;
                let entries: Vec<EntryIn> =
                    serde_json::from_str(entries).map_err(|e| bad(format!("解析条目失败：{e}")))?;
                let store = spool_store(spool_dir)?;
                let files = parse_entries(&entries)?;
                // 调整规则必须作用在已存在的工作副本上，绝不在空目录里凭空建一份。
                let mut wc = open_existing(dir)?;
                let outcome = wc
                    .apply_sparse_rules(&rules, &files, *rev as u64, &store)
                    .map_err(to_napi)?;
                serde_json::to_string(&SparseJson {
                    added: outcome.added,
                    removed: outcome.removed,
                    pruned_dirs: outcome.pruned_dirs,
                    kept_dirs: outcome.kept_dirs,
                })
                .map_err(|e| bad(format!("序列化稀疏结果失败：{e}")))
            }
        }
    }
}

/// 返回计数（`u32`）的操作：检出写了几个文件、推进了几条基线、还原了几条。
enum NumOp {
    Checkout {
        dir: String,
        repo_id: String,
        rev: u32,
        entries: String,
        spool_dir: String,
        rules: String,
    },
    AdvanceBase {
        dir: String,
        rev: u32,
    },
    Revert {
        dir: String,
        paths: String,
    },
}

impl NumOp {
    fn run(&self) -> napi::Result<u32> {
        match self {
            NumOp::Checkout {
                dir,
                repo_id,
                rev,
                entries,
                spool_dir,
                rules,
            } => {
                let entries: Vec<EntryIn> =
                    serde_json::from_str(entries).map_err(|e| bad(format!("解析条目失败：{e}")))?;
                let store = spool_store(spool_dir)?;
                let files = parse_entries(&entries)?;
                let rules = parse_sparse_rules(rules)?;
                // 检出是唯一允许「目录不存在就新建」的入口。
                let mut wc = open_or_create(dir)?;
                let written = wc
                    .checkout_with_rules(repo_id, *rev as u64, &files, &store, &rules)
                    .map_err(to_napi)?;
                Ok(written as u32)
            }
            NumOp::AdvanceBase { dir, rev } => {
                let mut wc = open_existing(dir)?;
                let affected = wc.advance_normal_bases(*rev as u64).map_err(to_napi)?;
                Ok(affected as u32)
            }
            NumOp::Revert { dir, paths } => {
                let paths: Vec<String> =
                    serde_json::from_str(paths).map_err(|e| bad(format!("解析路径失败：{e}")))?;
                let mut wc = open_existing(dir)?;
                let restored = wc.revert(&paths).map_err(to_napi)?;
                Ok(restored as u32)
            }
        }
    }
}

/// 没有返回值的操作：提交后的本地收尾、解决一条冲突。
enum UnitOp {
    MarkCommitted {
        dir: String,
        rev: u32,
        changes: String,
    },
    ResolveConflict {
        dir: String,
        path: String,
        choice: String,
        /// 手工合并结果；`mine` / `theirs` / `delete` 时为空。
        content: Option<Vec<u8>>,
    },
}

impl UnitOp {
    fn run(&self) -> napi::Result<()> {
        match self {
            UnitOp::MarkCommitted { dir, rev, changes } => {
                let incoming: Vec<ChangeIn> =
                    serde_json::from_str(changes).map_err(|e| bad(format!("解析变更失败：{e}")))?;
                let mut paths: HashMap<String, PathBuf> = HashMap::new();
                let mut changes = Vec::with_capacity(incoming.len());
                for change in &incoming {
                    let kind = parse_change_kind(&change.kind)?;
                    let hash = match &change.hash {
                        Some(hex) => Some(from_hex(hex).map_err(to_napi)?),
                        None => None,
                    };
                    if let Some(hash) = hash {
                        // 内容从工作目录流式读，不整份进内存；读不到就跳过基线缓存——缺 pristine
                        // 只影响离线 diff 的显示，不影响已提交结果的正确性（服务端早已有这份内容）。
                        let path = Path::new(dir).join(&change.path);
                        if path.is_file() {
                            paths.insert(to_hex(&hash), path);
                        }
                    }
                    changes.push(vcs_core::LocalChange {
                        path: change.path.clone(),
                        kind,
                        entry_kind: EntryKind::File,
                        hash,
                        size: change.size,
                    });
                }
                let store = WorkingDirectoryStore { paths };
                let mut wc = open_existing(dir)?;
                wc.mark_committed(*rev as u64, &changes, &store)
                    .map_err(to_napi)?;
                Ok(())
            }
            UnitOp::ResolveConflict {
                dir,
                path,
                choice,
                content,
            } => {
                let choice = match choice.as_str() {
                    "mine" => ConflictChoice::Mine,
                    "theirs" => ConflictChoice::Theirs,
                    "custom" => ConflictChoice::Custom,
                    "delete" => ConflictChoice::Delete,
                    other => return Err(bad(format!("未知的冲突处理方式：{other}"))),
                };
                let mut wc = open_existing(dir)?;
                wc.resolve_conflict(path, choice, content.as_deref())
                    .map_err(to_napi)
            }
        }
    }
}

/// 三方内容的「线程间形态」：Buffer 只在 JS 线程存在，后台线程用 `Vec<u8>`。
///
/// 必须是 `pub` 的：它出现在 `DetailTask` 的关联类型里，而 `Task` 的关联类型是公开接口。
pub struct RawDetail {
    path: String,
    kind: String,
    base_hash: Option<String>,
    mine_hash: Option<String>,
    theirs_hash: Option<String>,
    base: Option<Vec<u8>>,
    mine: Option<Vec<u8>>,
    theirs: Option<Vec<u8>>,
}

/// 三方合并窗口只处理「人能看懂」的内容：超过这个体积就不往 JS 里搬了。
///
/// 冲突编辑器本来就没法编辑一个几 GB 的文件，读进来只会把主进程撑爆；
/// 返回 None 时界面显示「（内容过大，请用外部工具合并）」。
const CONFLICT_CONTENT_MAX_BYTES: u64 = 8 * 1024 * 1024;

fn pristine_or_none(wc: &WorkingCopy, hash: Option<Hash>) -> Option<Vec<u8>> {
    // 读不到就返回 None：三栏里显示「（无此版本）」比整块报错有用。
    let hash = hash?;
    if wc.pristine_size(&hash).unwrap_or(0) > CONFLICT_CONTENT_MAX_BYTES {
        return None;
    }
    wc.read_pristine(&hash).ok()
}

struct DetailOp {
    dir: String,
    path: String,
}

impl DetailOp {
    fn run(&self) -> napi::Result<RawDetail> {
        let wc = open_existing(&self.dir)?;
        let record = wc
            .conflict_record(&self.path)
            .map_err(to_napi)?
            .ok_or_else(|| bad(format!("{} 没有未解决的冲突", self.path)))?;
        let mine_path = Path::new(&self.dir).join(&record.path);
        let mine = if std::fs::metadata(&mine_path).map(|m| m.len()).unwrap_or(0)
            > CONFLICT_CONTENT_MAX_BYTES
        {
            None
        } else {
            std::fs::read(&mine_path).ok()
        };
        Ok(RawDetail {
            path: record.path,
            kind: record.kind,
            base_hash: record.base_hash.map(|hash| to_hex(&hash)),
            mine_hash: record.mine_hash.map(|hash| to_hex(&hash)),
            theirs_hash: record.theirs_hash.map(|hash| to_hex(&hash)),
            base: pristine_or_none(&wc, record.base_hash),
            mine,
            theirs: pristine_or_none(&wc, record.theirs_hash),
        })
    }
}

// ---------------- Task 包装 ----------------

pub struct JsonTask(JsonOp);
pub struct NumTask(NumOp);
pub struct UnitTask(UnitOp);
pub struct DetailTask(DetailOp);

impl Task for JsonTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> napi::Result<String> {
        self.0.run()
    }

    fn resolve(&mut self, _env: Env, output: String) -> napi::Result<String> {
        Ok(output)
    }
}

impl Task for NumTask {
    type Output = u32;
    type JsValue = u32;

    fn compute(&mut self) -> napi::Result<u32> {
        self.0.run()
    }

    fn resolve(&mut self, _env: Env, output: u32) -> napi::Result<u32> {
        Ok(output)
    }
}

impl Task for UnitTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> napi::Result<()> {
        self.0.run()
    }

    fn resolve(&mut self, _env: Env, _output: ()) -> napi::Result<()> {
        Ok(())
    }
}

impl Task for DetailTask {
    type Output = RawDetail;
    type JsValue = ConflictDetail;

    fn compute(&mut self) -> napi::Result<RawDetail> {
        self.0.run()
    }

    fn resolve(&mut self, _env: Env, output: RawDetail) -> napi::Result<ConflictDetail> {
        Ok(ConflictDetail {
            path: output.path,
            kind: output.kind,
            base_hash: output.base_hash,
            mine_hash: output.mine_hash,
            theirs_hash: output.theirs_hash,
            base: output.base.map(Buffer::from),
            mine: output.mine.map(Buffer::from),
            theirs: output.theirs.map(Buffer::from),
        })
    }
}

// ---------------- 哈希 ----------------

/// BLAKE3-256 十六进制摘要。与服务端 `vcs_core::hash_bytes` 完全同一实现。
///
/// 唯一保持同步的接口：入参是一块已经在内存里的内容，算完就走，没有磁盘等待。
#[napi]
pub fn hash_bytes_hex(data: Buffer) -> String {
    to_hex(&hash_bytes(data.as_ref()))
}

/// 流式算文件摘要，返回 `{"hash":"…","size":N}`（Promise）。
///
/// 提交前校验「扫描之后用户没再改文件」用它：读一遍就够，不把文件读进内存。
/// 几 GB 的文件要在后台线程里读几分钟——主进程不能陪着等。
#[napi]
pub fn hash_file_hex(path: String) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::HashFile { path }))
}

// ---------------- 工作副本 ----------------

/// 工作副本归属与稀疏规则（Promise）：
/// `{"repoId":"...","rev":3,"oldestRev":1,"sparseRules":[]}`（空数组 = 全量检出）。
/// 目录不是一个工作副本时直接报错——绝不顺手建一份。
#[napi]
pub fn wc_info(dir: String) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::Info { dir }))
}

/// 检出：把服务端给的条目铺到磁盘，并写 pristine 基线；返回铺盘的文件数（Promise）。
///
/// 内容从 `spool_dir` 里流式读（CAS 布局，桌面端边下载边写），所以整库多大都不进内存。
/// `rulesJson` 是稀疏路径数组的 JSON，传 `[]` 表示全量检出。
#[napi]
pub fn wc_checkout(
    dir: String,
    repo_id: String,
    rev: u32,
    entries_json: String,
    spool_dir: String,
    rules_json: String,
) -> AsyncTask<NumTask> {
    AsyncTask::new(NumTask(NumOp::Checkout {
        dir,
        repo_id,
        rev,
        entries: entries_json,
        spool_dir,
        rules: rules_json,
    }))
}

/// 调整稀疏规则：补拉新路径、移除不再需要的路径（Promise）。
///
/// `entriesJson` 与 `spoolDir` 是**新增前缀**下的文件与内容（内容可为空）；
/// 有本地改动挡路时 core 会直接报错，一个文件都不会删。
#[napi]
pub fn wc_apply_sparse_rules(
    dir: String,
    rules_json: String,
    rev: u32,
    entries_json: String,
    spool_dir: String,
) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::ApplySparseRules {
        dir,
        rules: rules_json,
        rev,
        entries: entries_json,
        spool_dir,
    }))
}

/// 扫描工作副本，返回 `[{path, kind}]`（Promise）。走 core 的 mtime 快路径。
#[napi]
pub fn wc_status(dir: String) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::Status { dir }))
}

/// 采集待提交的变更（含内容地址，Promise）。以全量校验为准，不依赖 mtime 快路径。
#[napi]
pub fn wc_local_changes(dir: String) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::LocalChanges { dir }))
}

/// 离线 diff（基线 vs 磁盘，Promise）。完全本地，不访问服务端。
#[napi]
pub fn wc_diff(dir: String) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::Diff { dir }))
}

/// 提交成功后的本地收尾：刷新基线、删除已删条目、推进 head（Promise）。
///
/// 内容从工作目录直接读取——提交刚成功，磁盘上的字节就是刚提交的那份。
#[napi]
pub fn wc_mark_committed(dir: String, rev: u32, changes_json: String) -> AsyncTask<UnitTask> {
    AsyncTask::new(UnitTask(UnitOp::MarkCommitted {
        dir,
        rev,
        changes: changes_json,
    }))
}

/// 应用服务端差异（update，Promise）：干净条目覆盖、脏条目记冲突，本地内容一律不动。
#[napi]
pub fn wc_apply_update(
    dir: String,
    repo_id: String,
    target_rev: u32,
    changes_json: String,
    spool_dir: String,
) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::ApplyUpdate {
        dir,
        repo_id,
        target_rev,
        changes: changes_json,
        spool_dir,
    }))
}

/// 全量 update 成功后推进干净条目的基线版本（Promise，调用方需确认变更集覆盖了整个仓库）。
#[napi]
pub fn wc_advance_base(dir: String, rev: u32) -> AsyncTask<NumTask> {
    AsyncTask::new(NumTask(NumOp::AdvanceBase { dir, rev }))
}

/// 撤销本地改动（Promise）：从基线恢复内容并清除冲突标记。
#[napi]
pub fn wc_revert(dir: String, paths_json: String) -> AsyncTask<NumTask> {
    AsyncTask::new(NumTask(NumOp::Revert {
        dir,
        paths: paths_json,
    }))
}

/// 未解决的冲突列表 `[{path, kind}]`（Promise）。
#[napi]
pub fn wc_conflicts(dir: String) -> AsyncTask<JsonTask> {
    AsyncTask::new(JsonTask(JsonOp::Conflicts { dir }))
}

/// 三方内容：base 来自 pristine，mine 来自工作目录，theirs 也来自 pristine
/// （冲突发生时已缓存，所以合并全程离线）。
#[napi(object)]
pub struct ConflictDetail {
    pub path: String,
    pub kind: String,
    pub base_hash: Option<String>,
    pub mine_hash: Option<String>,
    pub theirs_hash: Option<String>,
    pub base: Option<Buffer>,
    pub mine: Option<Buffer>,
    pub theirs: Option<Buffer>,
}

/// 读一条冲突的三方内容（Promise，单份上限 8 MiB）。
#[napi]
pub fn wc_conflict_detail(dir: String, path: String) -> AsyncTask<DetailTask> {
    AsyncTask::new(DetailTask(DetailOp { dir, path }))
}

/// 解决一条冲突（Promise）。`choice` ∈ `mine` / `theirs` / `custom` / `delete`。
#[napi]
pub fn wc_resolve_conflict(
    dir: String,
    path: String,
    choice: String,
    content: Option<Buffer>,
) -> AsyncTask<UnitTask> {
    AsyncTask::new(UnitTask(UnitOp::ResolveConflict {
        dir,
        path,
        choice,
        // Buffer 只在 JS 线程活着，先拷成 Vec<u8> 再交给后台线程。
        content: content.map(|buffer| buffer.to_vec()),
    }))
}

/// 有没有未解决的冲突。
///
/// 保持同步：一次 `SELECT EXISTS` 级的查询，等一次线程池往返反而更贵；
/// 而且调用方总在「提交」这条串行流程里，不存在与写事务并发的问题。
#[napi]
pub fn wc_has_conflicts(dir: String) -> napi::Result<bool> {
    let wc = open_existing(&dir)?;
    wc.has_conflicts().map_err(to_napi)
}
