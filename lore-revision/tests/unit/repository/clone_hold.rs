// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::fs::filesystem_provider::HoldOptions;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::repository::clone::CloneContext;
use lore_revision::repository::clone::CloneError;
use lore_revision::repository::clone::CloneOptions;
use lore_revision::repository::clone::clone_onto_held_state;
use lore_revision::repository::clone::refuse_on_held_state;
use lore_revision::state::State;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_execution_with;
use crate::fs::filesystem_provider::test_repository;

/// What a clone onto a provider holding its state did.
struct CloneOutcome {
    result: Result<(), CloneError>,
    holds: Vec<(Hash, HoldOptions)>,
    file_infos: usize,
    /// How many times the held ignore file was read into the store.
    warms: usize,
}

/// What a clone of `revision` with `options` onto a provider holding its state did, run under
/// `execution` (`clone_onto_held_state`, its caching awaited).
async fn holds_of_clone(
    revision: Hash,
    options: CloneOptions,
    execution: Arc<ExecutionContext>,
) -> CloneOutcome {
    LORE_CONTEXT
        .scope(execution, async move {
            let filesystem = Arc::new(TestFilesystemProvider::holding_state());
            let repository = test_repository(filesystem.clone()).await;
            let state = State::new();
            state.set_revision(revision);
            let operation = repository.file_system().begin_operation().await.unwrap();
            assert!(operation.holds_state());

            let ctx = CloneContext {
                operation,
                repository,
                state,
                options: Arc::new(options),
                stats: Arc::default(),
                modified_times: Arc::default(),
            };
            let mut result = clone_onto_held_state(&ctx).await;
            if let Ok(Some(cache_task)) = &mut result {
                let _ = cache_task.await;
            }
            CloneOutcome {
                result: result.map(drop),
                holds: filesystem.hold_calls.lock().clone(),
                file_infos: filesystem.file_infos(),
                warms: filesystem.warm_count.load(Ordering::SeqCst),
            }
        })
        .await
}

/// A clone onto a provider holding its state hands it the cloned revision once, reads nothing
/// from a filesystem, and then reads the held ignore file into the store once.
#[tokio::test]
async fn a_clone_hands_a_provider_holding_its_state_the_revision_once() {
    let outcome = holds_of_clone(
        Hash::from_u64(7),
        CloneOptions::default(),
        test_execution_with(LoreGlobalArgs::default()),
    )
    .await;
    outcome.result.expect("the clone succeeds");
    assert_eq!(
        vec![(Hash::from_u64(7), HoldOptions::default())],
        outcome.holds
    );
    assert_eq!(
        0, outcome.file_infos,
        "the clone reads nothing from a filesystem"
    );
    assert_eq!(
        1, outcome.warms,
        "the clone reads the held ignore file once"
    );
}

/// A bare clone onto a provider holding its state hands it nothing, as a bare clone writes no
/// tree, and reads no ignore file.
#[tokio::test]
async fn a_bare_clone_hands_a_provider_holding_its_state_nothing() {
    let outcome = holds_of_clone(
        Hash::from_u64(7),
        CloneOptions {
            bare: true,
            ..Default::default()
        },
        test_execution_with(LoreGlobalArgs::default()),
    )
    .await;
    outcome.result.expect("the bare clone succeeds");
    assert!(
        outcome.holds.is_empty(),
        "a bare clone held {:?}",
        outcome.holds
    );
    assert_eq!(0, outcome.warms, "a bare clone reads no ignore file");
}

/// A dry-run clone onto a provider holding its state hands it nothing, and reads no ignore file.
#[tokio::test]
async fn a_dry_run_clone_hands_a_provider_holding_its_state_nothing() {
    let outcome = holds_of_clone(
        Hash::from_u64(7),
        CloneOptions::default(),
        test_execution_with(LoreGlobalArgs {
            dry_run: 1,
            ..Default::default()
        }),
    )
    .await;
    outcome.result.expect("the dry-run clone succeeds");
    assert!(
        outcome.holds.is_empty(),
        "a dry-run clone held {:?}",
        outcome.holds
    );
    assert_eq!(0, outcome.warms, "a dry-run clone reads no ignore file");
}

/// A clone restricted to a dependency set onto a provider holding its state is refused, since the
/// provider holds whole revisions. A bare one, one of an empty repository, and one onto a provider
/// that holds no State, are not.
#[tokio::test]
async fn a_dependency_set_clone_onto_a_provider_holding_its_state_is_refused() {
    let restricted = |bare| CloneOptions {
        bare,
        root_files: vec!["first.txt".into()],
        ..Default::default()
    };
    let revision = Hash::from_u64(7);
    let holding = test_repository(Arc::new(TestFilesystemProvider::holding_state())).await;
    let result = refuse_on_held_state(&holding, &restricted(false), revision);
    assert!(
        matches!(result, Err(CloneError::InvalidArguments(_))),
        "the clone is refused, got {result:?}"
    );
    refuse_on_held_state(&holding, &restricted(true), revision)
        .expect("a bare clone is refused nothing");
    refuse_on_held_state(&holding, &restricted(false), Hash::default())
        .expect("a clone of an empty repository is refused nothing");
    let os = test_repository(Arc::new(TestFilesystemProvider::new())).await;
    refuse_on_held_state(&os, &restricted(false), revision)
        .expect("a provider holding no State is refused nothing");
}

/// A clone of an empty repository onto a provider holding its state hands it nothing, as an
/// ordinary clone of one writes nothing, and reads no ignore file.
#[tokio::test]
async fn an_empty_repository_clone_hands_a_provider_holding_its_state_nothing() {
    let outcome = holds_of_clone(
        Hash::default(),
        CloneOptions::default(),
        test_execution_with(LoreGlobalArgs::default()),
    )
    .await;
    outcome.result.expect("the empty clone succeeds");
    assert!(
        outcome.holds.is_empty(),
        "an empty clone held {:?}",
        outcome.holds
    );
    assert_eq!(0, outcome.warms, "an empty clone reads no ignore file");
}
