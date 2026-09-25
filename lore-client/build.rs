// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_macros)]
use std::error::Error;

include!("../build-helper.rs");

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");

    // Compile metadata into lore.exe for Windows
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        // Hack around EXE and DLL having the same file name for PDB file
        let profile_dir = profile_dir();
        println!("cargo:rustc-link-arg-bins=/PDB:{profile_dir}\\lore.exe.pdb");

        // Create WindowsResource with defaults from the crate
        let mut winres = winresource::WindowsResource::new();

        // Set language to en-US
        winres.set_language(0x0409);

        winres.compile().unwrap();
    }

    Ok(())
}
