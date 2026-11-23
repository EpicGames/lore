// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_revision::fs::swfs::paths::RelativePathKey;
use lore_revision::fs::swfs::state::ListedDeletions;
use lore_revision::fs::swfs::state::ThawRemovals;
use lore_revision::fs::swfs::state::name_committed_removals;
use lore_revision::util::path::RelativePath;

fn path(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).unwrap()
}

/// The deletions SWFS lists at `paths`, spelled as given.
fn listed(paths: &[&str]) -> ListedDeletions {
    paths
        .iter()
        .map(|listed| (listed.to_lowercase(), path(listed)))
        .collect()
}

/// What a commit that removed the files `removed` and the directories `removed_trees` whole names
/// where SWFS lists `deletions` and froze something at each of `frozen`: the paths named, sorted,
/// and the removals noted.
fn named(
    deletions: &[&str],
    removed: &[&str],
    removed_trees: &[&str],
    frozen: &[&str],
) -> (Vec<String>, ThawRemovals) {
    let mut removals = ThawRemovals::default();
    let trees: Vec<RelativePath> = removed_trees.iter().map(|tree| path(tree)).collect();
    name_committed_removals(
        &mut removals,
        &listed(deletions),
        removed.iter().map(|removed| path(removed)),
        &trees,
        |at| frozen.contains(&at.as_lowercase_str()),
    );
    let mut named: Vec<String> = removals
        .committed
        .iter()
        .map(|named| named.0.as_str().to_owned())
        .collect();
    named.sort();
    (named, removals)
}

fn holds(set: &std::collections::HashSet<RelativePathKey>, at: &str) -> bool {
    set.contains(&RelativePathKey(path(at)))
}

#[test]
fn a_removed_file_swfs_lists_as_deleted_is_named() {
    let (named, removals) = named(&["dir/a.txt"], &["dir/a.txt"], &[], &[]);
    assert_eq!(named, ["dir/a.txt"]);
    assert!(holds(&removals.committed, "dir/a.txt"));
}

/// An entry naming a path SWFS does not list as deleted makes an empty file of it.
#[test]
fn a_removed_file_swfs_does_not_list_is_not_named() {
    let (named, removals) = named(&["other.txt"], &["dir/a.txt"], &[], &[]);
    assert!(named.is_empty());
    assert!(removals.committed.is_empty());
    assert!(removals.held_deleted.is_empty());
}

/// The user deleted the directory whole, so SWFS lists the directory.
#[test]
fn a_removed_file_is_named_by_the_nearest_listed_directory_above_it() {
    let (named, _) = named(&["dir", "dir/sub"], &["dir/sub/deep/a.txt"], &[], &[]);
    assert_eq!(named, ["dir/sub"]);
}

#[test]
fn files_removed_below_one_listed_directory_name_it_once() {
    let (named, _) = named(&["dir"], &["dir/a.txt", "dir/b.txt"], &[], &[]);
    assert_eq!(named, ["dir"]);
}

#[test]
fn a_directory_deleted_whole_names_every_deletion_listed_at_or_below_it() {
    let (named, removals) = named(
        &["dir", "dir/a.txt", "dir/sub", "dir/sub/b.txt", "kept/c.txt"],
        &[],
        &["dir"],
        &[],
    );
    assert_eq!(named, ["dir", "dir/a.txt", "dir/sub", "dir/sub/b.txt"]);
    assert!(!holds(&removals.committed, "kept/c.txt"));
}

/// `dir2` only starts with the deleted directory's name.
#[test]
fn a_directory_deleted_whole_names_nothing_in_a_sibling_sharing_its_prefix() {
    let (named, _) = named(&["dir2", "dir2/x.txt", "dir/a.txt"], &[], &["dir"], &[]);
    assert_eq!(named, ["dir/a.txt"]);
}

#[test]
fn a_deleted_directory_nested_in_a_kept_one_names_only_what_lies_below_it() {
    let (named, _) = named(
        &["parent/dir", "parent/dir/a.txt", "parent/other.txt"],
        &[],
        &["parent/dir"],
        &[],
    );
    assert_eq!(named, ["parent/dir", "parent/dir/a.txt"]);
}

/// SWFS matches an entry to the deletion it lists, so the entry is spelled as SWFS lists it,
/// whichever case the commit spelled the path in.
#[test]
fn what_is_named_is_spelled_as_swfs_lists_it() {
    let (named, removals) = named(
        &["Dir", "Dir/Sub/B.txt", "Top.TXT"],
        &["top.txt"],
        &["dir"],
        &[],
    );
    assert_eq!(named, ["Dir", "Dir/Sub/B.txt", "Top.TXT"]);
    assert!(holds(&removals.committed, "Dir/Sub/B.txt"));
    assert!(!holds(&removals.committed, "dir/sub/b.txt"));
}

/// What SWFS froze at a path the commit removed is the user's, made there since.
#[test]
fn a_path_swfs_froze_something_at_is_held_and_not_named() {
    let (named, removals) = named(
        &["gone.txt", "dir", "dir/a.txt"],
        &["gone.txt"],
        &["dir"],
        &["gone.txt", "dir/a.txt"],
    );
    assert_eq!(named, ["dir"]);
    assert!(holds(&removals.held_deleted, "gone.txt"));
    assert!(holds(&removals.held_deleted, "dir/a.txt"));
    assert!(!holds(&removals.committed, "gone.txt"));
    assert!(!holds(&removals.committed, "dir/a.txt"));
}

/// The directory a removed file is named by is a path like any other named.
#[test]
fn the_listed_directory_above_a_removed_file_is_held_where_swfs_froze_something_at_it() {
    let (named, removals) = named(&["dir"], &["dir/a.txt"], &[], &["dir"]);
    assert!(named.is_empty());
    assert!(holds(&removals.held_deleted, "dir"));
}

#[test]
fn nothing_is_named_where_swfs_lists_no_deletion() {
    let (named, removals) = named(&[], &["a.txt", "dir/b.txt"], &["dir"], &[]);
    assert!(named.is_empty());
    assert!(removals.committed.is_empty());
}
