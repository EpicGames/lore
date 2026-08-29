// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

fn main() {
    if env::var("CARGO_FEATURE_SWFS").is_ok() {
        let swfs_lib_dir =
            PathBuf::from_str(&env::var("SWFS_LIB_DIR").expect("SWFS_LIB_DIR not set"))
                .expect("SWFS_LIB_DIR not a path");

        println!("cargo:rerun-if-changed={}/swfs.lib", swfs_lib_dir.display());
        println!("cargo:rerun-if-env-changed=SWFS_LIB_DIR");

        let platform = env::var("CARGO_CFG_TARGET_OS").expect("No target OS set");
        let profile = env::var("PROFILE").expect("No profile set");

        match platform.as_str() {
            "macos" => {
                panic!("SWFS not supported on macos");
            }
            "linux" => {
                panic!("SWFS not supported on linux");
            }
            "windows" => {
                println!("cargo:rustc-link-search={}", swfs_lib_dir.display());
                println!("cargo:rustc-link-lib=static=swfs");
                if profile == "debug" {
                    println!("cargo:rustc-link-lib=msvcrtd");
                } else {
                    println!("cargo:rustc-link-lib=msvcrt");
                }
            }
            _ => {
                panic!("Unknown platform");
            }
        }
    }
}
