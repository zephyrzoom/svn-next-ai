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
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::hash::{hash_bytes, object_rel_path};
use crate::{Error, ErrorCode, Hash, ObjectReader, ObjectStore, Result, STREAM_BUF};

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

    /// 把暂存文件原子地挪到最终路径。
    ///
    /// rename 在同一文件系统内是原子的：要么看不到对象，要么看到完整对象。并发写同一个
    /// 对象时后到者丢弃自己的副本——地址即内容，两份字节必然相同。
    fn commit_tmp(&self, tmp: PathBuf, target: PathBuf) -> Result<()> {
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
        let mut reader = self.open(hash)?;
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .map_err(|e| Error::new(ErrorCode::ObjectMissing, format!("读取对象失败：{e}")))?;
        Ok(out)
    }

    /// 流式读取：只读一个标记字节，剩下的交给文件 / zstd 解码器，
    /// 内存占用与对象大小无关（这也是「检出几 GB 文件不炸」的前提）。
    fn open(&self, hash: &Hash) -> Result<ObjectReader> {
        let path = self.path_of(hash);
        let mut file = File::open(&path).map_err(|_| {
            Error::new(
                ErrorCode::ObjectMissing,
                format!("对象不存在：{}", crate::hash::to_hex(hash)),
            )
        })?;
        let mut flag = [0u8; 1];
        file.read_exact(&mut flag)
            .map_err(|e| io_err("读取对象标记失败", &path, e))?;
        let reader = BufReader::new(file);
        match flag[0] {
            FLAG_RAW => Ok(Box::new(reader) as ObjectReader),
            FLAG_ZSTD => {
                let decoder = zstd::stream::read::Decoder::new(reader).map_err(|e| {
                    Error::new(ErrorCode::ObjectMissing, format!("zstd 解码失败：{e}"))
                })?;
                Ok(Box::new(BufReader::new(decoder)) as ObjectReader)
            }
            other => Err(Error::new(
                ErrorCode::ObjectMissing,
                format!("未知的对象压缩标记：{other}"),
            )),
        }
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
        self.commit_tmp(tmp, target)
    }

    /// 从磁盘文件流式入库：一遍读出算摘要 + 压缩，再按「压缩后更小才压」定稿。
    ///
    /// 内存只用一个 64 KiB 缓冲：推上来的对象先落在临时文件里，这里按文件读，
    /// 不会把整个对象塞进内存。压缩不划算时再读一遍源文件写成 raw——多一次 IO，
    /// 换来的是「对象多大都不影响内存」。
    fn put_from_path(&self, hash: &Hash, src: &Path) -> Result<()> {
        let target = self.path_of(hash);
        if target.exists() {
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err("创建分片目录失败", parent, e))?;
        }
        let raw_size = fs::metadata(src)
            .map_err(|e| io_err("读取待入库文件大小失败", src, e))?
            .len();

        // 第 1 遍：边读边算摘要、边压缩到临时文件。
        let compressed_tmp = self.unique_tmp_name();
        let mut hasher = blake3::Hasher::new();
        let mut input =
            BufReader::new(File::open(src).map_err(|e| io_err("打开待入库文件失败", src, e))?);
        let compressed_size = {
            let out = File::create(&compressed_tmp)
                .map_err(|e| io_err("创建压缩暂存失败", &compressed_tmp, e))?;
            let mut encoder = zstd::stream::write::Encoder::new(BufWriter::new(out), ZSTD_LEVEL)
                .map_err(|e| Error::new(ErrorCode::IoError, format!("zstd 压缩失败：{e}")))?;
            let mut buf = vec![0u8; STREAM_BUF];
            loop {
                let n = input
                    .read(&mut buf)
                    .map_err(|e| io_err("读取待入库文件失败", src, e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                encoder
                    .write_all(&buf[..n])
                    .map_err(|e| Error::new(ErrorCode::IoError, format!("zstd 压缩失败：{e}")))?;
            }
            let mut out = encoder
                .finish()
                .map_err(|e| Error::new(ErrorCode::IoError, format!("zstd 收尾失败：{e}")))?;
            out.flush()
                .map_err(|e| io_err("刷新压缩暂存失败", &compressed_tmp, e))?;
            out.get_ref()
                .metadata()
                .map_err(|e| io_err("读取压缩结果大小失败", &compressed_tmp, e))?
                .len()
        };
        let actual = *hasher.finalize().as_bytes();
        if actual != *hash {
            let _ = fs::remove_file(&compressed_tmp);
            return Err(Error::new(
                ErrorCode::ObjectMissing,
                format!(
                    "内容摘要与地址不一致，拒绝写入：期望 {} 实际 {}",
                    crate::hash::to_hex(hash),
                    crate::hash::to_hex(&actual)
                ),
            ));
        }

        // 第 2 遍：写最终对象（标记字节 + 正文），只保留更小的一种表示。
        let use_zstd = compressed_size + 1 < raw_size + 1;
        let staged = self.unique_tmp_name();
        {
            let out = File::create(&staged).map_err(|e| io_err("创建暂存文件失败", &staged, e))?;
            let mut out = BufWriter::new(out);
            out.write_all(&[if use_zstd { FLAG_ZSTD } else { FLAG_RAW }])
                .map_err(|e| io_err("写对象标记失败", &staged, e))?;
            let body_path = if use_zstd { &compressed_tmp } else { src };
            let mut body = BufReader::new(
                File::open(body_path).map_err(|e| io_err("打开对象正文失败", body_path, e))?,
            );
            std::io::copy(&mut body, &mut out).map_err(|e| io_err("写暂存文件失败", &staged, e))?;
            out.flush()
                .map_err(|e| io_err("刷新暂存文件失败", &staged, e))?;
            out.get_ref()
                .sync_all()
                .map_err(|e| io_err("刷新暂存文件失败", &staged, e))?;
        }
        let _ = fs::remove_file(&compressed_tmp);
        self.commit_tmp(staged, target)
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

    /// 单个对象没有大小上限：解压不再用「固定输出上限」的 bulk 接口。
    ///
    /// 520 MiB 超过原来 `bulk::decompress(…, 512 MiB)` 的天花板，标了 `ignore`
    /// 以免 CI 常态吃 500+ MiB 内存；需要时手动跑
    /// `cargo test -p vcs-core -- --ignored big_object_has_no_size_cap`。
    #[test]
    #[ignore = "需要 500 MiB 以上内存与磁盘，手动执行"]
    fn big_object_has_no_size_cap() {
        let dir = TempDir::new("big");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        // 全零内容压缩率极高，落盘只有几百 KB，但解压后是 520 MiB。
        let data = vec![0u8; 520 * 1024 * 1024];
        let hash = hash_bytes(&data);
        store.put(&hash, &data).unwrap();
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

    /// 流式读写：内容不进内存，且压缩与否都能读回来。
    #[test]
    fn stream_put_and_open_round_trip() {
        let dir = TempDir::new("stream");
        let store = FileObjectStore::open(dir.0.join("cas")).unwrap();
        // 4 MiB 高度重复内容：会走 zstd 分支；再写一份随机内容走 raw 分支。
        let mut repetitive = vec![0u8; 4 * 1024 * 1024];
        for (i, byte) in repetitive.iter_mut().enumerate() {
            *byte = (i % 97) as u8;
        }
        // 伪随机内容压缩不动，用来覆盖 raw 分支。
        let mut seed = 0x1234_5678_9abc_def0u64;
        let raw: Vec<u8> = (0..1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed & 0xff) as u8
            })
            .collect();

        for (name, data) in [("repetitive", &repetitive), ("raw", &raw)] {
            let src = dir.0.join(name);
            fs::write(&src, data).unwrap();
            let hash = hash_bytes(data);
            store.put_from_path(&hash, &src).unwrap();
            let mut reader = store.open(&hash).unwrap();
            let mut back = Vec::new();
            reader.read_to_end(&mut back).unwrap();
            assert_eq!(&back, data, "{name} 流式读回来的内容必须一致");
        }

        // 重复内容应当被压缩；随机小内容保持原样（多 1 字节标记）。
        let repetitive_hash = hash_bytes(&repetitive);
        assert!(store.stored_size(&repetitive_hash).unwrap() < repetitive.len() as u64 / 4);
        repetitive.clear();
        assert_eq!(
            store.stored_size(&hash_bytes(&raw)).unwrap(),
            raw.len() as u64 + 1
        );
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
