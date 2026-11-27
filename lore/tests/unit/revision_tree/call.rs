// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore::revision_tree::call::*;
use lore::revision_tree::delete::DeleteError;
use lore::revision_tree::handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::modify::ModifyError;
use lore::revision_tree::move_node::MoveError;
use lore_base::error::InvalidArguments;
use lore_base::error::NodeNotFound;
use lore_base::error::NotAuthorized;
use lore_base::error::SlowDown;
use lore_error_set::prelude::*;
use lore_revision::event::LoreErrorCode;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::state::StateError;

use crate::call::test_support::completes;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;
use crate::revision_tree::handle::test_support;

#[tokio::test]
async fn handle_miss_completes_with_error_code() {
    let (sink, callback) = recording_callback();
    let missed = Arc::new(AtomicBool::new(false));
    let missed_setter = missed.clone();
    let status = revision_tree_call(
        LoreGlobalArgs::default(),
        callback,
        LoreRevisionTree::INVALID,
        (),
        "handle_miss_test",
        move |_args: &()| missed_setter.store(true, Ordering::SeqCst),
        |_internal, _args: ()| async move { Ok::<_, DispatchError>(()) },
    )
    .await;
    assert!(
        missed.load(Ordering::SeqCst),
        "on_handle_miss must fire when the handle is unknown"
    );
    let events = take_recorded(&sink);
    let completes = completes(&events);
    assert_eq!(completes.len(), 1, "exactly one Complete event");

    let expected = DispatchError::from(InvalidArguments {
        reason: "revision tree handle is unknown or has been closed".into(),
    });
    let expected_code = expected.ffi_code();
    assert_eq!(status, expected_code);
    let data = &completes[0];
    assert_eq!(data.status, expected_code);
    assert_eq!(data.error.error_code, expected_code);
    assert_eq!(data.error.message.as_str(), expected.to_string());
}

#[tokio::test]
async fn happy_path_completes_with_status_zero_and_decrements_counter() {
    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal.clone());
    assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);

    let invoked = Arc::new(AtomicU64::new(0));
    let invoked_clone = invoked.clone();

    let (sink, callback) = recording_callback();
    let missed = Arc::new(AtomicBool::new(false));
    let missed_setter = missed.clone();
    let status = revision_tree_call(
        LoreGlobalArgs::default(),
        callback,
        handle_value,
        (),
        "happy_path_test",
        move |_args: &()| missed_setter.store(true, Ordering::SeqCst),
        move |internal_arc, _args: ()| async move {
            invoked_clone.fetch_add(1, Ordering::AcqRel);
            assert!(internal_arc.in_flight.load(Ordering::Acquire) >= 1);
            Ok::<_, DispatchError>(())
        },
    )
    .await;

    assert_eq!(status, 0);
    assert!(
        !missed.load(Ordering::SeqCst),
        "on_handle_miss must not fire on the happy path"
    );
    assert_eq!(invoked.load(Ordering::Acquire), 1);
    assert_eq!(
        internal.in_flight.load(Ordering::Acquire),
        0,
        "counter must return to zero after the verb"
    );
    let events = take_recorded(&sink);
    let completes = completes(&events);
    assert_eq!(completes.len(), 1, "exactly one Complete event");
    let data = &completes[0];
    assert_eq!(data.status, 0);
    assert_eq!(data.error.error_code, 0);
    assert!(data.error.message.is_empty());
    assert!(data.error.trace_locations.is_empty());
    handle::unregister(handle_value);
}

#[tokio::test]
async fn verb_error_completes_with_error_code() {
    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal.clone());
    let (sink, callback) = recording_callback();
    let missed = Arc::new(AtomicBool::new(false));
    let missed_setter = missed.clone();
    let status = revision_tree_call(
        LoreGlobalArgs::default(),
        callback,
        handle_value,
        (),
        "verb_error_test",
        move |_args: &()| missed_setter.store(true, Ordering::SeqCst),
        move |_internal, _args: ()| async move {
            Err::<(), _>(DispatchError::from(InvalidArguments {
                reason: "simulated verb error".into(),
            }))
        },
    )
    .await;
    assert!(
        !missed.load(Ordering::SeqCst),
        "on_handle_miss must not fire when the handle resolves"
    );
    assert_eq!(
        internal.in_flight.load(Ordering::Acquire),
        0,
        "counter must return to zero even on error"
    );

    let events = take_recorded(&sink);
    let completes = completes(&events);
    assert_eq!(completes.len(), 1, "exactly one Complete event");

    let expected = DispatchError::from(InvalidArguments {
        reason: "simulated verb error".into(),
    });
    let expected_code = expected.ffi_code();
    assert_eq!(status, expected_code);
    let data = &completes[0];
    assert_eq!(data.status, expected_code);
    assert_eq!(data.error.error_code, expected_code);
    assert_eq!(data.error.message.as_str(), expected.to_string());
    handle::unregister(handle_value);
}

/// A State failure fails an entry with the failure's own code: `SLOW_DOWN` for a `SlowDown`,
/// `NODE_NOT_FOUND` for a `NodeNotFound`, `INTERNAL` for an internal failure. An entry that has not
/// failed reports no code.
#[test]
fn a_state_failure_fails_an_entry_with_its_own_code() {
    let slowed = EntryFailure::of(&StateError::from(SlowDown));
    assert_eq!(slowed, EntryFailure::SLOWED_DOWN);
    assert!(slowed.failed());
    assert_eq!(slowed.code(), LoreErrorCode::SlowDown);
    for (error, code) in [
        (
            StateError::internal("a block that cannot be read"),
            LoreErrorCode::Internal,
        ),
        (StateError::from(NodeNotFound), LoreErrorCode::NodeNotFound),
        (
            StateError::from(NotAuthorized),
            LoreErrorCode::NotAuthorized,
        ),
    ] {
        let failure = EntryFailure::of(&error);
        assert!(failure.failed());
        assert_eq!(failure.code(), code, "{error}");
    }
    assert_eq!(EntryFailure::INTERNAL.code(), LoreErrorCode::Internal);
    assert!(!EntryFailure::NONE.failed());
    assert_eq!(EntryFailure::NONE.code(), LoreErrorCode::None);
}

/// Two failures of one entry, from two tasks or two nodes of its subtree, merge to the stronger in
/// either order: any failure other than a `SlowDown` outranks one, which outranks none, and of two
/// of one rank the first is kept. The failure takes a byte.
#[test]
fn an_entry_failing_twice_keeps_the_stronger_failure() {
    let ranked = [
        EntryFailure::NONE,
        EntryFailure::SLOWED_DOWN,
        EntryFailure::INTERNAL,
    ];
    for (first_rank, first) in ranked.iter().enumerate() {
        for (second_rank, second) in ranked.iter().enumerate() {
            let mut merged = *first;
            merged.merge(*second);
            assert_eq!(
                merged,
                ranked[first_rank.max(second_rank)],
                "{first:?} then {second:?}"
            );
        }
    }

    let not_found = EntryFailure::of(&StateError::from(NodeNotFound));
    let refused = EntryFailure::of(&StateError::from(NotAuthorized));
    let mut merged = EntryFailure::SLOWED_DOWN;
    merged.merge(not_found);
    assert_eq!(merged.code(), LoreErrorCode::NodeNotFound);
    merged.merge(refused);
    merged.merge(EntryFailure::INTERNAL);
    assert_eq!(
        merged.code(),
        LoreErrorCode::NodeNotFound,
        "the first of a rank"
    );
    assert!(not_found.outranks(EntryFailure::SLOWED_DOWN));
    assert!(!not_found.outranks(refused));

    assert_eq!(std::mem::size_of::<EntryFailure>(), 1);
}

/// A call's error for a State `SlowDown` is the verb's own `SlowDown`, which the call completes
/// with as `SLOW_DOWN`, the context on its trace; for any other State failure an `Internal` naming
/// the context. Each verb's batch terminal reports the `SlowDown` as `SLOW_DOWN`.
#[test]
fn a_calls_error_keeps_a_state_slow_down_and_takes_any_other_failure_as_internal() {
    let slowed: ModifyError = state_failure(StateError::from(SlowDown), "rewrite the node");
    assert!(slowed.is_slow_down(), "{slowed:?}");
    assert_eq!(slowed.ffi_code(), LoreErrorCode::SlowDown as i32);
    assert!(
        slowed
            .trace()
            .locations()
            .iter()
            .any(|location| location.context() == Some("rewrite the node")),
        "the context is on the trace: {slowed:?}"
    );
    assert_eq!(
        LoreErrorCode::of(&Err::<(), _>(slowed)),
        LoreErrorCode::SlowDown
    );

    let failed: ModifyError = state_failure(StateError::from(NodeNotFound), "rewrite the node");
    assert!(failed.is_internal(), "{failed:?}");
    assert_eq!(failed.ffi_code(), LoreErrorCode::Internal as i32);
    assert!(
        failed.to_string().starts_with("rewrite the node: "),
        "{failed}"
    );

    let moved: MoveError = state_failure(StateError::from(SlowDown), "move the node");
    assert_eq!(
        LoreErrorCode::of(&Err::<(), _>(moved)),
        LoreErrorCode::SlowDown
    );
    let deleted: DeleteError = state_failure(StateError::from(SlowDown), "delete the subtree");
    assert_eq!(
        LoreErrorCode::of(&Err::<(), _>(deleted)),
        LoreErrorCode::SlowDown
    );
}

/// A call whose failed entries all slowed down ends with `SlowDown`, the count of failures on its
/// trace; one with any other failure among them, or where an entry failed but none says how, ends
/// with an `Internal` naming the count.
#[test]
fn a_batch_ends_slowed_down_only_where_every_failure_slowed_down() {
    let slowed: ModifyError = EntryFailure::SLOWED_DOWN.call_error("2/3 node modifies failed");
    assert!(slowed.is_slow_down(), "{slowed:?}");
    assert!(
        slowed
            .trace()
            .locations()
            .iter()
            .any(|location| location.context() == Some("2/3 node modifies failed")),
        "the count is on the trace: {slowed:?}"
    );
    for strongest in [EntryFailure::INTERNAL, EntryFailure::NONE] {
        let failed: ModifyError = strongest.call_error("2/3 node modifies failed");
        assert!(failed.is_internal(), "{strongest:?}: {failed:?}");
        assert_eq!(failed.to_string(), "2/3 node modifies failed");
    }
}
