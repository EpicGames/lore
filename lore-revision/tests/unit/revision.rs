// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::util::path::RelativePath;

mod diff;
mod last_merged;
mod resolve;
mod sync;
mod sync_hold;

use lore_revision::revision::*;

fn path(text: &str) -> RelativePath {
    RelativePath::new_from_initial_path(text).expect("valid path")
}

#[test]
fn is_below_accepts_only_paths_inside_the_mount() {
    assert!(is_below(&path("shared"), &path("shared/a.txt")));
    assert!(is_below(&path("shared"), &path("shared/deep/a.txt")));
    assert!(!is_below(&path("shared"), &path("shared")));
    assert!(!is_below(&path("shared"), &path("shared-other/a.txt")));
    assert!(!is_below(&path("shared"), &path("root.txt")));
}

#[test]
fn push_unique_path_holds_one_entry_per_path() {
    let mut paths = Vec::new();
    push_unique_path(&mut paths, &path("shared"));
    push_unique_path(&mut paths, &path("shared"));
    push_unique_path(&mut paths, &path("other"));
    assert_eq!(paths.len(), 2);
}
