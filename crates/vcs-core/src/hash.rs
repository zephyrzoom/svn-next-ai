//! 内容寻址的地址计算与表示。
//!
//! 对象地址一律是 blake3-256 摘要的 32 字节值，对外表示为 64 位小写十六进制。
//! 目录布局为 `objects/<前 2 位>/<其余 62 位>`：单目录条目数被摊平，
//! 避免几百万对象堆在一个目录里拖慢 ext4 / APFS 的查找。

use crate::{Error, ErrorCode, Hash, HASH_LEN};

/// 流式读取的缓冲区大小。哈希、写入、校验都用它，保证内存与对象大小无关。
pub const STREAM_BUF: usize = 64 * 1024;

/// 计算对象地址。CAS 的唯一写入入口都走这里，保证「地址 = 内容摘要」。
pub fn hash_bytes(data: &[u8]) -> Hash {
    *blake3::hash(data).as_bytes()
}

/// 流式计算对象地址（攒够一块喂一次 hasher），用于文件上传前的校验。
///
/// 与 [`hash_bytes`] 结果完全一致，区别只是**内存只占一个缓冲区**：
/// 几个 GB 的文件也不会被整体读进内存。
pub fn hash_reader(reader: &mut dyn std::io::Read) -> std::io::Result<(Hash, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; STREAM_BUF];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((*hasher.finalize().as_bytes(), total))
}

/// 流式计算文件摘要，返回（地址，字节数）。
pub fn hash_file(path: &std::path::Path) -> std::io::Result<(Hash, u64)> {
    let mut file = std::fs::File::open(path)?;
    hash_reader(&mut file)
}

/// 空目录树的地址，用于初始化空仓库的 root_tree。
pub fn empty_tree_hash() -> Hash {
    hash_bytes(&crate::tree::encode_tree(&[]))
}

/// 转为 64 位小写十六进制。
pub fn to_hex(hash: &Hash) -> String {
    blake3::Hash::from_bytes(*hash).to_hex().to_string()
}

/// 解析十六进制；长度或字符非法一律视为协议错误而不是 panic。
pub fn from_hex(input: &str) -> crate::Result<Hash> {
    let parsed = blake3::Hash::from_hex(input.trim())
        .map_err(|_| Error::new(ErrorCode::ObjectMissing, format!("非法的对象地址：{input}")))?;
    Ok(*parsed.as_bytes())
}

/// CAS 内的相对路径，形如 `ab/cdef...`（共 64 位十六进制）。
pub fn object_rel_path(hash: &Hash) -> String {
    let hex = to_hex(hash);
    debug_assert_eq!(hex.len(), HASH_LEN * 2);
    format!("{}/{}", &hex[..2], &hex[2..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_same_address() {
        // 去重的前提：内容相同则地址必然相同，与写入次数和路径无关。
        assert_eq!(
            hash_bytes(b"202610/report.xlsx"),
            hash_bytes(b"202610/report.xlsx")
        );
        assert_ne!(hash_bytes(b"a"), hash_bytes(b"b"));
    }

    #[test]
    fn hex_roundtrip() {
        let h = hash_bytes(b"hello");
        let hex = to_hex(&h);
        assert_eq!(hex.len(), 64);
        assert_eq!(from_hex(&hex).unwrap(), h);
        assert!(from_hex("zz").is_err());
        assert!(from_hex("").is_err());
    }

    #[test]
    fn object_paths_are_two_level() {
        let h = hash_bytes(b"x");
        let p = object_rel_path(&h);
        assert_eq!(p.len(), 2 + 1 + 62);
        assert_eq!(&p[2..3], "/");
    }
}
