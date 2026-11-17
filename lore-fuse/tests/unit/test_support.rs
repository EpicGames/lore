// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

use lore_fuse::test_support::*;

#[test]
fn availability_reports_missing_device_for_nonexistent_path() {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let availability = probe_with(Path::new("/nonexistent"), &path_env);

    assert!(!availability.device);
}

#[test]
#[should_panic(expected = "must be declared in a `needs_fuse` module")]
fn the_gate_refuses_a_test_the_needs_fuse_filter_does_not_select() {
    lore_fuse::skip_without_fuse!();
}

mod needs_fuse {
    #[test]
    fn the_gate_admits_a_test_the_needs_fuse_filter_selects() {
        let _ = lore_fuse::test_support::fuse_test_can_mount("lore-fuse");
    }

    #[test]
    #[should_panic(expected = "must be one of")]
    fn the_gate_refuses_a_test_in_a_package_the_fuse_runs_do_not_name() {
        let _ = lore_fuse::test_support::fuse_test_can_mount("lore-cli");
    }
}
