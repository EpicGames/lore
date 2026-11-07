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
