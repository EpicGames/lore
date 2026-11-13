// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::load::*;
use lore::storage::handle as storage_handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::InvalidArguments;
use lore_base::error::NotFound;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::FfiError;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreGlobalArgs;

use crate::call::test_support::complete_statuses;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;
use crate::revision_tree::handle::test_support;
use crate::revision_tree::support::loaded_handle_id;

/// With the parent gone the check unregisters the tree rather than merely reporting;
/// with the parent alive it leaves it registered. The interleaving itself is not
/// reachable — there is no controllable yield between the lookup and the registration.
#[tokio::test]
async fn a_load_withdraws_its_handle_when_the_parent_storage_handle_is_gone() {
    let store_handle = storage_handle::register(in_memory_for_tests("withdraw").await);
    let live = rt_handle::register(test_support::new_for_testing().await);
    assert!(
        !withdraw_if_parent_closed(store_handle, live),
        "a live parent must leave the handle alone"
    );
    assert!(rt_handle::lookup(live).is_some());
    rt_handle::unregister(live);

    let orphan = rt_handle::register(test_support::new_for_testing().await);
    storage_handle::unregister(store_handle);
    assert!(
        withdraw_if_parent_closed(store_handle, orphan),
        "a closed parent must withdraw the handle"
    );
    assert!(
        rt_handle::lookup(orphan).is_none(),
        "the withdrawn handle must not stay in the registry",
    );
}

#[tokio::test]
async fn load_from_zero_hash_returns_empty_handle() {
    let store = in_memory_for_tests("load-zero-hash").await;
    let store_handle = storage_handle::register(store);
    let (sink, callback) = recording_callback();
    let args = LoreRevisionTreeLoadArgs {
        store: store_handle,
        repository: Partition::from([0x11u8; 16]),
        revision_hash: Hash::default(),
    };

    let status = load(LoreGlobalArgs::default(), args, callback).await;

    assert_eq!(status, 0);
    let events = take_recorded(&sink);
    let id = loaded_handle_id(&events)
        .unwrap_or_else(|| panic!("missing RevisionTreeLoaded event, got {events:?}"));
    assert_ne!(id, 0);
    assert!(
        rt_handle::lookup(lore::revision_tree::handle::LoreRevisionTree { handle_id: id })
            .is_some(),
        "loaded handle must be present in the registry",
    );
    assert!(
        complete_statuses(&events).contains(&0),
        "Complete event must report status=0, got {events:?}"
    );
    let loaded_pos = events
        .iter()
        .position(|e| matches!(e, LoreEvent::RevisionTreeLoaded(_)))
        .expect("RevisionTreeLoaded must be present");
    let complete_pos = events
        .iter()
        .position(|e| matches!(e, LoreEvent::Complete(_)))
        .expect("Complete must be present");
    assert!(
        loaded_pos < complete_pos,
        "RevisionTreeLoaded must fire before Complete, got {events:?}"
    );

    rt_handle::unregister(lore::revision_tree::handle::LoreRevisionTree { handle_id: id });
    storage_handle::unregister(store_handle);
}

#[tokio::test]
async fn load_from_unknown_hash_fails_with_not_found() {
    let store = in_memory_for_tests("load-unknown-hash").await;
    let store_handle = storage_handle::register(store);
    let (sink, callback) = recording_callback();
    let args = LoreRevisionTreeLoadArgs {
        store: store_handle,
        repository: Partition::from([0x22u8; 16]),
        revision_hash: Hash::from([0x99u8; 32]),
    };

    let status = load(LoreGlobalArgs::default(), args, callback).await;

    let expected_code = LoadError::from(NotFound).ffi_code();
    assert_eq!(status, expected_code);
    let events = take_recorded(&sink);
    assert!(
        complete_statuses(&events).contains(&expected_code),
        "Complete event must report the not-found code, got {events:?}"
    );
    assert!(
        loaded_handle_id(&events).is_none(),
        "failed load must not emit RevisionTreeLoaded, got {events:?}"
    );

    storage_handle::unregister(store_handle);
}

#[tokio::test]
async fn load_with_unknown_store_handle_fails_with_invalid_arguments() {
    let (sink, callback) = recording_callback();
    let args = LoreRevisionTreeLoadArgs {
        store: LoreStore::INVALID,
        repository: Partition::default(),
        revision_hash: Hash::default(),
    };

    let status = load(LoreGlobalArgs::default(), args, callback).await;

    let expected_code = LoadError::from(InvalidArguments {
        reason: "storage handle is unknown or has been closed".into(),
    })
    .ffi_code();
    assert_eq!(status, expected_code);
    let events = take_recorded(&sink);
    assert!(
        complete_statuses(&events).contains(&expected_code),
        "Complete event must report the invalid-arguments code, got {events:?}"
    );
}

/// The tree holds its storage handle's own state, whose connection the teardown cascade
/// matches on.
#[tokio::test]
async fn load_holds_the_storage_handles_state_for_the_close_cascade() {
    let store = in_memory_for_tests("load-parent-capture").await;
    let store_handle = storage_handle::register(store.clone());
    let (sink, callback) = recording_callback();
    let args = LoreRevisionTreeLoadArgs {
        store: store_handle,
        repository: Partition::from([0x44u8; 16]),
        revision_hash: Hash::default(),
    };

    let status = load(LoreGlobalArgs::default(), args, callback).await;
    assert_eq!(status, 0);

    let events = take_recorded(&sink);
    let id = loaded_handle_id(&events).expect("RevisionTreeLoaded event");
    let entry = rt_handle::REGISTRY
        .get(&id)
        .expect("registered revision tree handle");
    assert!(
        Arc::ptr_eq(&entry.store_internal, &store),
        "internal must hold the storage handle's own state for the cascade"
    );
    drop(entry);

    rt_handle::unregister(lore::revision_tree::handle::LoreRevisionTree { handle_id: id });
    storage_handle::unregister(store_handle);
}

#[tokio::test]
async fn load_two_revision_trees_against_different_repositories_on_one_storage_handle() {
    let store = in_memory_for_tests("load-multi-repo").await;
    let store_handle = storage_handle::register(store);

    let (sink_a, callback_a) = recording_callback();
    let status_a = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store: store_handle,
            repository: Partition::from([0xAAu8; 16]),
            revision_hash: Hash::default(),
        },
        callback_a,
    )
    .await;
    assert_eq!(status_a, 0);
    let id_a = loaded_handle_id(&take_recorded(&sink_a)).expect("first load event missing");

    let (sink_b, callback_b) = recording_callback();
    let status_b = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store: store_handle,
            repository: Partition::from([0xBBu8; 16]),
            revision_hash: Hash::default(),
        },
        callback_b,
    )
    .await;
    assert_eq!(status_b, 0);
    let id_b = loaded_handle_id(&take_recorded(&sink_b)).expect("second load event missing");

    assert_ne!(id_a, id_b, "two loads must produce distinct handles");
    let entry_a = rt_handle::REGISTRY
        .get(&id_a)
        .expect("handle A must be registered");
    let entry_b = rt_handle::REGISTRY
        .get(&id_b)
        .expect("handle B must be registered");
    assert_eq!(entry_a.repository, Partition::from([0xAAu8; 16]));
    assert_eq!(entry_b.repository, Partition::from([0xBBu8; 16]));
    drop(entry_a);
    drop(entry_b);

    rt_handle::unregister(lore::revision_tree::handle::LoreRevisionTree { handle_id: id_a });
    rt_handle::unregister(lore::revision_tree::handle::LoreRevisionTree { handle_id: id_b });
    storage_handle::unregister(store_handle);
}
