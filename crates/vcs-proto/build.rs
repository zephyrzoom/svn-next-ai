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
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
