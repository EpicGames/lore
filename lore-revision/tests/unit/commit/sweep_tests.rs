// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The commits' calls of the sweep of detached slots (C15): a CLI commit and a C API commit each
//! free, before they save, a slot a dropped add left in flight, taken from its block's free list
//! and never published. Without its call a commit leaves its slot taken. The CLI commit's slot is
//! one an earlier session saved in the staged State, which its sweep finds only by the flag the
//! slot's block takes as it loads; the C API commit's is one taken this session in its State in
//! memory.

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::commit;
use lore_revision::commit::CommitOptions;
use lore_revision::commit::commit_in_memory_revision;
use lore_revision::lore::RepositoryId;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::State;

use crate::helper::in_memory;
use crate::helper::stage_paths;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::helper::test_store_create;
use crate::stage::sweep_tests::freed;
use crate::stage::sweep_tests::leak_a_slot;
use crate::stage::sweep_tests::staged_with_a_leaked_slot;

/// A CLI commit over a staged State an earlier session saved with a slot in flight: the
/// revision it saves holds the slot freed.
#[tokio::test]
async fn a_commit_frees_a_slot_left_in_flight_before_it_saves() {
    let (immutable, mutable, execution) =
        test_store_create().await.expect("Failed to create stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let instance =
            test_repository_create(immutable, mutable, RepositoryId::from(uuid::Uuid::now_v7()))
                .await;
        let (repository, token) = (&instance.repository, &instance.write_token);
        test_file_write(&instance.path.join("a.file"), &[0, 1, 2]);
        stage_paths(repository, &instance.path, &["."], true, false).await;
        let leaked = staged_with_a_leaked_slot(repository, token).await;

        let revision =
            commit::commit_boxed(repository.clone(), token, CommitOptions::new(String::new()))
                .await
                .expect("the commit");

        let state = State::deserialize(repository.clone(), revision)
            .await
            .expect("the committed State loads");
        assert!(
            freed(&state, repository, leaked).await,
            "the commit freed slot {leaked} before it saved"
        );
    }))
    .await
    .expect("Task failed");
}

/// A C API commit of a State in memory holding a slot a dropped add left in flight this
/// session: the State and the revision it publishes hold the slot freed.
#[tokio::test]
async fn a_c_api_commit_frees_a_slot_left_in_flight_before_it_saves() {
    let (immutable, mutable, execution) =
        test_store_create().await.expect("Failed to create stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let branch = in_memory::branch_id();
        let state = State::new();
        let dir = in_memory::add(
            &state,
            repository.clone(),
            ROOT_NODE,
            in_memory::directory("dir"),
            "dir",
        )
        .await;
        in_memory::add(
            &state,
            repository.clone(),
            dir,
            in_memory::file("a.bin"),
            "a.bin",
        )
        .await;
        let leaked = leak_a_slot(&state, &repository, dir).await;

        let revision = commit_in_memory_revision(
            repository.clone(),
            &in_memory::token(),
            state.clone(),
            in_memory::metadata_on(branch),
            Hash::default(),
            branch,
        )
        .await
        .expect("the commit");

        assert!(
            freed(&state, &repository, leaked).await,
            "the commit freed slot {leaked} in its State"
        );
        let published = State::deserialize(repository.clone(), revision)
            .await
            .expect("the committed State loads");
        assert!(
            freed(&published, &repository, leaked).await,
            "the revision holds slot {leaked} freed"
        );
    }))
    .await
    .expect("Task failed");
}
