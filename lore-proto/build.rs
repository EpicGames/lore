// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::env;
use std::io::Result;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Resolve the `protoc` binary the same way prost-build does: the `PROTOC`
/// environment variable if set, otherwise `protoc` from `PATH`.
fn protoc_path() -> PathBuf {
    env::var_os("PROTOC").map_or_else(|| PathBuf::from("protoc"), PathBuf::from)
}

/// Returns true if `protoc` can actually be executed. When it can't, the build
/// falls back to the pregenerated sources checked in under `src/grpc`.
fn protoc_available() -> bool {
    Command::new(protoc_path())
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Emit `cargo:rerun-if-changed` for every `.proto` under `dir` so the bindings
/// are regenerated only when an input actually changes.
fn rerun_if_proto_changed(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rerun_if_proto_changed(&path)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("proto") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("No manifest dir set");
    let proto_dir = PathBuf::from(&crate_dir).join("proto");
    let output_dir = PathBuf::from(&crate_dir).join("src").join("grpc");

    // Declare the codegen inputs unconditionally so Cargo re-runs this script
    // when PROTOC changes or a watched .proto is edited — even on a build where
    // protoc is missing. Emitting these before the availability check is what
    // lets a fixed PROTOC (or a newly installed protoc plus a .proto edit)
    // recover and regenerate; if they were only emitted after the early return,
    // the protoc-missing run would record no watches and fixing PROTOC alone
    // would never re-run this script.
    println!("cargo:rerun-if-env-changed=PROTOC");
    rerun_if_proto_changed(&proto_dir)?;

    // protoc is only needed to regenerate the gRPC bindings. When it isn't
    // installed, rely on the pregenerated sources committed under src/grpc so
    // the crate still builds.
    if !protoc_available() {
        return Ok(());
    }

    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    // Use Bytes for buffers instead of Vec
    config.bytes(["."]);

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(
            config,
            &[
                "./proto/model.proto",
                "./proto/admin.proto",
                "./proto/lock.proto",
                "./proto/epic_events.proto",
                "./proto/lore_notification.proto",
                "./proto/notification.proto",
                "./proto/replication.proto",
            ],
            &["./proto"],
        )?;

    // lore.model.v1 — shared base types
    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(config, &["./proto/lore/model/v1/model.proto"], &["./proto"])?;

    // lore.storage.v1 — storage service, references lore.model.v1 via extern_path
    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);
    config.extern_path(".lore.model.v1", "crate::lore::model::v1");

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(
            config,
            &["./proto/lore/storage/v1/storage.proto"],
            &["./proto"],
        )?;

    // lore.revision.v1 — baseline revision-graph service, references lore.model.v1
    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);
    config.extern_path(".lore.model.v1", "crate::lore::model::v1");

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(
            config,
            &["./proto/lore/revision/v1/revision.proto"],
            &["./proto"],
        )?;

    // lore.repository.v1 — repository-management service, references lore.model.v1
    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);
    config.extern_path(".lore.model.v1", "crate::lore::model::v1");

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(
            config,
            &["./proto/lore/repository/v1/repository.proto"],
            &["./proto"],
        )?;

    // lore.thin_client.v1 — thin-client presentation helpers, references lore.model.v1.
    // model.proto and thin_client.proto share the same package and are compiled
    // together into a single generated module.
    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);
    config.extern_path(".lore.model.v1", "crate::lore::model::v1");

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(
            config,
            &[
                "./proto/lore/thin_client/v1/model.proto",
                "./proto/lore/thin_client/v1/thin_client.proto",
            ],
            &["./proto"],
        )?;

    // lore.environment.v1 — server-side environment discovery service. Self-contained — declares its own messages
    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(
            config,
            &["./proto/lore/environment/v1/environment.proto"],
            &["./proto"],
        )?;

    // lore.user.v1 — user directory, a wire-compatible copy of the UrcAuthApi
    // directory RPCs. Self-contained — declares its own messages
    let mut config = tonic_prost_build::Config::new();
    config.enable_type_names();
    config.bytes(["."]);

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_with_config(config, &["./proto/lore/user/v1/user.proto"], &["./proto"])?;

    let mut config = tonic_prost_build::Config::new();
    // Use Bytes for buffers instead of Vec
    config.bytes(["."]);

    tonic_prost_build::configure()
        .out_dir(&output_dir)
        .protoc_arg("--experimental_allow_proto3_optional")
        .build_server(false)
        .compile_with_config(
            config,
            &["./proto/auth_api.proto", "./proto/rebac_api.proto"],
            &["./proto"],
        )?;

    Ok(())
}
