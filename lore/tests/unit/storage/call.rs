// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore::storage::call::*;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore_base::error::InvalidArguments;
use lore_error_set::FfiError;
use lore_revision::interface::LoreGlobalArgs;
use lore_storage::StorageError;

use crate::call::test_support::completes;
use crate::call::test_support::has_error_event;
use crate::call::test_support::recording_callback;
use crate::call::test_support::registered_store;
use crate::call::test_support::take_recorded;

#[tokio::test]
async fn handle_miss_completes_with_error_code_and_no_error_event() {
    let (sink, callback) = recording_callback();
    let status = storage_call(
        LoreGlobalArgs::default(),
        callback,
        LoreStore::INVALID,
        (),
        "handle_miss_test",
        |_store, _args: ()| async move { Ok::<_, StorageError>(()) },
    )
    .await;

    let events = take_recorded(&sink);
    assert!(
        !has_error_event(&events),
        "no Error event must be emitted on terminal failure"
    );

    let completes = completes(&events);
    assert_eq!(completes.len(), 1, "exactly one Complete event");

    // The status holds the handle-miss error's real error code and the detail
    // carries the same code and message.
    let expected = StorageError::from(InvalidArguments {
        reason: "storage handle is unknown or has been closed".into(),
    });
    let expected_code = expected.ffi_code();
    // The synchronous return equals the error code, matching `Complete.status`.
    assert_eq!(status, expected_code);
    let data = &completes[0];
    assert_eq!(data.status, expected_code);
    assert_eq!(data.error.error_code, expected_code);
    assert_eq!(data.error.message.as_str(), expected.to_string());
}

#[tokio::test]
async fn happy_path_completes_with_status_zero_and_decrements_counter() {
    let (store, store_handle) = registered_store("call-test").await;
    assert_eq!(store.in_flight.load(Ordering::Acquire), 0);

    let invoked = Arc::new(AtomicU64::new(0));
    let invoked_clone = invoked.clone();

    let (sink, callback) = recording_callback();
    let status = storage_call(
        LoreGlobalArgs::default(),
        callback,
        store_handle,
        (),
        "happy_path_test",
        move |store_arc, _args: ()| async move {
            invoked_clone.fetch_add(1, Ordering::AcqRel);
            assert!(store_arc.in_flight.load(Ordering::Acquire) >= 1);
            Ok::<_, StorageError>(())
        },
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(invoked.load(Ordering::Acquire), 1);
    assert_eq!(
        store.in_flight.load(Ordering::Acquire),
        0,
        "counter must return to zero after the op"
    );
    let events = take_recorded(&sink);
    let completes = completes(&events);
    assert_eq!(completes.len(), 1, "exactly one Complete event");
    let data = &completes[0];
    assert_eq!(data.status, 0);
    assert_eq!(data.error.error_code, 0);
    assert!(data.error.message.is_empty());
    assert!(data.error.trace_locations.is_empty());
    handle::unregister(store_handle);
}

#[tokio::test]
async fn op_error_completes_with_error_code_and_no_error_event() {
    let (store, store_handle) = registered_store("call-test").await;
    let (sink, callback) = recording_callback();
    let status = storage_call(
        LoreGlobalArgs::default(),
        callback,
        store_handle,
        (),
        "op_error_test",
        move |_store, _args: ()| async move {
            Err::<(), _>(StorageError::from(InvalidArguments {
                reason: "simulated op error".into(),
            }))
        },
    )
    .await;
    assert_eq!(
        store.in_flight.load(Ordering::Acquire),
        0,
        "counter must return to zero even on error"
    );

    let events = take_recorded(&sink);
    assert!(
        !has_error_event(&events),
        "no Error event must be emitted on terminal failure"
    );

    let completes = completes(&events);
    assert_eq!(completes.len(), 1, "exactly one Complete event");

    // The status holds the op error's real error code and the detail carries
    // the same code and message.
    let expected = StorageError::from(InvalidArguments {
        reason: "simulated op error".into(),
    });
    let expected_code = expected.ffi_code();
    // The synchronous return equals the error code, matching `Complete.status`.
    assert_eq!(status, expected_code);
    let data = &completes[0];
    assert_eq!(data.status, expected_code);
    assert_eq!(data.error.error_code, expected_code);
    assert_eq!(data.error.message.as_str(), expected.to_string());
    handle::unregister(store_handle);
}
