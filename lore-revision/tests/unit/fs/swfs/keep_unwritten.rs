// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What an SWFS operation that moved the instance to another revision keeps of the changes it did
//! not write (ADR-00034): only an operation that says it leaves changes unwritten diffs the two
//! revisions for them.
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
// The fixtures write and remove the files a mount holds directly, as a user would.
#![allow(clippy::disallowed_methods)]
use std::sync::Arc;

use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::swfs::api_interface::SwfsInterface;
use lore_revision::fs::swfs::filesystem::SwfsOperation;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::instance::store_current_anchor;
use lore_revision::instance::store_current_anchor_branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use super::listing_fixture::Held;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::stored;
use super::listing_fixture::writable_test_repository;

/// An operation begun on a mount serving a revision that holds `kept.txt` as `old`, whose stored
/// current revision then holds it as `new` beside an added `added.txt`, as a sync to that revision
/// stores it. The mount holds `kept.txt` as `stale`, so a write of the old content shows. Returns
/// the operation, the mount, and the repository with the revision the operation moved to.
async fn moved_to_another_revision() -> (SwfsOperation, TempDir, Arc<RepositoryContext>, Arc<State>)
{
    let mount = TempDir::new("swfs-keep-unwritten");
    let repository = writable_test_repository().await;
    let before = revision_state(&repository, &[("kept.txt", Held::File(b"old"))]).await;
    let after = revision_state(
        &repository,
        &[
            ("kept.txt", Held::File(b"new")),
            ("added.txt", Held::File(b"added")),
        ],
    )
    .await;
    let after_revision = stored(&repository, &after).await;
    store_current_anchor(&repository, after_revision)
        .await
        .expect("storing the current anchor");
    store_current_anchor_branch(&repository, BranchId::from([5; 16]))
        .await
        .expect("storing the current branch");
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    std::fs::write(mount_root.join("kept.txt"), b"stale").expect("holding kept.txt on the mount");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository.clone(), Some(before), mount_path).await;
    resources.modified().set_trusted(true);
    let operation = SwfsOperation::for_test(SwfsInterface::unmounted(resources), &mount_root);
    (operation, mount, repository, after)
}

fn mounted(mount: &TempDir, path: &str) -> Option<Vec<u8>> {
    std::fs::read(mount.path().join("mount").join(path)).ok()
}

#[tokio::test]
async fn an_operation_that_moves_the_revision_keeps_nothing_it_was_not_told_to() {
    in_context(async {
        let (operation, mount, _repository, _after) = moved_to_another_revision().await;

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(operation.revision_diffs_for_test(), 0);
        assert_eq!(mounted(&mount, "kept.txt"), Some(b"stale".to_vec()));
        assert!(!operation.hides_for_test(&node("added.txt")));
    })
    .await;
}

#[tokio::test]
async fn an_operation_that_writes_every_change_does_not_diff_the_revisions() {
    in_context(async {
        let (operation, mount, repository, after) = moved_to_another_revision().await;
        for path in ["kept.txt", "added.txt"] {
            let written = after
                .find_node(repository.clone(), path)
                .await
                .expect("finding the change");
            operation
                .write_node(repository.clone(), &written, &node(path))
                .await
                .expect("writing the change");
        }

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(operation.revision_diffs_for_test(), 0);
        assert_eq!(mounted(&mount, "kept.txt"), Some(b"stale".to_vec()));
    })
    .await;
}

#[tokio::test]
async fn an_operation_that_leaves_changes_unwritten_keeps_what_the_working_tree_showed() {
    in_context(async {
        let (operation, mount, _repository, _after) = moved_to_another_revision().await;
        operation.leaves_changes_unwritten();

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(operation.revision_diffs_for_test(), 1);
        assert_eq!(mounted(&mount, "kept.txt"), Some(b"old".to_vec()));
        assert!(operation.hides_for_test(&node("added.txt")));
    })
    .await;
}

#[tokio::test]
async fn a_failed_operation_keeps_what_it_left_unwritten() {
    in_context(async {
        let (operation, mount, _repository, _after) = moved_to_another_revision().await;

        // Fails at the thaw, which a mount that was never frozen cannot take, after realizing.
        let _ = operation.finalize(true, false).await;

        assert_eq!(operation.revision_diffs_for_test(), 1);
        assert_eq!(mounted(&mount, "kept.txt"), Some(b"old".to_vec()));
        assert!(operation.hides_for_test(&node("added.txt")));
    })
    .await;
}
