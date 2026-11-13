// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;
use std::time::Instant;

use lore::revision_tree::close::*;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::handle::RevisionTreeGuard;
use lore::storage::handle as storage_handle;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::InvalidArguments;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreGlobalArgs;

use crate::call::test_support::complete_statuses;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;
use crate::revision_tree::support::load_handle;
use crate::revision_tree::support::load_tree;

/// The caller id and error code of every `RevisionTreeCloseComplete` event in `events`, in order.
fn close_outcomes(events: &[LoreEvent]) -> Vec<(u64, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeCloseComplete(data) => Some((data.id, data.error_code)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn close_releases_handle_and_subsequent_close_returns_invalid_arguments() {
    let (handle_value, store_handle_id) =
        load_handle("close-releases", Partition::from([0x11u8; 16])).await;

    let (sink_first, callback_first) = recording_callback();
    let status_first = close(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCloseArgs {
            id: 42,
            handle: handle_value,
            ..Default::default()
        },
        callback_first,
    )
    .await;
    assert_eq!(status_first, 0, "first close must succeed");
    let events_first = take_recorded(&sink_first);
    assert!(
        close_outcomes(&events_first).contains(&(42, LoreErrorCode::None)),
        "first close must emit RevisionTreeCloseComplete with the caller id, got {events_first:?}"
    );
    assert!(
        complete_statuses(&events_first).contains(&0),
        "first close must complete with status=0, got {events_first:?}"
    );
    let close_complete_pos = events_first
        .iter()
        .position(|e| matches!(e, LoreEvent::RevisionTreeCloseComplete(_)))
        .expect("RevisionTreeCloseComplete must be present");
    let complete_pos = events_first
        .iter()
        .position(|e| matches!(e, LoreEvent::Complete(_)))
        .expect("Complete must be present");
    assert!(
        close_complete_pos < complete_pos,
        "RevisionTreeCloseComplete must fire before Complete, got {events_first:?}"
    );
    assert!(
        rt_handle::lookup(handle_value).is_none(),
        "handle must be unregistered after close",
    );

    let (sink_second, callback_second) = recording_callback();
    let status_second = close(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCloseArgs {
            id: 43,
            handle: handle_value,
            ..Default::default()
        },
        callback_second,
    )
    .await;
    assert_eq!(
        status_second,
        InvalidArguments::FFI_CODE,
        "second close on the same handle must fail"
    );
    let events_second = take_recorded(&sink_second);
    assert!(
        close_outcomes(&events_second).contains(&(43, LoreErrorCode::InvalidArguments)),
        "second close must emit RevisionTreeCloseComplete with InvalidArguments, got {events_second:?}"
    );
    assert!(
        complete_statuses(&events_second).contains(&InvalidArguments::FFI_CODE),
        "second close must complete with status=InvalidArguments, got {events_second:?}"
    );

    storage_handle::unregister(lore::storage::handle::LoreStore {
        handle_id: store_handle_id,
    });
}

#[tokio::test]
async fn close_with_unknown_handle_returns_invalid_arguments() {
    let (sink, callback) = recording_callback();
    let status = close(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCloseArgs {
            id: 7,
            handle: LoreRevisionTree::INVALID,
            ..Default::default()
        },
        callback,
    )
    .await;
    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "close on INVALID must fail with status=InvalidArguments"
    );
    let events = take_recorded(&sink);
    assert!(
        close_outcomes(&events).contains(&(7, LoreErrorCode::InvalidArguments)),
        "close on INVALID must emit RevisionTreeCloseComplete with InvalidArguments, got {events:?}"
    );
    assert!(
        complete_statuses(&events).contains(&InvalidArguments::FFI_CODE),
        "close on INVALID must complete with status=InvalidArguments, got {events:?}"
    );
}

#[tokio::test]
async fn close_with_unknown_handle_emits_close_complete_with_invalid_arguments_error_code() {
    let (sink, callback) = recording_callback();
    let status = close(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCloseArgs {
            id: 4242,
            handle: LoreRevisionTree::INVALID,
            ..Default::default()
        },
        callback,
    )
    .await;
    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "close on INVALID must fail with status=InvalidArguments"
    );
    let events = take_recorded(&sink);
    assert!(
        close_outcomes(&events).contains(&(4242, LoreErrorCode::InvalidArguments)),
        "failed close must still emit RevisionTreeCloseComplete carrying the caller id, got {events:?}"
    );
}

/// Close must block until the in-flight counter drains. A held
/// `RevisionTreeGuard` keeps the counter at 1 so close's
/// `mark_invalid_and_await` cannot complete until the guard drops.
#[allow(clippy::disallowed_methods)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_waits_for_in_flight_to_drain() {
    let (handle_value, store_handle_id) =
        load_handle("close-drain-wait", Partition::from([0x33u8; 16])).await;
    let guard = RevisionTreeGuard::enter(handle_value).expect("guard enter must succeed");

    let (sink, callback) = recording_callback();
    let close_task = tokio::spawn(async move {
        close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 99,
                handle: handle_value,
                ..Default::default()
            },
            callback,
        )
        .await
    });

    let deadline = Instant::now() + Duration::from_secs(1);
    while rt_handle::lookup(handle_value).is_some() {
        if Instant::now() > deadline {
            panic!("close never unregistered the handle");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        !close_task.is_finished(),
        "close must block while the in-flight counter is non-zero",
    );

    drop(guard);

    let status = close_task.await.expect("close task join");
    assert_eq!(status, 0, "close must complete with status=0 after drain");
    let events = take_recorded(&sink);
    assert!(
        close_outcomes(&events).contains(&(99, LoreErrorCode::None)),
        "drained close must emit RevisionTreeCloseComplete, got {events:?}"
    );

    storage_handle::unregister(lore::storage::handle::LoreStore {
        handle_id: store_handle_id,
    });
}

/// Two closes racing on one handle. `handle::unregister` is an atomic
/// `DashMap::remove`, so exactly one close observes the entry and
/// succeeds; the other sees an already-closed handle and fails.
#[allow(clippy::disallowed_methods)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_close_on_same_handle_one_wins() {
    let (handle_value, store_handle_id) =
        load_handle("concurrent-close", Partition::from([0x55u8; 16])).await;

    let close_one = tokio::spawn(async move {
        close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 1,
                handle: handle_value,
                ..Default::default()
            },
            None,
        )
        .await
    });
    let close_two = tokio::spawn(async move {
        close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 2,
                handle: handle_value,
                ..Default::default()
            },
            None,
        )
        .await
    });

    let mut statuses = [
        close_one.await.expect("close one join"),
        close_two.await.expect("close two join"),
    ];
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [0, InvalidArguments::FFI_CODE],
        "exactly one close must succeed (0) and one must fail, got {statuses:?}"
    );
    assert!(
        rt_handle::lookup(handle_value).is_none(),
        "handle must be unregistered after the race",
    );

    storage_handle::unregister(lore::storage::handle::LoreStore {
        handle_id: store_handle_id,
    });
}

/// Full lifecycle: open storage, load a revision tree, close the tree,
/// then close the storage handle. Both handles must be deregistered
/// afterwards. Asserted per-handle rather than "registry empty" because
/// the registries are process-global and shared with other tests running
/// in parallel.
#[tokio::test]
async fn open_load_close_storageclose_deregisters_both_handles() {
    let store = in_memory_for_tests("lifecycle").await;
    let store_handle = storage_handle::register(store);

    let tree_handle = load_tree(store_handle, Partition::from([0x66u8; 16])).await;

    let close_status = close(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCloseArgs {
            id: 1,
            handle: tree_handle,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(close_status, 0, "revision tree close must succeed");
    assert!(
        rt_handle::lookup(tree_handle).is_none(),
        "revision tree handle must be deregistered after close",
    );

    let storage_close_status = lore::storage::close::close(
        LoreGlobalArgs::default(),
        lore::storage::close::LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(storage_close_status, 0, "storage close must succeed");
    assert!(
        storage_handle::lookup(store_handle).is_none(),
        "storage handle must be deregistered after storage close",
    );
}

/// Closing the parent storage handle leaves the revision tree handle
/// fully usable: it stays registered, a guard still enters, and the tree
/// still closes cleanly — because the revision tree holds its own `Arc`
/// to the underlying store instead of re-looking-up the storage handle.
#[tokio::test]
async fn revision_handle_survives_parent_storage_close() {
    let store = in_memory_for_tests("survive-parent-close").await;
    let store_handle = storage_handle::register(store);

    let tree_handle = load_tree(store_handle, Partition::from([0x77u8; 16])).await;

    let storage_close_status = lore::storage::close::close(
        LoreGlobalArgs::default(),
        lore::storage::close::LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(storage_close_status, 0, "parent storage close must succeed");
    assert!(
        storage_handle::lookup(store_handle).is_none(),
        "parent storage handle must be unregistered after its own close",
    );

    assert!(
        rt_handle::lookup(tree_handle).is_some(),
        "revision tree handle must remain registered after parent storage close",
    );
    let guard = RevisionTreeGuard::enter(tree_handle)
        .expect("guard must still enter after parent storage close");
    drop(guard);

    let close_status = close(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCloseArgs {
            id: 1,
            handle: tree_handle,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(close_status, 0, "revision tree close must still succeed");
}
