//! svn-next 的 Node 原生模块（napi-rs）。
//!
//! **存在的唯一理由**：让 Electron 复用 `vcs-core` 的版本语义，而不是在 JS 里重写一遍。
//! 哈希、树编码、`wc.db`、pristine 基线、离线 diff、冲突元数据全都在 `vcs-core` 里，
//! 这个 crate 只做三件事：类型转换、JSON 序列化、错误码透传——不放任何业务判断。
//!
//! 约定：
//!   - 结构化数据一律用 JSON 字符串进出（`serde_json`），二进制内容用 `Buffer`；
//!   - 对象内容按「`hashes[i]` ↔ `blobs[i]`」成对传入，缺一个就报错，不做静默补齐；
//!   - `vcs_core::Error` 统一转成 `"OutOfDate: 本地版本 r1 已落后…"` 这种可读文本，
//!     JS 侧只负责展示，真正的判定永远发生在 Rust。
#![deny(clippy::all)]

use std::path::Path;

use napi::bindgen_prelude::Buffer;
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use vcs_core::{
    from_hex, hash_bytes, to_hex, ChangeKind, ConflictChoice, EntryKind, FlatEntry, Hash,
    MemoryObjectStore, ObjectStore, RemoteChange, WcState, WorkingCopy,
};

fn to_napi(err: vcs_core::Error) -> napi::Error {
    napi::Error::from_reason(format!("{:?}: {}", err.code, err.message))
}

fn bad(msg: impl Into<String>) -> napi::Error {
    napi::Error::from_reason(msg.into())
}

/// 打开工作副本：目录里已有 `.vcs/wc.db` 就沿用，否则新建。
fn open(dir: &str) -> napi::Result<WorkingCopy> {
    let path = Path::new(dir);
    let existed = path.join(".vcs").join("wc.db").exists();
    if existed {
        WorkingCopy::open(path).map_err(to_napi)
    } else {
        WorkingCopy::create(path).map_err(to_napi)
    }
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

/// 用「哈希 ↔ 内容」对搭一个临时对象存储。
///
/// `WorkingCopy` 的检出 / 更新 / 提交收尾都要 `&dyn ObjectStore`；桌面端没有本地 CAS，
/// 内容刚从网络或磁盘拿在手上，用内存存储最直接，也复用了同一套经过测试的写入逻辑。
fn memory_store(hashes: Vec<String>, blobs: Vec<Buffer>) -> napi::Result<MemoryObjectStore> {
    if hashes.len() != blobs.len() {
        return Err(bad(format!(
            "hashes 与 blobs 数量不一致：{} vs {}",
            hashes.len(),
            blobs.len()
        )));
    }
    let store = MemoryObjectStore::new();
    for (hex, data) in hashes.iter().zip(blobs.iter()) {
        let hash: Hash = from_hex(hex).map_err(to_napi)?;
        store.put(&hash, data.as_ref()).map_err(to_napi)?;
    }
    Ok(store)
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

// ---------------- 哈希 ----------------

/// BLAKE3-256 十六进制摘要。与服务端 `vcs_core::hash_bytes` 完全同一实现。
#[napi]
pub fn hash_bytes_hex(data: Buffer) -> String {
    to_hex(&hash_bytes(data.as_ref()))
}

// ---------------- 工作副本 ----------------

/// 工作副本归属：`{"repoId": "...", "rev": 3, "oldestRev": 1}`。
#[napi]
pub fn wc_info(dir: String) -> napi::Result<String> {
    let wc = open(&dir)?;
    let (repo_id, rev, oldest) = working_copy_info(&wc)?;
    Ok(format!(
        "{{\"repoId\":\"{repo_id}\",\"rev\":{rev},\"oldestRev\":{oldest}}}"
    ))
}

/// 检出：把服务端给的条目与内容铺到磁盘，并写 pristine 基线。
#[napi]
pub fn wc_checkout(
    dir: String,
    repo_id: String,
    rev: u32,
    entries_json: String,
    hashes: Vec<String>,
    blobs: Vec<Buffer>,
) -> napi::Result<u32> {
    let entries: Vec<EntryIn> =
        serde_json::from_str(&entries_json).map_err(|e| bad(format!("解析条目失败：{e}")))?;
    let store = memory_store(hashes, blobs)?;
    let mut wc = open(&dir)?;
    let files: Vec<FlatEntry> = entries
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
        .collect::<napi::Result<Vec<_>>>()?;
    let written = wc
        .checkout(&repo_id, rev as u64, &files, &store)
        .map_err(to_napi)?;
    Ok(written as u32)
}

/// 扫描工作副本，返回 `[{path, kind}]`。走 core 的 mtime 快路径。
#[napi]
pub fn wc_status(dir: String) -> napi::Result<String> {
    let wc = open(&dir)?;
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

/// 采集待提交的变更（含内容地址）。以全量校验为准，不依赖 mtime 快路径。
#[napi]
pub fn wc_local_changes(dir: String) -> napi::Result<String> {
    let wc = open(&dir)?;
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

/// 离线 diff（基线 vs 磁盘）。完全本地，不访问服务端。
#[napi]
pub fn wc_diff(dir: String) -> napi::Result<String> {
    let wc = open(&dir)?;
    let diffs = wc.diff().map_err(to_napi)?;
    let out: Vec<DiffJson> = diffs
        .into_iter()
        .map(|(path, text)| DiffJson { path, text })
        .collect();
    serde_json::to_string(&out).map_err(|e| bad(format!("序列化差异失败：{e}")))
}

/// 提交成功后的本地收尾：刷新基线、删除已删条目、推进 head。
///
/// 内容从工作目录直接读取——提交刚成功，磁盘上的字节就是刚提交的那份。
#[napi]
pub fn wc_mark_committed(dir: String, rev: u32, changes_json: String) -> napi::Result<()> {
    let incoming: Vec<ChangeIn> =
        serde_json::from_str(&changes_json).map_err(|e| bad(format!("解析变更失败：{e}")))?;
    let store = MemoryObjectStore::new();
    let mut changes = Vec::with_capacity(incoming.len());
    for change in &incoming {
        let kind = parse_change_kind(&change.kind)?;
        let hash = match &change.hash {
            Some(hex) => Some(from_hex(hex).map_err(to_napi)?),
            None => None,
        };
        if let Some(hash) = hash {
            // 读不到就跳过基线缓存：缺 pristine 只影响离线 diff 的显示，
            // 不影响已提交结果的正确性（服务端早已有这份内容）。
            if let Ok(data) = std::fs::read(Path::new(&dir).join(&change.path)) {
                store.put(&hash, &data).map_err(to_napi)?;
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
    let mut wc = open(&dir)?;
    wc.mark_committed(rev as u64, &changes, &store)
        .map_err(to_napi)?;
    Ok(())
}

/// 应用服务端差异（update）：干净条目覆盖、脏条目记冲突，本地内容一律不动。
#[napi]
pub fn wc_apply_update(
    dir: String,
    repo_id: String,
    target_rev: u32,
    changes_json: String,
    hashes: Vec<String>,
    blobs: Vec<Buffer>,
) -> napi::Result<String> {
    let incoming: Vec<ChangeIn> =
        serde_json::from_str(&changes_json).map_err(|e| bad(format!("解析变更失败：{e}")))?;
    let store = memory_store(hashes, blobs)?;
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
    let mut wc = open(&dir)?;
    let outcome = wc
        .apply_update(&repo_id, target_rev as u64, &remote, &store)
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

/// 全量 update 成功后推进干净条目的基线版本（调用方需确认变更集覆盖了整个仓库）。
#[napi]
pub fn wc_advance_base(dir: String, rev: u32) -> napi::Result<u32> {
    let mut wc = open(&dir)?;
    let affected = wc.advance_normal_bases(rev as u64).map_err(to_napi)?;
    Ok(affected as u32)
}

/// 撤销本地改动：从基线恢复内容并清除冲突标记。
#[napi]
pub fn wc_revert(dir: String, paths_json: String) -> napi::Result<u32> {
    let paths: Vec<String> =
        serde_json::from_str(&paths_json).map_err(|e| bad(format!("解析路径失败：{e}")))?;
    let mut wc = open(&dir)?;
    let restored = wc.revert(&paths).map_err(to_napi)?;
    Ok(restored as u32)
}

/// 未解决的冲突列表 `[{path, kind}]`。
#[napi]
pub fn wc_conflicts(dir: String) -> napi::Result<String> {
    let wc = open(&dir)?;
    let conflicts = wc.conflicts().map_err(to_napi)?;
    let out: Vec<ConflictJson> = conflicts
        .into_iter()
        .map(|(path, kind)| ConflictJson { path, kind })
        .collect();
    serde_json::to_string(&out).map_err(|e| bad(format!("序列化冲突失败：{e}")))
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

fn pristine_or_none(wc: &WorkingCopy, hash: Option<Hash>) -> Option<Buffer> {
    // 读不到就返回 None：三栏里显示「（无此版本）」比整块报错有用。
    hash.and_then(|hash| wc.read_pristine(&hash).ok())
        .map(Buffer::from)
}

#[napi]
pub fn wc_conflict_detail(dir: String, path: String) -> napi::Result<ConflictDetail> {
    let wc = open(&dir)?;
    let record = wc
        .conflict_record(&path)
        .map_err(to_napi)?
        .ok_or_else(|| bad(format!("{path} 没有未解决的冲突")))?;
    let mine = std::fs::read(Path::new(&dir).join(&path))
        .ok()
        .map(Buffer::from);
    Ok(ConflictDetail {
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

/// 解决一条冲突。`choice` ∈ `mine` / `theirs` / `custom` / `delete`。
#[napi]
pub fn wc_resolve_conflict(
    dir: String,
    path: String,
    choice: String,
    content: Option<Buffer>,
) -> napi::Result<()> {
    let choice = match choice.as_str() {
        "mine" => ConflictChoice::Mine,
        "theirs" => ConflictChoice::Theirs,
        "custom" => ConflictChoice::Custom,
        "delete" => ConflictChoice::Delete,
        other => return Err(bad(format!("未知的冲突处理方式：{other}"))),
    };
    let mut wc = open(&dir)?;
    wc.resolve_conflict(&path, choice, content.as_ref().map(|b| b.as_ref()))
        .map_err(to_napi)
}

#[napi]
pub fn wc_has_conflicts(dir: String) -> napi::Result<bool> {
    let wc = open(&dir)?;
    wc.has_conflicts().map_err(to_napi)
}
