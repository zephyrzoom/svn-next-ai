//! 服务器数据卷的容量探测。
//!
//! 扩容审批要回答的是一个物理问题：**这台机器的磁盘还放得下吗**。
//! 数据库里的 `used_bytes` 只是对象池自己记账的字节数，回答不了这个问题，
//! 所以这里直接问文件系统。
//!
//! 只依赖目标平台各自的标准接口（Unix 用 `statvfs`，Windows 用
//! `GetDiskFreeSpaceExW`），不引入额外的第三方容量探测库。

use std::path::Path;

use vcs_core::{Error, ErrorCode, Result};

/// 数据卷容量快照。`used_bytes` 是整块卷已占用的字节数，
/// 与对象池记账的 `object_bytes` 不是一回事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageCapacity {
    pub total_bytes: u64,
    /// 非特权用户实际可用的字节数（Unix 下是 `f_bavail`，已扣掉 root 保留块）。
    pub available_bytes: u64,
    pub used_bytes: u64,
}

impl StorageCapacity {
    fn from_parts(total_bytes: u64, available_bytes: u64) -> Self {
        let available_bytes = available_bytes.min(total_bytes);
        Self {
            total_bytes,
            available_bytes,
            used_bytes: total_bytes - available_bytes,
        }
    }
}

/// 探测 `path` 所在卷的容量。路径不存在时按它的父目录往上找，
/// 避免「刚装好还没建目录」时报错。
pub fn disk_capacity(path: &Path) -> Result<StorageCapacity> {
    let target = existing_ancestor(path);
    query(&target).map_err(|e| {
        Error::new(
            ErrorCode::IoError,
            format!("读取磁盘容量失败（{}）：{e}", target.display()),
        )
    })
}

fn existing_ancestor(path: &Path) -> std::path::PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => return std::path::PathBuf::from("."),
        }
    }
}

#[cfg(unix)]
fn query(path: &Path) -> std::io::Result<StorageCapacity> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径包含 NUL 字节"))?;
    // SAFETY: `statvfs` 只写入我们提供的结构体；c_path 在调用期间保持有效。
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: 指针来自上面的有效 CString，结构体内存由栈上变量保证。
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // `f_frsize` 是片段大小；个别文件系统只填 `f_bsize`，退回它即可。
    let block = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    Ok(StorageCapacity::from_parts(
        stat.f_blocks as u64 * block,
        stat.f_bavail as u64 * block,
    ))
}

#[cfg(windows)]
fn query(path: &Path) -> std::io::Result<StorageCapacity> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut free_total: u64 = 0;
    // SAFETY: wide 以 NUL 结尾且长度与指针一致；三个输出指针都指向栈上 u64。
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free_total) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(StorageCapacity::from_parts(total, available))
}

#[cfg(not(any(unix, windows)))]
fn query(_path: &Path) -> std::io::Result<StorageCapacity> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "该平台暂不支持磁盘容量探测",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_capacity_of_temp_dir() {
        let dir = std::env::temp_dir();
        let cap = disk_capacity(&dir).expect("读取临时目录容量");
        assert!(cap.total_bytes > 0);
        assert!(cap.available_bytes <= cap.total_bytes);
        assert_eq!(cap.used_bytes + cap.available_bytes, cap.total_bytes);
    }

    #[test]
    fn missing_path_falls_back_to_ancestor() {
        let dir = std::env::temp_dir().join("vcs-disk-probe-not-created");
        let cap = disk_capacity(&dir).expect("路径不存在时按祖先目录探测");
        assert!(cap.total_bytes > 0);
    }
}
