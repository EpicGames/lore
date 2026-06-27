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
        .includes(Some(native_dir.join("thirdparty")));

    if platform == "linux" && arch == "aarch64" {
        if env::var("LORE_CPU_NEOVERSE_512TVB").is_ok() {
            let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
            if cpuinfo.contains("sve2") {
                cc_builder.flag("-mcpu=neoverse-512tvb");
            } else {
                println!(
                    "cargo:warning=LORE_CPU_NEOVERSE_512TVB is set but SVE2 not detected in /proc/cpuinfo; skipping -mcpu=neoverse-512tvb and building for generic aarch64 to avoid illegal hardware instruction. Disable the `neoverse-512tvb` feature to suppress this warning"
                );
            }
        } else {
            println!(
                "cargo:warning=Building rpmalloc without -mcpu=neoverse-512tvb; binary may be slower on Graviton3+. Set LORE_CPU_NEOVERSE_512TVB=1 to opt in"
            );
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
