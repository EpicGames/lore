// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

fn default_swfs_lib_dir(platform: &str) -> String {
    match platform {
        "macos" => "macos".to_owned(),
        "linux" => "linux".to_owned(),
        "windows" => "win64".to_owned(),
        _ => {
            panic!("Unknown platform");
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=SWFS_LIB_DIR");

    let platform = env::var("CARGO_CFG_TARGET_OS").expect("No target OS set");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("No target arch set");

    // The default is anchored to this package rather than left relative: rustc resolves a relative
    // link-search path against its own working directory, which is the root of whichever Cargo
    // workspace is building this crate, not necessarily the one holding it.
    let swfs_lib_dir = match env::var("SWFS_LIB_DIR") {
        Ok(dir) => PathBuf::from_str(&dir).expect("SWFS_LIB_DIR not a path"),
        Err(_) => PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("No manifest dir set"))
            .join("native")
            .join("swfs")
            .join(default_swfs_lib_dir(&platform)),
    };

    match platform.as_str() {
        "macos" | "linux" => {
            // SWFS is not available on this platform yet, so there is nothing to link. Link the
            // library here once it is.
        }
        "windows" => {
            // Only the x86-64 library exists.
            // The C runtime is not linked here: the binary's own `+crt-static` supplies it, and
            // linking `msvcrt` would make the binary depend on the dynamic runtime DLLs.
            if arch == "x86_64" {
                // Cargo sets `PROFILE` to "debug" for the `dev` profile and every profile that
                // inherits from it, and to "release" for every other profile.
                let swfs_lib = match env::var("PROFILE").expect("No profile set").as_str() {
                    "debug" => "swfs_d",
                    _ => "swfs",
                };
                println!(
                    "cargo:rerun-if-changed={}/{swfs_lib}.lib",
                    swfs_lib_dir.display()
                );

                println!("cargo:rustc-link-search={}", swfs_lib_dir.display());
                println!("cargo:rustc-link-lib=static={swfs_lib}");
            }
        }
        _ => {
            panic!("Unknown platform");
        }
    }
}
