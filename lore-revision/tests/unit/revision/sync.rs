// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // Test fixtures writing the copies in a temporary directory.

use std::path::Path;
use std::sync::Arc;

use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::repository::MERGE_ARTIFACT_SUFFIXES;
use lore_revision::repository::MINE_SUFFIX;
use lore_revision::revision::sync::*;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::TestFilesystemProvider;

/// An operation rooted at `root`, which is what the helpers name their paths against.
async fn os_operation(root: &Path) -> Arc<InstanceOperationImpl> {
    FilesystemProvider::begin_operation(&OsFilesystem::new(root))
        .await
        .expect("beginning an operation over the OS filesystem")
}

fn relative(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("relative path")
}

/// Every copy on its own, so a helper that reads one suffix and stops is not mistaken for
/// one that reads all three.
#[tokio::test]
async fn a_copy_under_any_suffix_is_found() {
    for suffix in MERGE_ARTIFACT_SUFFIXES {
        let dir = TempDir::new("lore-merge-artifact-");
        let operation = os_operation(dir.path()).await;
        std::fs::write(dir.path().join(format!("file.txt{suffix}")), b"side").expect("write copy");

        assert!(
            exist_merge_artifacts(&operation, &relative("file.txt")).await,
            "the copy under {suffix} was not found"
        );
    }
}

#[tokio::test]
async fn a_file_no_merge_left_copies_beside_reports_none() {
    let dir = TempDir::new("lore-merge-artifact-");
    let operation = os_operation(dir.path()).await;
    std::fs::write(dir.path().join("file.txt"), b"merged").expect("write file");

    assert!(!exist_merge_artifacts(&operation, &relative("file.txt")).await);
}

/// The file the copies belong to is not one of them, and stays.
#[tokio::test]
async fn removing_takes_every_copy_and_leaves_the_file() {
    let dir = TempDir::new("lore-merge-artifact-");
    let operation = os_operation(dir.path()).await;
    std::fs::write(dir.path().join("file.txt"), b"merged").expect("write file");
    for suffix in MERGE_ARTIFACT_SUFFIXES {
        std::fs::write(dir.path().join(format!("file.txt{suffix}")), b"side").expect("write copy");
    }

    unlink_merge_artifacts(&operation, &relative("file.txt")).await;

    assert!(!exist_merge_artifacts(&operation, &relative("file.txt")).await);
    for suffix in MERGE_ARTIFACT_SUFFIXES {
        assert!(
            !dir.path().join(format!("file.txt{suffix}")).exists(),
            "the copy under {suffix} was left behind"
        );
    }
    assert!(
        dir.path().join("file.txt").exists(),
        "the file the copies belong to was removed"
    );
}

/// Read from the working tree rather than from the tree the path is tracked in: a
/// provider serving tracked content virtually holds no node for a sidecar, so one asked
/// through [`InstanceOperation::file_info`] answers that every copy is absent.
#[tokio::test]
async fn a_copy_is_looked_for_outside_the_tracked_tree() {
    let provider = Arc::new(TestFilesystemProvider::holding_every_path());
    let operation = provider
        .begin_operation()
        .await
        .expect("beginning an operation over the test provider");

    assert!(exist_merge_artifacts(&operation, &relative("file.txt")).await);
    assert_eq!(
        0,
        provider.file_infos(),
        "the copies were looked up through the tracked tree"
    );
}

/// A path under a directory, so the suffix lands on the name rather than anywhere in the
/// path it is reached by.
#[tokio::test]
async fn a_copy_beside_a_nested_file_is_found_and_removed() {
    let dir = TempDir::new("lore-merge-artifact-");
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir_all(dir.path().join("sub")).expect("create directory");
    let copy = dir
        .path()
        .join("sub")
        .join(format!("file.txt{MINE_SUFFIX}"));
    std::fs::write(&copy, b"side").expect("write copy");

    let nested = relative("sub/file.txt");
    assert!(exist_merge_artifacts(&operation, &nested).await);

    unlink_merge_artifacts(&operation, &nested).await;

    assert!(
        !copy.exists(),
        "the copy beside a nested file was left behind"
    );
}

/// A filesystem error a sync meets keeps its kind: local modifications and a missing file reach
/// the caller as themselves, not as an internal error.
#[test]
fn a_filesystem_error_keeps_its_kind_through_a_sync() {
    use lore_base::error::FileNotFound;
    use lore_base::error::LocalModifications;
    use lore_revision::fs::filesystem_provider::FsError;

    let modified = SyncError::from(FsError::from(LocalModifications));
    assert!(
        matches!(modified, SyncError::LocalModifications(_)),
        "{modified:?}"
    );

    let missing = SyncError::from(FsError::from(FileNotFound {
        resource: "gone.txt".to_string(),
    }));
    assert!(matches!(missing, SyncError::FileNotFound(_)), "{missing:?}");
}

/// A realize that fails answers its own error only once the task run beside it has ended, so
/// the repository that task holds is released by the time the sync answers.
#[tokio::test]
async fn a_failed_realize_waits_for_the_task_beside_it() {
    use lore_base::error::LocalModifications;
    use lore_base::lore_spawn;
    use lore_revision::fs::filesystem_provider::FsError;

    let repository = Arc::new(());
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let held = repository.clone();
    let beside = lore_spawn!(async move {
        let _ = released.await;
        drop(held);
    });

    let mut realizing = std::pin::pin!(realize_joining(beside, async {
        Err(SyncError::from(FsError::from(LocalModifications)))
    }));
    assert!(
        futures::poll!(realizing.as_mut()).is_pending(),
        "the failed realize answered while the task beside it still ran"
    );

    release
        .send(())
        .expect("the task beside the realize is waiting");
    let realized = realizing.await;
    assert!(
        matches!(realized, Err(SyncError::LocalModifications(_))),
        "{realized:?}"
    );
    assert_eq!(
        1,
        Arc::strong_count(&repository),
        "the task beside the realize still holds the repository"
    );
}

/// A sync whose filesystem operation fails to begin starts no cache task, so nothing is left
/// holding the repository once the sync answers. The test runtime runs one thread, so a task
/// spawned and never awaited would still hold it at the check.
#[tokio::test]
async fn a_sync_that_cannot_begin_its_operation_leaves_the_repository_unheld() {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;

    use crate::fs::filesystem_provider::test_store_create;
    use crate::helper::RepositoryContextCreationArgsExt;
    use crate::helper::default_repository_creation_args;
    use crate::helper::test_commit_tree;
    use crate::helper::test_file_write;
    use crate::helper::test_repository_create;

    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                immutable.clone(),
                mutable.clone(),
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            test_file_write(&instance.path.join("first.txt"), b"first");
            let first = test_commit_tree(&instance, "First").await.revision();
            test_file_write(&instance.path.join("second.txt"), b"second");
            test_commit_tree(&instance, "Second").await;

            let filesystem = Arc::new(TestFilesystemProvider::failing_begin());
            let repository = Arc::new(
                RepositoryContext::new(
                    default_repository_creation_args(immutable, mutable)
                        .with_path(&instance.path)
                        .with_id(instance.repository.id)
                        .with_instance_id(instance.repository.instance_id)
                        .with_filesystem_provider(filesystem.clone()),
                )
                .with_write_token(instance.write_token.share()),
            );

            let synced = sync_boxed(
                repository.clone(),
                &instance.write_token,
                SyncOptions {
                    revision: Some(first.to_string()),
                    ..Default::default()
                },
            )
            .await;

            assert!(synced.is_err(), "{synced:?}");
            assert_eq!(
                1,
                filesystem.begins(),
                "the sync never reached its operation"
            );
            assert_eq!(
                1,
                Arc::strong_count(&repository),
                "a task the sync started still holds the repository"
            );
        })
        .await;
}

/// The walk a sync runs between the remote's latest revision of the branch and the local one.
mod branch_point {
    use std::time::Duration;

    use lore_base::error::NotAuthorized;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchId;
    use lore_base::types::Hash;
    use lore_revision::instance::load_current_anchor_boxed;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_transport::Connection;
    use lore_transport::ProtocolError;

    use super::*;
    use crate::fs::filesystem_provider::test_store_create;
    use crate::helper::RepositoryContextCreationArgsExt;
    use crate::helper::default_repository_creation_args;
    use crate::helper::test_commit_tree;
    use crate::helper::test_file_write;
    use crate::helper::test_repository_create;
    use crate::paged_remote::Anchor;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::remote_revision;

    /// The result of a sync of an instance whose branch holds a local commit the remote lacks,
    /// from the remote that `remote` builds for the branch. The sync is bounded by five seconds,
    /// so that a walk that loops fails the test.
    async fn sync_from(remote: impl FnOnce(BranchId) -> PagedRemote) -> Result<(), SyncError> {
        let (immutable, mutable, execution) =
            test_store_create().await.expect("creating the stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let instance = test_repository_create(
                    immutable.clone(),
                    mutable.clone(),
                    RepositoryId::from(uuid::Uuid::now_v7()),
                )
                .await;
                test_file_write(&instance.path.join("local.txt"), b"local");
                test_commit_tree(&instance, "Local").await;
                let (_, branch) = load_current_anchor_boxed(&instance.repository)
                    .await
                    .expect("reading the current anchor");
                let connection = Connection::detached_revision(
                    Vec::new(),
                    Ok(()),
                    instance.repository.id,
                    Arc::new(remote(branch)),
                );
                let repository = Arc::new(
                    RepositoryContext::new(
                        default_repository_creation_args(immutable, mutable)
                            .with_path(&instance.path)
                            .with_id(instance.repository.id)
                            .with_instance_id(instance.repository.instance_id)
                            .with_remote(Ok(connection)),
                    )
                    .with_write_token(instance.write_token.share()),
                );
                let synced = sync_boxed(repository, &instance.write_token, SyncOptions::default());
                tokio::time::timeout(Duration::from_secs(5), synced)
                    .await
                    .expect("the sync ends within five seconds")
            })
            .await
    }

    /// A sync whose walk between the remote's latest revision and the local one stops fails with
    /// the walk's error, which keeps its code: `RevisionNotFound` naming the remote's latest where
    /// the remote's page does not move the line, and the remote's `NotAuthorized` where its list
    /// fails.
    #[tokio::test]
    async fn a_sync_whose_walk_stops_fails_with_the_walks_error() {
        let remote_latest = remote_revision(3, Hash::default(), Hash::from([0xA0; 32])).await;
        let top = remote_latest.signature;

        let stopped = sync_from(|branch| {
            PagedRemote::new(branch, top)
                .holding([&remote_latest])
                .page(Anchor::Signature(top), vec![(3, top)], top)
        })
        .await;
        let refused = sync_from(|branch| {
            PagedRemote::new(branch, top)
                .failing(Anchor::Signature(top), ProtocolError::from(NotAuthorized))
        })
        .await;

        assert_eq!(
            (
                stopped
                    .as_ref()
                    .err()
                    .and_then(|error| error.as_revision_not_found())
                    .map(|error| error.revision.clone()),
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_not_authorized()),
            ),
            (Some(top.to_string()), true),
            "{stopped:?}; {refused:?}"
        );
    }
}
