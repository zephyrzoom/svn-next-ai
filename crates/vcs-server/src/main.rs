//! vcs-server 二进制入口。
//!
//! M1 阶段这个二进制不是网络服务，而是「服务端存储」的运维命令行：
//! gRPC 服务在 M3 挂上 `ServerStore`，届时这里会多一个 `serve` 子命令，
//! 业务层代码不需要改动。

use std::path::PathBuf;
use std::process::ExitCode;

use vcs_core::{ObjectStore, CAS_LAYOUT};
use vcs_server::ServerStore;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut root = PathBuf::from(".vcs-server");

    let mut command = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" | "-r" => {
                if let Some(v) = args.next() {
                    root = PathBuf::from(v);
                }
            }
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => command = Some(other.to_string()),
        }
    }

    match command.as_deref() {
        None | Some("info") => match run_info(&root) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("错误：{e}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            eprintln!("未知命令：{other}");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn run_info(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = ServerStore::open(root)?;
    let objects = store.objects();
    let hashes = objects.list_hashes()?;
    let stored: u64 = hashes
        .iter()
        .map(|h| objects.stored_size(h).unwrap_or(0))
        .sum();

    println!("vcs-server {} (M1)", env!("CARGO_PKG_VERSION"));
    println!("root      : {}", store.root().display());
    println!("protocol  : proto/repo.proto (vcs.repo.v1)");
    println!("schema    : schema/server.sql");
    println!(
        "layout    : {CAS_LAYOUT}（objects/ 全局去重，压缩后 {} 字节）",
        stored
    );
    println!("objects   : {} 个", hashes.len());
    println!(
        "users     : {}",
        if store.has_any_user()? {
            "已初始化"
        } else {
            "未初始化（需 init-admin）"
        }
    );
    Ok(())
}

fn print_help() {
    println!("用法：vcs-server [--root <目录>] [info]");
    println!();
    println!("  info    显示存储目录统计（默认命令）");
    println!();
    println!("M1 阶段服务端以本地存储目录形态存在；gRPC 服务在 M3 挂载。");
}
