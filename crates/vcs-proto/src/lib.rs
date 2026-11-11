//! svn-next gRPC 契约的 Rust 绑定。
//!
//! 真源永远是 `proto/repo.proto`，本 crate 只负责代码生成，不手写任何消息定义，
//! 也不在这里添加业务逻辑——业务逻辑全部在 `vcs-core` / `vcs-server`。
//!
//! 生成物同时包含服务端与客户端两套代码：
//!   - `repo_service_server` / `auth_service_server` / `repo_admin_service_server`
//!   - `repo_service_client` / `auth_service_client` / `repo_admin_service_client`

pub mod vcs {
    pub mod repo {
        pub mod v1 {
            // tonic 0.12 生成的 trait / client 方法都返回 `Result<_, tonic::Status>`，
            // 而 `Status` 有 176 字节；clippy 1.98+ 会穿透 include_proto! 对生成代码
            // 报 result_large_err。签名由 tonic 决定、无法装箱，因此只在生成代码
            // 的边界放行，不放宽手写代码的 lint。
            #![allow(clippy::result_large_err)]

            tonic::include_proto!("vcs.repo.v1");
        }
    }
}

pub use vcs::repo::v1::*;
