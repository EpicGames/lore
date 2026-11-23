// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::commit::CommitError;
use lore_revision::commit::CommittedLink;
use lore_revision::commit::UncommittedPin;
use lore_revision::commit::refuse_link_commits;
use lore_revision::commit::refuse_scoped_link_commit;
use lore_revision::commit::reject_unmovable_link_mounts;
use lore_revision::commit::repins;
use lore_revision::link::LinkFlags;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::state::LinkReference;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;

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

fn committed(repository: u8, signature: u8, made: bool) -> CommittedLink {
    CommittedLink {
        repository: RepositoryId::from([repository; 16]),
        branch: BranchId::from([1; 16]),
        signature: Hash::from([signature; 32]),
        made,
    }
}

/// Every mount of a repository takes the revision a commit made through one of them, whatever
/// order the walk committed them in, and the revision its mounts kept where they kept one. Mounts
/// that kept different revisions stay where they are.
#[test]
fn a_repository_takes_the_revision_its_commit_made() {
    let taken = |links: Vec<CommittedLink>| -> Vec<(RepositoryId, Hash)> {
        repins(links)
            .iter()
            .map(|link| (link.repository, link.signature))
            .collect()
    };
    let (one, two) = (RepositoryId::from([1; 16]), RepositoryId::from([2; 16]));

    assert_eq!(
        taken(vec![
            committed(1, 1, false),
            committed(1, 2, true),
            committed(2, 3, false),
            committed(2, 3, false),
        ]),
        [(one, Hash::from([2; 32])), (two, Hash::from([3; 32]))]
    );
    assert_eq!(
        taken(vec![committed(1, 1, false), committed(1, 2, false)]),
        []
    );
}

/// A commit through a staged link node refuses where the mounts of its repository carry different
/// flags.
#[tokio::test]
async fn a_commit_refuses_a_staged_repository_whose_mounts_carry_different_flags() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let (state, linked, _, b) =
                in_memory::linking_twice(&repository, Hash::from_u64(1)).await;
            let row = state
                .link_find(repository.clone(), linked, b)
                .await
                .expect("finding the mount's row");
            state
                .link_replace(
                    repository.clone(),
                    LinkReference {
                        flags: LinkFlags::DisableAutoFollow.bits(),
                        ..row
                    },
                )
                .await
                .expect("changing one mount's flags");

            let refused = reject_unmovable_link_mounts(&repository, &state)
                .await
                .expect_err("the mounts carry different flags");

            assert!(refused.is_link_pins_diverge(), "{refused:?}");
        })
        .await;
}
