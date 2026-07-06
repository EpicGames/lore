// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::env;
use std::path::Path;

include!("../build-helper.rs");

const AARCH64_NEOVERSE_512TVB_ENV: &str = "LORE_AARCH64_NEOVERSE_512TVB";

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

    println!("cargo:rerun-if-env-changed={AARCH64_NEOVERSE_512TVB_ENV}");

    if platform == "linux" && arch == "aarch64" && env_flag_enabled(AARCH64_NEOVERSE_512TVB_ENV) {
        cc_builder.flag("-mcpu=neoverse-512tvb");
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

fn env_flag_enabled(name: &str) -> bool {
    env::var(name).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}
