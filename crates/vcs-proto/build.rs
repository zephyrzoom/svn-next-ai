//! 把 proto/repo.proto 编译成 Rust gRPC 代码。
//!
//! 使用 protoc-bin-vendored 提供 protoc，CI 与开发机都不需要预装。
//! 契约文件本身只有一份（proto/repo.proto），代码生成物不落盘到源码树，
//! 全部输出到 OUT_DIR，避免生成代码与手写代码混在一起。

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let proto_file = proto_dir.join("repo.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    // protoc 由 vendored 包提供，避免依赖宿主环境。
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        // 注意：不要用 server_mod_attribute / client_mod_attribute 注入
        // `#[allow(clippy::result_large_err)]`：tonic 生成的模块自带内部
        // `#![allow(...)]`，内外属性混用反而会触发 clippy::mixed_attributes_style。
        // 该 lint 的豁免统一放在 src/lib.rs 的 include_proto! 边界。
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
