// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::file::stage::created_first;
use lore_revision::util::path::RelativePath;

/// `paths` as the paths a stage takes.
fn paths(paths: &[&str]) -> Vec<RelativePath> {
    paths
        .iter()
        .map(|path| RelativePath::new_from_initial_path(path).expect("a clean relative path"))
        .collect()
}

/// The paths `created_first` creates, in its order.
fn created(antichain: &[&str], alone: &[&str]) -> Vec<String> {
    created_first(&paths(antichain), &paths(alone))
        .iter()
        .map(|created| created.path().to_string())
        .collect()
}

/// A directory staged as itself is created first, shallowest first, with the directories the
/// targets share; one a target covers is left to that target's walk.
#[test]
fn the_directories_staged_as_themselves_are_created_first_unless_a_target_covers_them() {
    assert_eq!(
        created(&["n/a.txt", "n/b.txt", "top.txt"], &["n", "n/sub"]),
        ["n", "n/sub"]
    );
    assert_eq!(created(&["s"], &["s/a", "s/b"]), Vec::<String>::new());
}

/// A directory the targets share, spelled in another case than a directory staged as itself, is
/// created once, as the directory staged as itself spells it.
#[test]
fn a_shared_directory_spelled_otherwise_is_created_once() {
    assert_eq!(created(&["dir/x.txt", "dir/y.txt"], &["Dir"]), ["Dir"]);
}
