//! 不可变完整 tree 快照的规范编码与组装。
//!
//! 每次提交都写一棵完整的目录树（Merkle 树），而不是给上一版打增量补丁。
//! 这样做的直接收益是：任意版本的目录结构都可以 O(1) 定位、树对象天然按内容去重、
//! 路径级裁剪（purged_paths）只需要「切断引用」而不用重写历史链。
//!
//! 编码必须是**规范**的：同样的目录内容在任何平台、任何写入顺序下都要产生完全相同的字节，
//! 否则 blake3 地址会漂移，去重与「同一内容只存一份」的承诺就失效了。
//! 因此这里不使用 serde/bincode 之类可能因版本变化而改变布局的格式，
//! 而是手写定长小端编码，并强制按名字字节序排序。

use std::collections::BTreeMap;

use crate::hash::hash_bytes;
use crate::{EntryKind, Error, ErrorCode, Hash, ObjectStore, TreeEntry, HASH_LEN};

/// 编码版本魔数。布局一旦变化必须升版本，旧对象仍然可读。
const TREE_MAGIC: &[u8; 8] = b"VCSTREE1";

fn kind_to_u8(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::File => 1,
        EntryKind::Dir => 2,
        EntryKind::Symlink => 3,
    }
}

fn u8_to_kind(v: u8) -> crate::Result<EntryKind> {
    match v {
        1 => Ok(EntryKind::File),
        2 => Ok(EntryKind::Dir),
        3 => Ok(EntryKind::Symlink),
        other => Err(Error::new(
            ErrorCode::TreeConflict,
            format!("未知的 entry 类型字节：{other}"),
        )),
    }
}

/// 规范编码：排序后的定长小端布局。
pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

    let mut out = Vec::with_capacity(12 + sorted.len() * 49);
    out.extend_from_slice(TREE_MAGIC);
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for entry in sorted {
        out.push(kind_to_u8(entry.kind));
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.hash);
        let name = entry.name.as_bytes();
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name);
    }
    out
}

/// 解析树对象。任何越界、版本不符都当作仓库损坏处理，不静默截断。
pub fn decode_tree(bytes: &[u8]) -> crate::Result<Vec<TreeEntry>> {
    let corrupt = |msg: &str| Error::new(ErrorCode::TreeConflict, msg.to_string());

    if bytes.len() < 12 || &bytes[..8] != TREE_MAGIC {
        return Err(corrupt("树对象魔数不匹配，可能不是 tree 对象或版本过旧"));
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().expect("4 字节")) as usize;
    let mut cursor = 12usize;
    let mut entries = Vec::with_capacity(count);

    for _ in 0..count {
        // kind(1) + size(8) + hash(32) + name_len(4)
        if cursor + 45 > bytes.len() {
            return Err(corrupt("树对象被截断"));
        }
        let kind = u8_to_kind(bytes[cursor])?;
        cursor += 1;
        let size = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().expect("8 字节"));
        cursor += 8;
        let mut hash: Hash = [0u8; HASH_LEN];
        hash.copy_from_slice(&bytes[cursor..cursor + HASH_LEN]);
        cursor += HASH_LEN;
        let name_len =
            u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().expect("4 字节")) as usize;
        cursor += 4;

        if cursor + name_len > bytes.len() {
            return Err(corrupt("树对象名字段被截断"));
        }
        let name = std::str::from_utf8(&bytes[cursor..cursor + name_len])
            .map_err(|_| corrupt("树对象中的文件名不是合法 UTF-8"))?
            .to_string();
        cursor += name_len;

        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(corrupt("树对象中存在非法文件名"));
        }
        entries.push(TreeEntry {
            name,
            kind,
            hash,
            size,
        });
    }

    if cursor != bytes.len() {
        return Err(corrupt("树对象尾部存在多余字节"));
    }
    Ok(entries)
}

/// 把树对象写入 CAS 并返回其地址。内容相同的树只会真正落盘一次。
pub fn write_tree(store: &dyn ObjectStore, entries: &[TreeEntry]) -> crate::Result<Hash> {
    let bytes = encode_tree(entries);
    let hash = hash_bytes(&bytes);
    store.put(&hash, &bytes)?;
    Ok(hash)
}

/// 读取一层目录内容。
pub fn read_tree(store: &dyn ObjectStore, hash: &Hash) -> crate::Result<Vec<TreeEntry>> {
    let bytes = store.get(hash)?;
    decode_tree(&bytes)
}

// ---------------- 自底向上组装 ----------------

#[derive(Debug)]
enum Node {
    File {
        hash: Hash,
        size: u64,
        kind: EntryKind,
    },
    Dir(BTreeMap<String, Node>),
}

/// 扁平文件清单 → 完整目录树。
///
/// 用于 `import`：调用方只需给出「相对路径 + 内容地址 + 大小」，
/// 目录层级与中间树对象由这里逐层生成。空目录不会被表示（MVP 明确不保留空目录）。
pub fn build_tree(
    store: &dyn ObjectStore,
    files: &[(String, Hash, u64, EntryKind)],
) -> crate::Result<Hash> {
    let mut root: BTreeMap<String, Node> = BTreeMap::new();

    for (path, hash, size, kind) in files {
        let normalized = crate::normalize_path(path)?;
        if normalized.is_empty() {
            return Err(Error::new(ErrorCode::PathConflict, "空路径不能加入版本库"));
        }
        let mut parts: Vec<&str> = normalized.split('/').collect();
        let file_name = parts.pop().expect("规范化后至少有一段").to_string();

        let mut cursor = &mut root;
        for part in parts {
            let entry = cursor
                .entry(part.to_string())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            match entry {
                Node::Dir(children) => cursor = children,
                Node::File { .. } => {
                    return Err(Error::new(
                        ErrorCode::PathConflict,
                        format!("路径冲突：{part} 既是文件又是目录"),
                    ));
                }
            }
        }

        match cursor.get(&file_name) {
            Some(Node::Dir(_)) => {
                return Err(Error::new(
                    ErrorCode::PathConflict,
                    format!("路径冲突：{normalized} 既是目录又是文件"),
                ));
            }
            Some(Node::File { .. }) => {
                return Err(Error::new(
                    ErrorCode::PathConflict,
                    format!("同一路径被重复加入：{normalized}"),
                ));
            }
            None => {
                cursor.insert(
                    file_name,
                    Node::File {
                        hash: *hash,
                        size: *size,
                        kind: *kind,
                    },
                );
            }
        }
    }

    write_level(store, &root)
}

fn write_level(store: &dyn ObjectStore, level: &BTreeMap<String, Node>) -> crate::Result<Hash> {
    let mut entries = Vec::with_capacity(level.len());
    for (name, node) in level {
        match node {
            Node::File { hash, size, kind } => entries.push(TreeEntry {
                name: name.clone(),
                kind: *kind,
                hash: *hash,
                size: *size,
            }),
            Node::Dir(children) => {
                let child_hash = write_level(store, children)?;
                entries.push(TreeEntry {
                    name: name.clone(),
                    kind: EntryKind::Dir,
                    hash: child_hash,
                    // 目录本身不占内容空间；子树字节数由 GC 单独统计。
                    size: 0,
                });
            }
        }
    }
    write_tree(store, &entries)
}

// ---------------- 增量变更（copy-on-write） ----------------

/// 对一棵树的单点变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeOp {
    /// 新增或覆盖一个文件 / 符号链接。
    Upsert {
        path: String,
        kind: EntryKind,
        hash: Hash,
        size: u64,
    },
    /// 删除一个路径；若是目录则整棵子树一并摘除。
    Delete { path: String },
}

#[derive(Debug, Clone)]
enum LevelOp {
    Upsert {
        kind: EntryKind,
        hash: Hash,
        size: u64,
    },
    Delete,
}

/// 在基线上应用变更，返回新的根树地址。
///
/// **copy-on-write**：只有变更路径经过的目录会被重新编码写入，其余子树直接复用原有哈希。
/// 一次提交的树写入量因此与「改动的深度」成正比，而不是与仓库规模成正比——
/// 这正是「完整 tree 快照」能承受高频提交的原因。
///
/// 删除某个目录下的最后一个文件时，空目录会被自动摘除（与 SVN 一致：版本库不保留空目录）。
pub fn apply_changes(store: &dyn ObjectStore, base: &Hash, ops: &[TreeOp]) -> crate::Result<Hash> {
    Ok(apply_changes_tracked(store, base, ops)?.0)
}

/// 同 [`apply_changes`]，但额外返回本次**实际新写入**的树对象地址。
///
/// 上层据此只登记这些对象，避免为了索引一棵新树而把整棵树的节点遍历一遍——
/// 那会把 copy-on-write 省下的开销又还回去。
pub fn apply_changes_tracked(
    store: &dyn ObjectStore,
    base: &Hash,
    ops: &[TreeOp],
) -> crate::Result<(Hash, Vec<Hash>)> {
    // 按父目录分组：dir -> (name -> op)
    let mut grouped: BTreeMap<String, BTreeMap<String, LevelOp>> = BTreeMap::new();
    for op in ops {
        let (raw_path, level_op) = match op {
            TreeOp::Upsert {
                path,
                kind,
                hash,
                size,
            } => (
                path,
                LevelOp::Upsert {
                    kind: *kind,
                    hash: *hash,
                    size: *size,
                },
            ),
            TreeOp::Delete { path } => (path, LevelOp::Delete),
        };
        let path = crate::normalize_path(raw_path)?;
        if path.is_empty() {
            return Err(Error::new(ErrorCode::PathConflict, "不能对根目录执行变更"));
        }
        let (dir, name) = match path.rfind('/') {
            Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
            None => (String::new(), path.clone()),
        };
        grouped.entry(dir).or_default().insert(name, level_op);
    }

    let mut written = Vec::new();
    match apply_level(store, base, "", &grouped, &mut written)? {
        Some(hash) => Ok((hash, written)),
        // 根被删空：仍然要返回一个合法的空树地址。
        None => {
            let empty = write_tree(store, &[])?;
            written.push(empty);
            Ok((empty, written))
        }
    }
}

fn apply_level(
    store: &dyn ObjectStore,
    tree: &Hash,
    dir: &str,
    grouped: &BTreeMap<String, BTreeMap<String, LevelOp>>,
    written: &mut Vec<Hash>,
) -> crate::Result<Option<Hash>> {
    let mut entries: BTreeMap<String, TreeEntry> = read_tree(store, tree)?
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect();

    let mut deleted_here = std::collections::BTreeSet::new();
    if let Some(ops) = grouped.get(dir) {
        for (name, op) in ops {
            match op {
                LevelOp::Delete => {
                    // 删除不存在的路径是幂等的空操作：并发提交后重放同一变更集不会失败。
                    entries.remove(name);
                    deleted_here.insert(name.clone());
                }
                LevelOp::Upsert { kind, hash, size } => {
                    entries.insert(
                        name.clone(),
                        TreeEntry {
                            name: name.clone(),
                            kind: *kind,
                            hash: *hash,
                            size: *size,
                        },
                    );
                }
            }
        }
    }

    // 找出本层需要继续下钻的子目录（只取下一级分量）。
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut children = std::collections::BTreeSet::new();
    for key in grouped.keys() {
        if key == dir || (key.is_empty() && dir.is_empty()) {
            continue;
        }
        let rest = if prefix.is_empty() {
            key.as_str()
        } else if let Some(r) = key.strip_prefix(&prefix) {
            r
        } else {
            continue;
        };
        if let Some(first) = rest.split('/').next() {
            if !first.is_empty() {
                children.insert(first.to_string());
            }
        }
    }

    for child in children {
        if deleted_here.contains(&child) {
            // 该目录已被整棵删除，子项变更无意义，直接跳过（不是错误）。
            continue;
        }
        let child_dir = if dir.is_empty() {
            child.clone()
        } else {
            format!("{dir}/{child}")
        };
        let base_hash = match entries.get(&child) {
            Some(entry) if entry.kind == EntryKind::Dir => Some(entry.hash),
            // 同名条目存在但它是文件：往里塞子项就是路径冲突，不猜用户意图。
            Some(_) => {
                return Err(Error::new(
                    ErrorCode::PathConflict,
                    format!("路径 {child_dir} 在基线中是文件，不能再放子项"),
                ));
            }
            // 基线里没有这个目录：这是「新建目录」，从空树开始往下应用即可。
            None => None,
        };

        let base = match base_hash {
            Some(h) => h,
            None => write_tree(store, &[])?,
        };
        match apply_level(store, &base, &child_dir, grouped, written)? {
            Some(new_hash) => {
                entries.insert(
                    child.clone(),
                    TreeEntry {
                        name: child.clone(),
                        kind: EntryKind::Dir,
                        hash: new_hash,
                        size: 0,
                    },
                );
            }
            // 子树被删空：连同目录条目一起摘除。
            None => {
                entries.remove(&child);
            }
        }
    }

    if entries.is_empty() && !dir.is_empty() {
        return Ok(None);
    }
    let list: Vec<TreeEntry> = entries.into_values().collect();
    let hash = write_tree(store, &list)?;
    written.push(hash);
    Ok(Some(hash))
}

/// 扁平化整棵树，供检出、diff、状态比对与可达性扫描使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatEntry {
    pub path: String,
    pub kind: EntryKind,
    pub hash: Hash,
    pub size: u64,
}

pub fn flatten_tree(store: &dyn ObjectStore, root: &Hash) -> crate::Result<Vec<FlatEntry>> {
    let mut out = Vec::new();
    walk(store, root, "", &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(
    store: &dyn ObjectStore,
    tree: &Hash,
    prefix: &str,
    out: &mut Vec<FlatEntry>,
) -> crate::Result<()> {
    for entry in read_tree(store, tree)? {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        match entry.kind {
            EntryKind::Dir => walk(store, &entry.hash, &path, out)?,
            EntryKind::File | EntryKind::Symlink => out.push(FlatEntry {
                path,
                kind: entry.kind,
                hash: entry.hash,
                size: entry.size,
            }),
        }
    }
    Ok(())
}

/// 收集一棵树及其所有子树的地址（去重，含根）。
///
/// 对象索引需要覆盖**每一个**写下的对象，而不是只有根树：
/// 少登记一层子树，配额统计与 GC 可达性扫描就会慢慢漂移。
pub fn all_tree_hashes(store: &dyn ObjectStore, root: &Hash) -> crate::Result<Vec<Hash>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    collect_trees(store, root, &mut seen, &mut out)?;
    Ok(out)
}

fn collect_trees(
    store: &dyn ObjectStore,
    hash: &Hash,
    seen: &mut std::collections::BTreeSet<Hash>,
    out: &mut Vec<Hash>,
) -> crate::Result<()> {
    if !seen.insert(*hash) {
        return Ok(());
    }
    let entries = read_tree(store, hash)?;
    out.push(*hash);
    for entry in entries {
        if entry.kind == EntryKind::Dir {
            collect_trees(store, &entry.hash, seen, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryObjectStore;

    fn entry(name: &str, kind: EntryKind, byte: u8) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            kind,
            hash: [byte; HASH_LEN],
            size: byte as u64,
        }
    }

    #[test]
    fn encoding_is_order_independent() {
        // 核心承诺：写入顺序不影响树地址，否则去重会失效。
        let a = vec![
            entry("b.txt", EntryKind::File, 2),
            entry("a.txt", EntryKind::File, 1),
        ];
        let b = vec![
            entry("a.txt", EntryKind::File, 1),
            entry("b.txt", EntryKind::File, 2),
        ];
        assert_eq!(encode_tree(&a), encode_tree(&b));
        assert_eq!(hash_bytes(&encode_tree(&a)), hash_bytes(&encode_tree(&b)));
    }

    #[test]
    fn decode_roundtrip() {
        let entries = vec![
            entry("docs", EntryKind::Dir, 9),
            entry("a.txt", EntryKind::File, 1),
            entry("link", EntryKind::Symlink, 3),
        ];
        let decoded = decode_tree(&encode_tree(&entries)).unwrap();
        assert_eq!(decoded.len(), 3);
        // 解码后按名字有序
        let names: Vec<&str> = decoded.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "docs", "link"]);
    }

    #[test]
    fn rejects_corrupted_trees() {
        let bytes = encode_tree(&[entry("a.txt", EntryKind::File, 1)]);
        assert!(decode_tree(&bytes[..bytes.len() - 1]).is_err());
        let mut wrong_magic = bytes.clone();
        wrong_magic[0] = b'X';
        assert!(decode_tree(&wrong_magic).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(decode_tree(&extra).is_err());
    }

    #[test]
    fn builds_nested_tree_and_flattens_it() {
        let store = MemoryObjectStore::new();
        let blob = hash_bytes(b"hello");
        store.put(&blob, b"hello").unwrap();

        let files = vec![
            ("202610/a.txt".to_string(), blob, 5u64, EntryKind::File),
            ("202610/sub/b.txt".to_string(), blob, 5u64, EntryKind::File),
            ("root.txt".to_string(), blob, 5u64, EntryKind::File),
        ];
        let root = build_tree(&store, &files).unwrap();

        let flat = flatten_tree(&store, &root).unwrap();
        let paths: Vec<&str> = flat.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["202610/a.txt", "202610/sub/b.txt", "root.txt"]);
    }

    #[test]
    fn collects_every_tree_object() {
        let store = MemoryObjectStore::new();
        let blob = hash_bytes(b"x");
        store.put(&blob, b"x").unwrap();
        let files = vec![
            ("a/b/c.txt".to_string(), blob, 1u64, EntryKind::File),
            ("a/d.txt".to_string(), blob, 1u64, EntryKind::File),
        ];
        let root = build_tree(&store, &files).unwrap();
        // 根 + a + a/b 共 3 棵树
        let trees = all_tree_hashes(&store, &root).unwrap();
        assert_eq!(trees.len(), 3);
        assert!(trees.contains(&root));
    }

    #[test]
    fn shared_subtree_is_deduplicated() {
        // 两个月份目录内容完全相同时，子树对象只落盘一份。
        let store = MemoryObjectStore::new();
        let blob = hash_bytes(b"same");
        store.put(&blob, b"same").unwrap();

        let files = vec![
            ("202610/x.txt".to_string(), blob, 4u64, EntryKind::File),
            ("202611/x.txt".to_string(), blob, 4u64, EntryKind::File),
        ];
        let root = build_tree(&store, &files).unwrap();
        let entries = read_tree(&store, &root).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, entries[1].hash);
        assert_eq!(store.len(), 3); // 1 个 blob + 1 个共享子树 + 1 个根树
    }

    #[test]
    fn apply_changes_adds_modifies_and_deletes() {
        let store = MemoryObjectStore::new();
        let a = hash_bytes(b"a");
        let b = hash_bytes(b"b");
        let c = hash_bytes(b"c");
        for (h, d) in [(&a, b"a".as_slice()), (&b, b"b"), (&c, b"c")] {
            store.put(h, d).unwrap();
        }

        let base = build_tree(
            &store,
            &[
                ("202610/a.txt".to_string(), a, 1, EntryKind::File),
                ("202610/b.txt".to_string(), b, 1, EntryKind::File),
            ],
        )
        .unwrap();

        // 改 a、删 b、加 c
        let next = apply_changes(
            &store,
            &base,
            &[
                TreeOp::Upsert {
                    path: "202610/a.txt".into(),
                    kind: EntryKind::File,
                    hash: c,
                    size: 1,
                },
                TreeOp::Delete {
                    path: "202610/b.txt".into(),
                },
                TreeOp::Upsert {
                    path: "202611/c.txt".into(),
                    kind: EntryKind::File,
                    hash: c,
                    size: 1,
                },
            ],
        )
        .unwrap();

        let flat = flatten_tree(&store, &next).unwrap();
        let paths: Vec<&str> = flat.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["202610/a.txt", "202611/c.txt"]);
        let a_entry = flat.iter().find(|e| e.path == "202610/a.txt").unwrap();
        assert_eq!(a_entry.hash, c);
    }

    #[test]
    fn apply_changes_is_copy_on_write() {
        // 只改 202610 下的文件，202611 的子树哈希必须原样复用。
        let store = MemoryObjectStore::new();
        let x = hash_bytes(b"x");
        let y = hash_bytes(b"y");
        store.put(&x, b"x").unwrap();
        store.put(&y, b"y").unwrap();

        let base = build_tree(
            &store,
            &[
                ("202610/a.txt".to_string(), x, 1, EntryKind::File),
                ("202611/a.txt".to_string(), y, 1, EntryKind::File),
            ],
        )
        .unwrap();
        let before = read_tree(&store, &base).unwrap();
        let nov_before = before.iter().find(|e| e.name == "202611").unwrap().hash;

        let next = apply_changes(
            &store,
            &base,
            &[TreeOp::Upsert {
                path: "202610/a.txt".into(),
                kind: EntryKind::File,
                hash: y,
                size: 1,
            }],
        )
        .unwrap();

        let after = read_tree(&store, &next).unwrap();
        let nov_after = after.iter().find(|e| e.name == "202611").unwrap().hash;
        assert_eq!(nov_before, nov_after, "未变更的子树被无谓重写了");
    }

    #[test]
    fn apply_changes_prunes_emptied_directories() {
        let store = MemoryObjectStore::new();
        let x = hash_bytes(b"x");
        store.put(&x, b"x").unwrap();
        let base = build_tree(
            &store,
            &[("202610/only.txt".to_string(), x, 1, EntryKind::File)],
        )
        .unwrap();

        let next = apply_changes(
            &store,
            &base,
            &[TreeOp::Delete {
                path: "202610/only.txt".into(),
            }],
        )
        .unwrap();
        assert!(flatten_tree(&store, &next).unwrap().is_empty());
        assert!(read_tree(&store, &next).unwrap().is_empty());
    }

    #[test]
    fn apply_changes_delete_is_idempotent() {
        let store = MemoryObjectStore::new();
        let x = hash_bytes(b"x");
        store.put(&x, b"x").unwrap();
        let base = build_tree(&store, &[("a.txt".to_string(), x, 1, EntryKind::File)]).unwrap();
        let op = TreeOp::Delete {
            path: "a.txt".into(),
        };
        let once = apply_changes(&store, &base, std::slice::from_ref(&op)).unwrap();
        let twice = apply_changes(&store, &once, &[op]).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn rejects_file_directory_conflicts() {
        let store = MemoryObjectStore::new();
        let blob = hash_bytes(b"x");
        let files = vec![
            ("a".to_string(), blob, 1u64, EntryKind::File),
            ("a/b".to_string(), blob, 1u64, EntryKind::File),
        ];
        assert_eq!(
            build_tree(&store, &files).unwrap_err().code,
            ErrorCode::PathConflict
        );
    }
}
