// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::env;
use std::path::Path;

include!("../build-helper.rs");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Populate environment with build details
    vergen::Emitter::default()
        .add_custom_instructions(&LoreVergen::default())?
        .emit()?;

    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("No manifest dir set");
    let native_dir = Path::join(Path::new(&crate_dir), "native");

    let platform = env::var("CARGO_CFG_TARGET_OS").expect("No target OS set");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("No target arch set");

    let mut cc_base_builder = cc::Build::new();
    let cc_builder = cc_base_builder
        .cargo_metadata(true)
        .static_crt(true)
        .force_frame_pointer(false)
        .opt_level(3)
        .define("RPMALLOC_FIRST_CLASS_HEAPS", "1")
        // rpmalloc is used as an explicit allocator, not a process-wide malloc override.
        .define("ENABLE_OVERRIDE", "0")
        .includes(Some(native_dir.join("thirdparty")));

    // Matches the -C target-cpu that .cargo/config.toml pins for this target,
    // so the C and Rust halves agree. Overridable, and settable to empty for a
    // baseline armv8-a build: neoverse-512tvb raises the architecture floor as
    // well as the tuning, and GCC then emits stlur (FEAT_LRCPC2, armv8.4) into
    // rpmalloc, which is undefined on Neoverse N1 parts such as Ampere Altra.
    println!("cargo:rerun-if-env-changed=LORE_ARM64_TARGET_CPU");
    if platform == "linux" && arch == "aarch64" {
        let target_cpu =
            env::var("LORE_ARM64_TARGET_CPU").unwrap_or_else(|_| "neoverse-512tvb".to_string());
        if !target_cpu.is_empty() {
            cc_builder.flag(format!("-mcpu={target_cpu}"));
        }
    }

    if cc_builder.get_compiler().is_like_msvc() {
        cc_builder.flag("/experimental:c11atomics");
        cc_builder.flag("/std:c11");
    }

    let rpmalloc_source = native_dir
        .join("thirdparty")
        .join("rpmalloc")
        .join("rpmalloc.c");
    let rpmalloc_header = native_dir
        .join("thirdparty")
        .join("rpmalloc")
        .join("rpmalloc.h");
    println!("cargo:rerun-if-changed={}", rpmalloc_source.display());
    println!("cargo:rerun-if-changed={}", rpmalloc_header.display());
    cc_builder.clone().file(rpmalloc_source).compile("rpmalloc");

    Ok(())
}
