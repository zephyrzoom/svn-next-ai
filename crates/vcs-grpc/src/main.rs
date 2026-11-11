//! `vcs-grpc` —— svn-next 的网络服务入口。
//!
//! 与 `vcs-server` 的分工：`vcs-server` 是存储运维工具（看布局、对象数），
//! 这个二进制才是长期跑着的服务进程。两者打开的是同一个存储根目录。
//!
//! 用法：
//! ```text
//! vcs-grpc --root /srv/vcs --listen 127.0.0.1:50051
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use vcs_grpc::{serve, ServeConfig};

#[derive(Parser)]
#[command(name = "vcs-grpc", version, about = "svn-next gRPC 服务端（M3）")]
struct Cli {
    /// 服务端存储根目录（与 vcs-server / vcs CLI 共用同一个）
    #[arg(long, env = "VCS_STORE", default_value = ".vcs-server")]
    root: PathBuf,

    /// 监听地址。默认只绑本机，要对外提供服务请显式指定 0.0.0.0 并自行加 TLS 网关。
    #[arg(long, env = "VCS_LISTEN", default_value = "127.0.0.1:50051")]
    listen: SocketAddr,

    /// 后台维护间隔（秒）：转正到期清理 + 全库 GC。0 表示关闭（只能手工 vcs gc）。
    #[arg(long, env = "VCS_MAINTENANCE_INTERVAL", default_value_t = 300)]
    maintenance_interval: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    serve(ServeConfig {
        root: cli.root,
        listen: cli.listen,
        maintenance_interval_secs: cli.maintenance_interval,
    })
    .await
}
