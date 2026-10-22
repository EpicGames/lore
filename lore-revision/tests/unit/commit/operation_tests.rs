// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build working-tree state directly; what these test is how the commit reads it.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::commit::*;
use lore_revision::filter::Filter;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::hold;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::layer;
use lore_revision::layer::Layer;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;
use serde::Serialize;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_execution_with;
use crate::fs::filesystem_provider::test_repository;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

/// One commit reads the working tree through the one operation it opens, whatever it
/// commits: a provider that freezes hands out one snapshot, and a second opened partway
/// would fragment some of the files against a tree the rest were never measured against.
///
/// The count is taken from the commit alone, the staging that precedes it opening its own.
#[tokio::test]
async fn one_call_reads_the_working_tree_through_one_operation() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    std::fs::write(dir.path().join("one.txt"), b"one").expect("write file");
    std::fs::write(dir.path().join("two.txt"), b"two").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();

    let begins = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let fixture = counting_repository(&root, immutable_store, mutable_store).await;
            stage_working_tree(&fixture, &root).await;
            fixture.begins.store(0, Ordering::Release);

            commit_staged(&fixture.repository, &fixture.token, test_commit())
                .await
                .expect("Committing");

            fixture.begins.load(Ordering::Acquire)
        }))
        .await
        .expect("Test task failed");

    assert_eq!(
        1, begins,
        "The commit opened an operation beyond the one it reads the working tree through"
    );
}

/// A staged file the working tree no longer holds is reported rather than fragmented: what
/// a commit stores for a file it reads from the working tree, and a path holding nothing
/// has nothing to store.
#[tokio::test]
async fn a_staged_file_the_working_tree_no_longer_holds_is_reported() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    std::fs::write(dir.path().join("gone.txt"), b"content").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();

    let error = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let fixture = counting_repository(&root, immutable_store, mutable_store).await;
            stage_working_tree(&fixture, &root).await;
            std::fs::remove_file(root.join("gone.txt")).expect("remove file");

            commit_staged(&fixture.repository, &fixture.token, test_commit())
                .await
                .expect_err("A commit of a file that is gone reported a revision")
        }))
        .await
        .expect("Test task failed");

    assert!(
        matches!(error, CommitError::FileNotFound { .. }),
        "The missing file was reported as {error} rather than as the file it is"
    );
}

/// The operation a commit read its files through, on a provider holding its state, is handed the
/// revision it stored once that is current, clean of staged and dirty nodes. A dry run leaves the
/// current revision where it was and hands nothing over.
#[tokio::test]
async fn the_operation_is_handed_the_committed_revision_and_nothing_on_a_dry_run() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_reading(dir.path()));

    let (signature, current) = commit_then_dry_run(&dir, filesystem.clone()).await;

    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)],
        "The commit handed over another revision than it stored, before it was current, or not \
         clean, or a dry run handed one over"
    );
    assert_eq!(current, signature, "The dry run moved the current revision");
}

/// A commit stands when its operation fails to take it as committed.
#[tokio::test]
async fn a_failed_hand_over_leaves_the_commit_standing() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_failing_committed(
        dir.path(),
    ));

    let (signature, current) = commit_then_dry_run(&dir, filesystem.clone()).await;

    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)],
        "The commit handed over another revision than it stored"
    );
    assert_eq!(current, signature, "The failed hand-over undid the commit");
}

/// A provider that does not hold its state is told of no content stored and handed no revision:
/// its working tree is the files.
#[tokio::test]
async fn a_provider_not_holding_its_state_is_handed_nothing() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::reading(dir.path()));

    commit_then_dry_run(&dir, filesystem.clone()).await;

    assert!(filesystem.stored_calls.lock().is_empty());
    assert!(filesystem.committed_calls.lock().is_empty());
}

/// The operation a commit read its files through, on a provider holding its state, is told what
/// each file was stored as, the address, size and mode the revision records, before the revision
/// is handed over. A view-excluded file, whose content the commit takes from the staged node, is
/// not told of.
#[tokio::test]
async fn the_operation_is_told_what_each_file_in_view_was_stored_as_before_the_hand_over() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_reading(dir.path()));
    std::fs::write(dir.path().join("one.txt"), b"one").expect("write file");
    std::fs::write(dir.path().join("excluded.txt"), b"one").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();
    let committed = filesystem.clone();
    let mut filter = Filter::default();
    filter
        .view
        .add_exclusion("excluded.txt")
        .expect("A view line");

    let (signature, node) = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                committed,
                Arc::new(filter),
            )
            .await;
            stage_working_tree(&fixture, &root).await;
            commit_staged(&fixture.repository, &fixture.token, test_commit())
                .await
                .expect("Committing through the OS filesystem");

            std::fs::write(root.join("one.txt"), b"two!").expect("rewrite file");
            std::fs::write(root.join("excluded.txt"), b"two!").expect("rewrite file");
            stage_working_tree(&fixture, &root).await;
            let signature = commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("Committing");
            let node = State::deserialize(committing.clone(), signature)
                .await
                .expect("The committed State")
                .find_node(committing, "one.txt")
                .await
                .expect("The committed file");
            (signature, node)
        }))
        .await
        .expect("Test task failed");

    assert_eq!(
        *filesystem.stored_calls.lock(),
        [(
            String::from("one.txt"),
            node.address,
            node.size,
            node.mode,
            false
        )],
        "The commit told of other content than the revision records, of the view-excluded file, \
         or after the hand-over"
    );
    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)]
    );
}

/// A commit of a link alone or of a layer is refused on a provider holding its state, which would
/// make a revision current that the provider is not handed.
#[tokio::test]
async fn a_commit_of_a_link_or_a_layer_alone_is_refused_on_a_provider_holding_its_state() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state());
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = test_repository(filesystem.clone()).await;
            let token = RepositoryWriteToken::acquire(dir.path()).await;
            for (options, what) in [
                (
                    CommitOptions {
                        link: Some(String::from("linked")),
                        ..test_commit()
                    },
                    "commit a link alone",
                ),
                (
                    CommitOptions {
                        layer: Some(String::from("layered")),
                        ..test_commit()
                    },
                    "commit a layer",
                ),
            ] {
                let refused = commit_staged(&repository, &token, options).await;
                assert!(
                    matches!(&refused, Err(CommitError::InvalidArguments(error))
                        if error.reason == hold::refused(what).reason),
                    "{refused:?}"
                );
            }
        })
        .await;
    assert!(filesystem.committed_calls.lock().is_empty());
}

/// A commit carrying a staged layer is refused on a provider holding its state, before it makes a
/// revision current: the layer's revision would be current without the provider being handed it.
#[tokio::test]
async fn a_commit_carrying_a_staged_layer_is_refused_on_a_provider_holding_its_state() {
    #[derive(Serialize)]
    struct LayerConfig {
        layers: Vec<Layer>,
    }

    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_reading(dir.path()));
    std::fs::write(dir.path().join("one.txt"), b"one").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();
    let committed = filesystem.clone();

    let (signature, refused, current) = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                committed,
                Arc::default(),
            )
            .await;
            stage_working_tree(&fixture, &root).await;
            let signature = commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("Committing");

            std::fs::write(root.join("one.txt"), b"two").expect("rewrite file");
            stage_working_tree(&fixture, &root).await;
            lore_revision::util::config::save(
                &LayerConfig {
                    layers: vec![Layer {
                        target_path: String::from("layered"),
                        repository: RepositoryId::from(uuid::Uuid::now_v7()),
                        staged: Hash::from([1u8; 32]),
                        ..Default::default()
                    }],
                },
                layer::layer_config_path(&committing).expect("Layer config path"),
            )
            .await
            .expect("Writing the layer set");
            let refused = commit_staged(&committing, &fixture.token, test_commit()).await;
            let (current, _) = lore_revision::instance::load_current_anchor_boxed(&committing)
                .await
                .expect("The current anchor");
            (signature, refused, current)
        }))
        .await
        .expect("Test task failed");

    assert!(
        matches!(&refused, Err(CommitError::InvalidArguments(error))
            if error.reason == hold::refused("commit a layer").reason),
        "{refused:?}"
    );
    assert_eq!(
        current, signature,
        "The refused commit moved the current revision"
    );
    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)],
        "The refused commit handed a revision over"
    );
}

/// Commits a file written to `dir`, then an edit of it on a dry run, both through a repository
/// over `filesystem`, staging through the OS filesystem. Answers the revision the commit
/// returned and the revision current after the dry run.
async fn commit_then_dry_run(
    dir: &lore_base::test_util::TempDir,
    filesystem: Arc<TestFilesystemProvider>,
) -> (Hash, Hash) {
    std::fs::write(dir.path().join("one.txt"), b"one").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();

    runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                filesystem,
                Arc::default(),
            )
            .await;

            stage_working_tree(&fixture, &root).await;
            let signature = commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("Committing");

            std::fs::write(root.join("one.txt"), b"two").expect("rewrite file");
            stage_working_tree(&fixture, &root).await;
            let dry_run = test_execution_with(LoreGlobalArgs {
                dry_run: 1,
                ..Default::default()
            });
            LORE_CONTEXT
                .scope(
                    dry_run,
                    commit_staged(&committing, &fixture.token, test_commit()),
                )
                .await
                .expect("A dry run commits");
            let (current, _) = lore_revision::instance::load_current_anchor_boxed(&committing)
                .await
                .expect("The current anchor");
            (signature, current)
        }))
        .await
        .expect("Test task failed")
}

/// An [`OsFilesystem`] that counts the operations opened on it.
struct CountingFilesystem {
    inner: OsFilesystem,
    begins: Arc<AtomicUsize>,
}

#[async_trait]
impl FilesystemProvider for CountingFilesystem {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        self.begins.fetch_add(1, Ordering::AcqRel);
        FilesystemProvider::begin_operation(&self.inner).await
    }
}

/// A repository over the working tree at `root`, its filesystem counting the operations
/// opened on it.
struct CountingRepository {
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    begins: Arc<AtomicUsize>,
}

/// A repository created at `root`, anchored on a fresh default branch, over a filesystem
/// that counts.
///
/// Call from inside a `LORE_CONTEXT` scope: creating a repository reads the execution
/// context.
async fn counting_repository(
    root: &std::path::Path,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> CountingRepository {
    let begins = Arc::new(AtomicUsize::new(0));
    let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    let token = RepositoryWriteToken::acquire(root).await;
    let created = lore_revision::repository::create_local(
        root,
        &token,
        repository_id,
        branch_id,
        branch::DEFAULT_DEFAULT_NAME.to_string(),
        lore_revision::repository::RepositoryConfig::default(),
        false,
    )
    .await
    .expect("Initializing the repository");

    let repository = Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(root)
                .with_id(repository_id)
                .with_instance_id(created.instance_id)
                .with_filesystem_provider(Arc::new(CountingFilesystem {
                    inner: OsFilesystem::new(root),
                    begins: begins.clone(),
                })),
        )
        .with_write_token(token.share()),
    );
    lore_revision::instance::store_current_anchor_branch(&repository, branch_id)
        .await
        .expect("Storing the anchor branch");

    CountingRepository {
        repository,
        token,
        begins,
    }
}

/// A repository created at `root` as [`counting_repository`] makes it, which stages, and a context
/// of the same repository over `filesystem`, under `filter`, which commits.
///
/// Call from inside a `LORE_CONTEXT` scope.
async fn committing_repository(
    root: &std::path::Path,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    filesystem: Arc<TestFilesystemProvider>,
    filter: Arc<Filter>,
) -> (CountingRepository, Arc<RepositoryContext>) {
    let fixture = counting_repository(root, immutable_store.clone(), mutable_store.clone()).await;
    let committing = Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(root)
                .with_id(fixture.repository.id)
                .with_instance_id(fixture.repository.instance_id)
                .with_filesystem_provider(filesystem)
                .with_filter(filter),
        )
        .with_write_token(fixture.token.share()),
    );
    (fixture, committing)
}

/// Stages everything the working tree at `root` holds, scanning rather than reading dirty
/// flags so a fixture that wrote its files directly is staged whole.
async fn stage_working_tree(fixture: &CountingRepository, root: &std::path::Path) {
    lore_revision::file::stage::stage(
        fixture.repository.clone(),
        &fixture.token,
        LoreArray::from_vec(vec![LoreString::from(&root.to_path_buf())]),
        lore_revision::stage::StageOptions {
            scan: true,
            ..Default::default()
        },
    )
    .await
    .expect("Staging the working tree");
}

/// A commit with the message `test`.
fn test_commit() -> CommitOptions {
    CommitOptions::new(String::from("test"))
}

/// Commits what `repository` holds staged as `options` asks, with no metadata beyond what a
/// commit stamps.
async fn commit_staged(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
) -> Result<Hash, CommitError> {
    Box::pin(commit_with_metadata(
        repository.clone(),
        token,
        options,
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
    ))
    .await
}
