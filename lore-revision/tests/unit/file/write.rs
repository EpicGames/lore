// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(not(target_os = "windows"))]
use std::path::Path;

#[cfg(not(target_os = "windows"))]
use lore_revision::file::write::*;

#[test]
#[cfg(not(target_os = "windows"))]
fn destination_inside_repo_without_token_is_write_required() {
    let result = check_destination_access(Path::new("/a/b"), "/a/b/payload.bin", None);
    assert!(matches!(result, Err(WriteError::WriteRequired(_))));
}

#[test]
#[cfg(not(target_os = "windows"))]
fn destination_outside_repo_without_token_is_ok() {
    let result = check_destination_access(Path::new("/a/b"), "/c/payload.bin", None);
    assert!(result.is_ok());
}
