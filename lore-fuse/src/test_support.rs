// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Test helpers: FUSE availability probing and the `skip_without_fuse!` gate.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Environment variable that forces every FUSE-gated test to skip.
pub const SKIP_ENV_VAR: &str = "LORE_TEST_SKIP_FUSE";

/// The test filter that selects every test that mounts FUSE: each is declared in a module named
/// `needs_fuse`.
pub const NEEDS_FUSE_FILTER: &str = "needs_fuse::";

/// The packages that hold the tests that mount FUSE. A run of the FUSE tests names each, with
/// [`NEEDS_FUSE_FILTER`].
pub const FUSE_TEST_PACKAGES: [&str; 3] = ["lore-fuse", "lore-integration-tests", "lore-revision"];

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

/// Whether the calling test, of `package`, can mount: [`fuse_availability`] reports FUSE usable.
///
/// # Panics
///
/// When [`NEEDS_FUSE_FILTER`] doesn't select the calling test, or [`FUSE_TEST_PACKAGES`] doesn't
/// name `package`, whatever FUSE offers here. libtest names the thread it runs a test on after the
/// test's path.
pub fn fuse_test_can_mount(package: &str) -> bool {
    let thread = std::thread::current();
    let test = thread.name().unwrap_or_default();
    assert!(
        test.contains(NEEDS_FUSE_FILTER),
        "{test} mounts FUSE, so it must be declared in a `needs_fuse` module: the FUSE CI jobs run \
         only the tests `{NEEDS_FUSE_FILTER}` selects"
    );
    assert!(
        FUSE_TEST_PACKAGES.contains(&package),
        "{test} mounts FUSE, so its package {package} must be one of {FUSE_TEST_PACKAGES:?}: the \
         FUSE CI jobs run only those packages' tests"
    );
    fuse_availability().usable()
}

/// Skips the calling test when FUSE is unavailable on this machine, or when
/// `LORE_TEST_SKIP_FUSE` is set. Panics in a test declared outside a `needs_fuse` module, or in a
/// package outside [`FUSE_TEST_PACKAGES`](crate::test_support::FUSE_TEST_PACKAGES), as
/// [`fuse_test_can_mount`](crate::test_support::fuse_test_can_mount) describes.
#[macro_export]
macro_rules! skip_without_fuse {
    () => {
        if !$crate::test_support::fuse_test_can_mount(env!("CARGO_PKG_NAME")) {
            eprintln!("skipped: FUSE unavailable");
            return;
        }
    };
}
