// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::fs::swfs::paths::at_or_below;
use lore_revision::fs::swfs::paths::rest_below;
use lore_revision::fs::swfs::paths::strictly_below;
use lore_revision::util::path::RelativePath;

fn path(text: &str) -> RelativePath {
    RelativePath::new_from_initial_path(text).expect("a relative path")
}

#[test]
fn the_rest_below_an_ancestor_is_what_follows_its_separator() {
    assert_eq!(rest_below("deps/lib/a.txt", "deps/lib"), Some("a.txt"));
    assert_eq!(rest_below("deps/lib", "deps/lib"), Some(""));
}

#[test]
fn a_path_is_compared_with_its_ancestor_in_any_spelling() {
    assert_eq!(rest_below("Deps/LIB/a.txt", "deps/lib"), Some("a.txt"));
    assert!(at_or_below(&path("DEPS/lib"), &path("deps/Lib")));
}

#[test]
fn a_name_that_only_starts_with_the_ancestor_lies_outside_it() {
    assert_eq!(rest_below("deps/library/a.txt", "deps/lib"), None);
    assert_eq!(rest_below("deps", "deps/lib"), None);
}

#[test]
fn the_root_is_above_every_path() {
    assert_eq!(rest_below("deps/lib", ""), Some("deps/lib"));
    assert!(at_or_below(&path("deps"), &RelativePath::new()));
    assert!(strictly_below(&path("deps"), &RelativePath::new()));
    assert!(!strictly_below(&RelativePath::new(), &RelativePath::new()));
}

#[test]
fn a_path_is_at_but_not_strictly_below_itself() {
    assert!(at_or_below(&path("deps/lib"), &path("deps/lib")));
    assert!(!strictly_below(&path("deps/lib"), &path("deps/lib")));
    assert!(strictly_below(&path("deps/lib/a.txt"), &path("deps/lib")));
}

#[test]
fn a_name_of_several_bytes_per_character_is_compared_without_splitting_one() {
    assert_eq!(rest_below("d\u{e9}ps/a.txt", "d\u{e9}ps"), Some("a.txt"));
    assert_eq!(rest_below("d\u{e9}ps/a.txt", "d\u{e9}"), None);
    // `de` would end inside the path's `é`, which the byte comparison turns down rather than
    // slicing the path there.
    assert_eq!(rest_below("d\u{e9}/a.txt", "de"), None);
}
