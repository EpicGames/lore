// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;

use lore_base::types::Hash;
use lore_revision::commit::CommitError;
use lore_revision::commit::UncommittedPin;
use lore_revision::commit::refuse_link_commits;
use lore_revision::commit::refuse_scoped_link_commit;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::util::path::RelativePath;

fn pin(mount_path: &str, pin: u8, branch: u8) -> UncommittedPin {
    UncommittedPin {
        repository: RepositoryId::from([1; 16]),
        branch: BranchId::from([branch; 16]),
        pin: Hash::from([pin; 32]),
        mount_path: path(mount_path),
    }
}

fn path(text: &str) -> RelativePath {
    RelativePath::new_from_initial_path(text).expect("valid path")
}

fn messages(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(path, message)| ((*path).to_owned(), (*message).to_owned()))
        .collect()
}

/// Links to one repository on one branch commit one revision once where only a merge staged it
/// and they take the same per-link messages, at them and below them. A link on another branch
/// commits apart.
#[test]
fn links_sharing_a_merged_revision_commit_it_once() {
    let none = HashSet::new();
    let shared = [pin("vendor/b", 1, 1), pin("vendor/c", 1, 1)];
    for alike in [
        messages(&[]),
        messages(&[("vendor/b", "one"), ("vendor/c", "one")]),
        messages(&[("vendor/b/deps/d", "two"), ("vendor/c/deps/d", "two")]),
        messages(&[("vendor/a", "own"), ("vendor/bb", "own")]),
    ] {
        refuse_link_commits(&shared, &none, &alike).expect("the links share one commit");
    }
    refuse_link_commits(
        &[pin("vendor/b", 1, 1), pin("vendor/c", 2, 2)],
        &none,
        &messages(&[]),
    )
    .expect("links on two branches commit apart");
}

/// Links to one repository on one branch are refused where they pin two revisions, pin one that
/// holds a file staged from the working tree, or are given different per-link messages.
#[test]
fn links_to_one_repository_that_would_lose_work_are_refused() {
    let none = HashSet::new();
    let shared = [pin("vendor/b", 1, 1), pin("vendor/c", 1, 1)];
    assert!(matches!(
        refuse_link_commits(
            &[pin("vendor/b", 1, 1), pin("vendor/c", 2, 1)],
            &none,
            &messages(&[])
        ),
        Err(CommitError::NotSupported(_))
    ));
    assert!(matches!(
        refuse_link_commits(
            &shared,
            &HashSet::from([Hash::from([1; 32])]),
            &messages(&[])
        ),
        Err(CommitError::NotSupported(_))
    ));
    for diverging in [
        messages(&[("vendor/b", "one"), ("vendor/c", "other")]),
        messages(&[("vendor/c", "one")]),
        messages(&[("vendor/c/deps/d", "two")]),
    ] {
        assert!(matches!(
            refuse_link_commits(&shared, &none, &diverging),
            Err(CommitError::InvalidArguments(_))
        ));
    }
}

/// A commit of one link alone, and the links below it, is refused where a link outside it to one
/// of their repositories on the same branch pins an uncommitted revision too.
#[test]
fn a_link_committed_alone_is_refused_while_another_link_pins_its_repository() {
    let pins = [pin("libs/d", 1, 1), pin("libs/d-again", 1, 1)];
    assert!(matches!(
        refuse_scoped_link_commit(&pins, &path("libs/d")),
        Err(CommitError::NotSupported(_))
    ));
    assert!(matches!(
        refuse_scoped_link_commit(
            &[pin("vendor/b/deps/d", 1, 1), pin("vendor/c", 1, 1)],
            &path("vendor/b")
        ),
        Err(CommitError::NotSupported(_))
    ));
    for (pins, scope) in [
        (
            vec![pin("libs/d", 1, 1), pin("libs/d/deps/d", 1, 1)],
            "libs/d",
        ),
        (
            vec![pin("libs/d", 1, 1), pin("libs/d-again", 1, 2)],
            "libs/d",
        ),
        (vec![pin("libs/d", 1, 1)], "libs/d"),
    ] {
        refuse_scoped_link_commit(&pins, &path(scope))
            .expect("no link outside the scope shares it");
    }
}
