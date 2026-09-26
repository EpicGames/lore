// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::env;

/// Sets `lore_unoptimized` for builds without optimization, which `args::invoke_args` reads.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(lore_unoptimized)");
    if env::var("OPT_LEVEL").is_ok_and(|level| level == "0") {
        println!("cargo:rustc-cfg=lore_unoptimized");
    }
}
