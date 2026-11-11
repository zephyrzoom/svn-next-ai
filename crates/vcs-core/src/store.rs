//! 本地文件系统 CAS 实现。
//!
//! 布局（全仓库唯一一份，SHARED_CAS）：
//!
//! ```text
//! <root>/objects/ab/cdef...   对象本体，文件名 = blake3 十六进制
//! <root>/tmp/                 写入暂存区，仅用于原子 rename
//! ```
//!
//! 三条不变式：
//!   1. 对象一经写入不再修改（不可变），所以「存在即可跳过」是安全的幂等写入；
//!   2. 写入先落 `tmp/` 再 `rename` 到最终路径，进程中断不会留下半个对象；
//!   3. 落盘前校验内容摘要与调用方给出的地址一致，宁可报错也不污染对象池。
//!
//! 压缩：文件首字节是标记，`0` 表示原样存储、`1` 表示 zstd 帧。
//! 小对象压缩反而变大，所以按「压缩后更小才压」决策，代价是读路径多一次分支。

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::hash::{hash_bytes, object_rel_path};
use crate::{Error, ErrorCode, Hash, ObjectStore, Result};

const FLAG_RAW: u8 = 0;
const FLAG_ZSTD: u8 = 1;
const ZSTD_LEVEL: i32 = 3;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct FileObjectStore {
    root: PathBuf,
    objects: PathBuf,
    tmp: PathBuf,
}

impl FileObjectStore {
    /// 打开（不存在则创建）一个 CAS 根目录。
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        for dir in [&root, &objects, &tmp] {
            fs::create_dir_all(dir).map_err(|e| io_err("创建 CAS 目录失败", dir, e))?;
        }
        Ok(Self { root, objects, tmp })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, hash: &Hash) -> PathBuf {
        self.objects.join(object_rel_path(hash))
    }

    fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        let compressed = zstd::bulk::compress(data, ZSTD_LEVEL)
            .map_err(|e| Error::new(ErrorCode::IoError, format!("zstd 压缩失败：{e}")))?;
        if compressed.len() + 1 < data.len() + 1 {
            let mut buf = Vec::with_capacity(compressed.len() + 1);
            buf.push(FLAG_ZSTD);
            buf.extend_from_slice(&compressed);
            Ok(buf)
        } else {
            let mut buf = Vec::with_capacity(data.len() + 1);
            buf.push(FLAG_RAW);
            buf.extend_from_slice(data);
            Ok(buf)
        }
    }

    fn decompress(&self, raw: &[u8]) -> Result<Vec<u8>> {
        let (flag, body) = raw
            .split_first()
            .ok_or_else(|| Error::new(ErrorCode::ObjectMissing, "对象文件为空"))?;
        match *flag {
            FLAG_RAW => Ok(body.to_vec()),
            FLAG_ZSTD => zstd::bulk::decompress(body, 512 * 1024 * 1024)
                .map_err(|e| Error::new(ErrorCode::ObjectMissing, format!("zstd 解压失败：{e}"))),
            other => Err(Error::new(
                ErrorCode::ObjectMissing,
                format!("未知的对象压缩标记：{other}"),
            )),
        }
    }

    fn unique_tmp_name(&self) -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.tmp
            .join(format!("obj-{}-{nanos}-{seq}.part", std::process::id()))
    }

    /// 遍历所有对象地址，供 GC 可达性扫描与配额统计使用。
    pub fn list_hashes(&self) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        let shards = fs::read_dir(&self.objects)
            .map_err(|e| io_err("读取对象目录失败", &self.objects, e))?;
        for shard in shards {
            let shard = shard.map_err(|e| io_err("读取分片目录失败", &self.objects, e))?;
            if !shard.path().is_dir() {
                continue;
            }
            let prefix = shard.file_name().to_string_lossy().to_string();
            if prefix.len() != 2 {
                continue;
            }
            for item in
                fs::read_dir(shard.path()).map_err(|e| io_err("读取对象失败", &shard.path(), e))?
            {
                let item = item.map_err(|e| io_err("读取对象失败", &shard.path(), e))?;
                let name = item.file_name().to_string_lossy().to_string();
                if !name.ends_with(".part") {
                    if let Ok(hash) = crate::hash::from_hex(&format!("{prefix}{name}")) {
                        out.push(hash);
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// 对象池当前占用的物理字节数（压缩后），即磁盘上真正被吃掉的空间。
    pub fn total_stored_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        for hash in self.list_hashes()? {
            total += self.stored_size(&hash)?;
        }
        Ok(total)
    }

    /// 清理 `tmp/` 中的残留（进程崩溃留下的半成品）。
    pub fn sweep_tmp(&self) -> Result<usize> {
        let mut removed = 0;
        for item in fs::read_dir(&self.tmp).map_err(|e| io_err("读取 tmp 失败", &self.tmp, e))?
        {
            let item = item.map_err(|e| io_err("读取 tmp 失败", &self.tmp, e))?;
            if item.file_name().to_string_lossy().ends_with(".part")
                && fs::remove_file(item.path()).is_ok()
            {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

impl ObjectStore for FileObjectStore {
    fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        let path = self.path_of(hash);
        let mut file = File::open(&path).map_err(|_| {
            Error::new(
                ErrorCode::ObjectMissing,
                format!("对象不存在：{}", crate::hash::to_hex(hash)),
            )
        })?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)
            .map_err(|e| io_err("读取对象失败", &path, e))?;
        self.decompress(&raw)
    }

    fn put(&self, hash: &Hash, data: &[u8]) -> Result<()> {
        let actual = hash_bytes(data);
        if actual != *hash {
            return Err(Error::new(
                ErrorCode::ObjectMissing,
                format!(
                    "内容摘要与地址不一致，拒绝写入：期望 {} 实际 {}",
                    crate::hash::to_hex(hash),
                    crate::hash::to_hex(&actual)
                ),
            ));
        }
        let target = self.path_of(hash);
        if target.exists() {
            // 不可变 + 地址即内容 => 已存在必然是同一份内容，直接跳过。
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err("创建分片目录失败", parent, e))?;
        }

        let payload = self.compress(data)?;
        let tmp = self.unique_tmp_name();
        {
            let mut file = File::create(&tmp).map_err(|e| io_err("创建暂存文件失败", &tmp, e))?;
            file.write_all(&payload)
                .map_err(|e| io_err("写暂存文件失败", &tmp, e))?;
            file.sync_all()
                .map_err(|e| io_err("刷新暂存文件失败", &tmp, e))?;
        }
        // rename 在同一文件系统内是原子的：要么看不到对象，要么看到完整对象。
        match fs::rename(&tmp, &target) {
            Ok(()) => Ok(()),
            Err(_) if target.exists() => {
                let _ = fs::remove_file(&tmp);
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(io_err("对象落盘失败", &target, e))
            }
        }
    }

    fn exists(&self, hash: &Hash) -> bool {
        self.path_of(hash).exists()
    }

    fn remove(&self, hash: &Hash) -> Result<()> {
        let path = self.path_of(hash);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::new(
                ErrorCode::ObjectMissing,
                format!("对象不存在：{}", crate::hash::to_hex(hash)),
            )),
            Err(e) => Err(io_err("删除对象失败", &path, e)),
        }
    }

    fn stored_size(&self, hash: &Hash) -> Result<u64> {
        let path = self.path_of(hash);
        let meta = fs::metadata(&path).map_err(|e| io_err("读取对象元数据失败", &path, e))?;
        Ok(meta.len())
    }
}

fn io_err(action: &str, path: &Path, err: std::io::Error) -> Error {
    Error::new(
        ErrorCode::IoError,
        format!("{action}：{}（{err}）", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "vcs-core-{tag}-{}-{}",
                std::process::id(),
                TMP_SEQ.fetch_add(1, Ordering::Relaxed)
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

    #[test]
    fn roundtrip_through_disk() {
        let dir = TempDir::new("roundtrip");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        let data = b"202610 monthly report".repeat(64);
        let hash = hash_bytes(&data);
        assert!(!store.exists(&hash));
        store.put(&hash, &data).unwrap();
        assert!(store.exists(&hash));
        assert_eq!(store.get(&hash).unwrap(), data);
        // 重复写入是幂等的
        store.put(&hash, &data).unwrap();
        assert_eq!(store.list_hashes().unwrap().len(), 1);
    }

    #[test]
    fn repetitive_content_is_compressed() {
        let dir = TempDir::new("compress");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        let data = vec![b'a'; 64 * 1024];
        let hash = hash_bytes(&data);
        store.put(&hash, &data).unwrap();
        let stored = store.stored_size(&hash).unwrap();
        assert!(stored < data.len() as u64 / 10, "压缩未生效：{stored}");
        assert_eq!(store.get(&hash).unwrap().len(), data.len());
    }

    #[test]
    fn tiny_content_is_stored_raw() {
        let dir = TempDir::new("tiny");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        let data = b"x";
        let hash = hash_bytes(data);
        store.put(&hash, data).unwrap();
        assert_eq!(store.stored_size(&hash).unwrap(), 2); // 1 字节标记 + 1 字节内容
    }

    #[test]
    fn rejects_address_mismatch() {
        let dir = TempDir::new("mismatch");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        let wrong = [0u8; 32];
        let err = store.put(&wrong, b"content").unwrap_err();
        assert_eq!(err.code, ErrorCode::ObjectMissing);
        assert!(!store.exists(&wrong));
    }

    #[test]
    fn missing_object_reports_object_missing() {
        let dir = TempDir::new("missing");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        assert_eq!(
            store.get(&[3u8; 32]).unwrap_err().code,
            ErrorCode::ObjectMissing
        );
    }

    #[test]
    fn sweep_removes_partial_files_only() {
        let dir = TempDir::new("sweep");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        let data = b"keep me";
        let hash = hash_bytes(data);
        store.put(&hash, data).unwrap();
        fs::write(store.tmp.join("obj-dead.part"), b"partial").unwrap();
        assert_eq!(store.sweep_tmp().unwrap(), 1);
        assert!(store.exists(&hash));
        // "keep me" 7 字节 + 1 字节原始标记
        assert_eq!(store.total_stored_bytes().unwrap(), 8);
    }
}
