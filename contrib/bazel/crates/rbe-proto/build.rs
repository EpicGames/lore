// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let proto = root.join("proto");

    let files = [
        proto.join("build/bazel/remote/execution/v2/remote_execution.proto"),
        proto.join("google/bytestream/bytestream.proto"),
        proto.join("google/longrunning/operations.proto"),
        proto.join("lore/rbe/worker/v1/worker.proto"),
    ];

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        // ByteStream carries blob content in chunks. As `Bytes`, a download is sliced into
        // response frames without copying, and an upload chunk is not copied out of the frame.
        .bytes(".google.bytestream")
        .compile_protos(&files, std::slice::from_ref(&proto))?;

    println!("cargo:rerun-if-changed={}", proto.display());
    Ok(())
}
