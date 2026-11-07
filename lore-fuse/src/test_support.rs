// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Test helpers: FUSE availability probing and the `skip_without_fuse!` gate.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Environment variable that forces every FUSE-gated test to skip.
pub const SKIP_ENV_VAR: &str = "LORE_TEST_SKIP_FUSE";

/// What this machine offers for running a real FUSE mount.
pub struct FuseAvailability {
    pub device: bool,
    pub fusermount: bool,
    pub skipped_by_env: bool,
}

impl FuseAvailability {
    /// Whether a test can go ahead and mount.
    pub fn usable(&self) -> bool {
        self.device && self.fusermount && !self.skipped_by_env
    }
}

/// Probes this process's environment for `/dev/fuse` and `fusermount3`, and checks the
/// skip environment variable.
pub fn fuse_availability() -> FuseAvailability {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let mut availability = probe_with(Path::new("/dev/fuse"), &path_env);
    availability.skipped_by_env = std::env::var_os(SKIP_ENV_VAR).is_some();
    availability
}

/// Probes a specific device path and `PATH`-like variable, for tests that need to control
/// the inputs rather than the real environment.
pub fn probe_with(device: &Path, path_env: &OsStr) -> FuseAvailability {
    let device_ok = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .is_ok();
    let fusermount_ok =
        std::env::split_paths(path_env).any(|dir| is_executable(&dir.join("fusermount3")));

    FuseAvailability {
        device: device_ok,
        fusermount: fusermount_ok,
        skipped_by_env: false,
    }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Skips the calling test when FUSE is unavailable on this machine, or when
/// `LORE_TEST_SKIP_FUSE` is set.
#[macro_export]
macro_rules! skip_without_fuse {
    () => {
        if !$crate::test_support::fuse_availability().usable() {
            eprintln!("skipped: FUSE unavailable");
            return;
        }
    };
}
