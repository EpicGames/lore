// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Dispatch helper for the low-level memory-based revision control API.
//!
//! `revision_tree_call` mirrors [`crate::storage::call::storage_call`] but
//! looks up a [`crate::revision_tree::handle::RevisionTreeInternal`]
//! instead of a `StoreInternal`. Every revision-tree verb goes through
//! this helper so the in-flight counter protocol and the `Complete` /
//! `End` event lifecycle apply uniformly.

use std::sync::Arc;
use std::time::Instant;

use lore_base::error::InvalidArguments;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::FfiError;
use lore_error_set::HasTrace;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorDetail;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::execution_context;

use crate::call::setup_execution;
use crate::interface::LoreEventCallback;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeGuard;
use crate::revision_tree::handle::RevisionTreeInternal;
use crate::util::log_command_done;
use crate::util::log_command_info;

/// Errors emitted by the dispatch helper itself (not by the verb impl).
#[error_set]
enum DispatchError {
    InvalidArguments,
}

impl EventError for DispatchError {
    fn translated(&self) -> LoreError {
        match self {
            DispatchError::InvalidArguments(_) => LoreError::InvalidArguments,
            DispatchError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Run a revision-tree verb behind the in-flight counter protocol.
///
/// The helper:
/// 1. Sets up an `ExecutionContext` and enters its `LORE_CONTEXT` scope.
/// 2. Acquires a [`RevisionTreeGuard`] for the handle. If the handle is
///    unknown or already closed, invokes `on_handle_miss` (letting the verb
///    emit its own `*Complete` terminal carrying the caller id), then
///    completes with the handle-miss error detail and returns its error code
///    without invoking the verb impl.
/// 3. Passes a cloned `Arc<RevisionTreeInternal>` to the verb impl
///    (ownership transferred; the impl can fan it out to spawned tasks).
/// 4. Translates the impl's `Result` into a `Complete{status}` event.
/// 5. Drops the `RevisionTreeGuard` only *after* `Complete` fires — so
///    the in-flight counter decrement orders after the last result event.
///
/// # Contract expected of `command`
///
/// All work (including spawned tasks) the verb initiates must complete
/// before the returned future resolves — no background work outlives a
/// data verb. Use `JoinSet` / `join_all` to await spawned futures before
/// returning.
pub(crate) async fn revision_tree_call<Arg, T, F, Fut, ResT, ErrT, M>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    handle: LoreRevisionTree,
    args: Arg,
    caller: T,
    on_handle_miss: M,
    command: F,
) -> i32
where
    ErrT: EventError + FfiError + HasTrace,
    Arg: std::fmt::Debug,
    M: FnOnce(),
    F: FnOnce(Arc<RevisionTreeInternal>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let Some(guard) = RevisionTreeGuard::enter(handle) else {
                on_handle_miss();
                let err = DispatchError::from(InvalidArguments {
                    reason: "revision tree handle is unknown or has been closed".into(),
                });
                return execution_context()
                    .dispatcher
                    .complete(LoreErrorDetail::from_error(&err))
                    .await;
            };

            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let internal = guard.internal_clone();
            let detail = LoreErrorDetail::from_result(command(internal, args).await);

            log_command_done(&caller, time_start);
            let status = execution_context().dispatcher.complete(detail).await;
            // Explicit drop after Complete: a closer waiting on the in-flight counter must
            // not be woken before Complete has fired.
            drop(guard);
            status
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::interface::LoreGlobalArgs;

    use super::*;
    use crate::call::test_support::CapturedEvent;
    use crate::call::test_support::completes;
    use crate::call::test_support::has_error_event;
    use crate::call::test_support::make_callback;
    use crate::revision_tree::handle;
    use crate::revision_tree::handle::test_support;

    #[tokio::test]
    async fn handle_miss_completes_with_error_code_and_no_error_event() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let missed = Arc::new(AtomicBool::new(false));
        let missed_setter = missed.clone();
        let status = revision_tree_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            LoreRevisionTree::INVALID,
            (),
            "handle_miss_test",
            move || missed_setter.store(true, Ordering::SeqCst),
            |_internal, _args: ()| async move { Ok::<_, DispatchError>(()) },
        )
        .await;
        assert!(
            missed.load(Ordering::SeqCst),
            "on_handle_miss must fire when the handle is unknown"
        );
        let events = sink.lock().unwrap().clone();
        assert!(
            !has_error_event(&events),
            "no Error event must be emitted on terminal failure"
        );

        let completes = completes(&events);
        assert_eq!(completes.len(), 1, "exactly one Complete event");

        // The status holds the handle-miss error's real error code and the detail
        // carries the same code and message.
        let expected = DispatchError::from(InvalidArguments {
            reason: "revision tree handle is unknown or has been closed".into(),
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
        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);

        let invoked = Arc::new(AtomicU64::new(0));
        let invoked_clone = invoked.clone();

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let missed = Arc::new(AtomicBool::new(false));
        let missed_setter = missed.clone();
        let status = revision_tree_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            handle_value,
            (),
            "happy_path_test",
            move || missed_setter.store(true, Ordering::SeqCst),
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
        let events = sink.lock().unwrap().clone();
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
    async fn verb_error_completes_with_error_code_and_no_error_event() {
        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let missed = Arc::new(AtomicBool::new(false));
        let missed_setter = missed.clone();
        let status = revision_tree_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            handle_value,
            (),
            "verb_error_test",
            move || missed_setter.store(true, Ordering::SeqCst),
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

        let events = sink.lock().unwrap().clone();
        assert!(
            !has_error_event(&events),
            "no Error event must be emitted on terminal failure"
        );

        let completes = completes(&events);
        assert_eq!(completes.len(), 1, "exactly one Complete event");

        // The status holds the verb error's real error code and the detail carries
        // the same code and message.
        let expected = DispatchError::from(InvalidArguments {
            reason: "simulated verb error".into(),
        });
        let expected_code = expected.ffi_code();
        // The synchronous return equals the error code, matching `Complete.status`.
        assert_eq!(status, expected_code);
        let data = &completes[0];
        assert_eq!(data.status, expected_code);
        assert_eq!(data.error.error_code, expected_code);
        assert_eq!(data.error.message.as_str(), expected.to_string());
        handle::unregister(handle_value);
    }
}
