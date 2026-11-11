// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::fs::filesystem_provider::HoldOptions;
use lore_revision::fs::realize::realize_state;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::RepositoryId;
use lore_revision::revision::sync::SyncError;
use lore_revision::revision::sync::SyncOptions;
use lore_revision::state::State;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_execution_with;
use crate::fs::filesystem_provider::test_repository_over;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;

/// Two committed states over the same real, OS-backed fixture: the first with one file, the
/// second with a second file added.
async fn two_committed_states(
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
) -> (Arc<State>, Arc<State>) {
    let instance = test_repository_create(
        immutable_store,
        mutable_store,
        RepositoryId::from(uuid::Uuid::now_v7()),
    )
    .await;
    test_file_write(&instance.path.join("first.txt"), b"first");
    let first = test_commit_tree(&instance, "First").await;
    test_file_write(&instance.path.join("second.txt"), b"second");
    let second = test_commit_tree(&instance, "Second").await;
    (first, second)
}

/// What a sync onto a provider holding its state did.
struct SyncOutcome {
    result: Result<(), SyncError>,
    holds: Vec<(Hash, HoldOptions)>,
    /// How many times the held ignore file was read into the store.
    warms: usize,
    file_infos: usize,
    target: Hash,
}

/// What a sync from the first to the second of [`two_committed_states`] did on `filesystem`, a
/// provider holding its state, with `options`, realized under global arguments `args`.
async fn sync_onto(
    filesystem: TestFilesystemProvider,
    options: SyncOptions,
    args: LoreGlobalArgs,
) -> SyncOutcome {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let (state_current, state_target) = Box::pin(two_committed_states(
                immutable_store.clone(),
                mutable_store.clone(),
            ))
            .await;
            let filesystem = Arc::new(filesystem);
            let repository =
                test_repository_over(immutable_store, mutable_store, filesystem.clone());
            let target = state_target.clone();
            let result = LORE_CONTEXT
                .scope(test_execution_with(args), async move {
                    let operation = repository.file_system().begin_operation().await.unwrap();
                    Box::pin(realize_state(
                        repository.clone(),
                        repository,
                        operation,
                        state_current,
                        target,
                        options,
                    ))
                    .await
                })
                .await;
            SyncOutcome {
                result,
                holds: filesystem.hold_calls.lock().clone(),
                warms: filesystem.warm_count.load(Ordering::SeqCst),
                file_infos: filesystem.file_infos(),
                target: state_target.revision(),
            }
        })
        .await
}

/// A sync onto a provider holding its state hands it the target alone, once, reading nothing
/// itself: the provider diffs and checks against the State it holds. The sync then reads the held
/// ignore file into the store once.
#[tokio::test]
async fn a_sync_hands_a_provider_holding_its_state_the_target_alone() {
    let outcome = sync_onto(
        TestFilesystemProvider::holding_state(),
        SyncOptions::default(),
        LoreGlobalArgs::default(),
    )
    .await;
    outcome.result.expect("the sync succeeds");
    assert_eq!(
        vec![(outcome.target, HoldOptions::default())],
        outcome.holds
    );
    assert_eq!(0, outcome.file_infos, "the sync reads no path");
    assert_eq!(1, outcome.warms, "the sync reads the held ignore file once");
}

/// A forced sync asks the provider to overwrite local changes.
#[tokio::test]
async fn a_forced_sync_holds_the_target_overwriting_local_changes() {
    let outcome = sync_onto(
        TestFilesystemProvider::holding_state(),
        SyncOptions::default(),
        LoreGlobalArgs {
            force: 1,
            ..Default::default()
        },
    )
    .await;
    outcome.result.expect("the forced sync succeeds");
    let options = HoldOptions {
        overwrite_local: true,
        ..Default::default()
    };
    assert_eq!(vec![(outcome.target, options)], outcome.holds);
}

/// A sync forwarding local changes asks the provider to keep them.
#[tokio::test]
async fn a_sync_forwarding_local_changes_holds_the_target_keeping_them() {
    let outcome = sync_onto(
        TestFilesystemProvider::holding_state(),
        SyncOptions {
            forward_changes: true,
            ..Default::default()
        },
        LoreGlobalArgs::default(),
    )
    .await;
    outcome.result.expect("the sync succeeds");
    let options = HoldOptions {
        keep_local: true,
        ..Default::default()
    };
    assert_eq!(vec![(outcome.target, options)], outcome.holds);
}

/// A dry-run sync asks the provider for a dry run, and reads no ignore file into the store.
#[tokio::test]
async fn a_dry_run_sync_holds_the_target_in_a_dry_run() {
    let outcome = sync_onto(
        TestFilesystemProvider::holding_state(),
        SyncOptions::default(),
        LoreGlobalArgs {
            dry_run: 1,
            ..Default::default()
        },
    )
    .await;
    outcome.result.expect("the dry run succeeds");
    let options = HoldOptions {
        dry_run: true,
        ..Default::default()
    };
    assert_eq!(vec![(outcome.target, options)], outcome.holds);
    assert_eq!(0, outcome.warms, "a dry run reads no ignore file");
}

/// A hold the provider refuses for local changes fails the sync with `LocalModifications`, and reads
/// no ignore file into the store.
#[tokio::test]
async fn a_hold_refused_for_local_changes_fails_the_sync() {
    let outcome = sync_onto(
        TestFilesystemProvider::refusing_holds(),
        SyncOptions::default(),
        LoreGlobalArgs::default(),
    )
    .await;
    assert!(
        matches!(outcome.result, Err(SyncError::LocalModifications(_))),
        "{:?}",
        outcome.result
    );
    assert_eq!(0, outcome.warms, "a refused hold reads no ignore file");
}
