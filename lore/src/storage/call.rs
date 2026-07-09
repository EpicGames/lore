// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Dispatch helper for the content-addressed storage API.
//!
//! `storage_call` mirrors `crate::call::repository_call` but without the
//! repository working-tree checks. Every op goes through this helper so
//! the in-flight counter protocol and the `Complete` / `End` event
//! lifecycle are applied uniformly.

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
use crate::storage::handle::LoreStore;
use crate::storage::store::OpGuard;
use crate::storage::store::StoreInternal;
use crate::util::log_command_done;
use crate::util::log_command_info;

/// Errors emitted by the dispatch helper itself (not by the op impl).
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

/// Run a storage-API op behind the in-flight counter protocol.
///
/// The helper:
/// 1. Sets up an `ExecutionContext` and enters its `LORE_CONTEXT` scope.
/// 2. Acquires an [`OpGuard`] for the handle — if the handle is unknown
///    or already closed, completes with the handle-miss error detail and
///    returns its error code without invoking the op impl.
/// 3. Passes a cloned `Arc<StoreInternal>` to the op impl (ownership
///    transferred; the impl can fan it out to spawned tasks).
/// 4. Translates the impl's `Result` into a `Complete{status}` event.
/// 5. Drops the `OpGuard` only *after* `Complete` fires — so the
///    in-flight counter decrement orders after the last result event.
///
/// # Contract expected of `command`
///
/// All work (including spawned tasks) the op initiates must complete
/// before the returned future resolves — no background work outlives
/// a data op. Use `JoinSet` / `join_all` to await spawned futures
/// before returning.
pub(crate) async fn storage_call<Arg, T, F, Fut, ResT, ErrT>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    store_handle: LoreStore,
    args: Arg,
    caller: T,
    command: F,
) -> i32
where
    ErrT: EventError + FfiError + HasTrace,
    Arg: std::fmt::Debug,
    F: FnOnce(Arc<StoreInternal>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let Some(guard) = OpGuard::enter(store_handle) else {
                let err = DispatchError::from(InvalidArguments {
                    reason: "storage handle is unknown or has been closed".into(),
                });
                return execution_context()
                    .dispatcher
                    .complete(LoreErrorDetail::from_error(&err))
                    .await;
            };

            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let store = guard.store_clone();
            let detail = LoreErrorDetail::from_result(command(store, args).await);

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
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::interface::LoreGlobalArgs;

    use super::*;
    use crate::call::test_support::CapturedEvent;
    use crate::call::test_support::completes;
    use crate::call::test_support::has_error_event;
    use crate::call::test_support::make_callback;
    use crate::storage::handle;
    use crate::storage::store::StoreInternal;
    use crate::storage::store::in_memory_for_tests;

    async fn register_test_store() -> (Arc<StoreInternal>, LoreStore) {
        let store = in_memory_for_tests("call-test").await;
        let store_handle = handle::register(store.clone());
        (store, store_handle)
    }

    #[tokio::test]
    async fn handle_miss_completes_with_error_code_and_no_error_event() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = storage_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            LoreStore::INVALID,
            (),
            "handle_miss_test",
            |_store, _args: ()| async move { Ok::<_, DispatchError>(()) },
        )
        .await;

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
        let (store, store_handle) = register_test_store().await;
        assert_eq!(store.in_flight.load(Ordering::Acquire), 0);

        let invoked = Arc::new(AtomicU64::new(0));
        let invoked_clone = invoked.clone();

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = storage_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            store_handle,
            (),
            "happy_path_test",
            move |store_arc, _args: ()| async move {
                invoked_clone.fetch_add(1, Ordering::AcqRel);
                assert!(store_arc.in_flight.load(Ordering::Acquire) >= 1);
                Ok::<_, DispatchError>(())
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
        let events = sink.lock().unwrap().clone();
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
        let (store, store_handle) = register_test_store().await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = storage_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            store_handle,
            (),
            "op_error_test",
            move |_store, _args: ()| async move {
                Err::<(), _>(DispatchError::from(InvalidArguments {
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

        let events = sink.lock().unwrap().clone();
        assert!(
            !has_error_event(&events),
            "no Error event must be emitted on terminal failure"
        );

        let completes = completes(&events);
        assert_eq!(completes.len(), 1, "exactly one Complete event");

        // The status holds the op error's real error code and the detail carries
        // the same code and message.
        let expected = DispatchError::from(InvalidArguments {
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
}
