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
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
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
            Self::InvalidArguments(_) => LoreError::InvalidArguments,
            Self::Internal(_) => LoreError::Internal,
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
/// 2. Acquires a [`RevisionTreeGuard`] for the handle — if the handle is
///    unknown or already closed, emits `LORE_EVENT_ERROR` +
///    `Complete{status:1}` and returns `1` without invoking the verb
///    impl.
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
#[allow(dead_code)] // Wired by per-verb modules.
pub(crate) async fn revision_tree_call<Arg, T, F, Fut, ResT, ErrT>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    handle: LoreRevisionTree,
    args: Arg,
    caller: T,
    command: F,
) -> i32
where
    ErrT: EventError,
    Arg: std::fmt::Debug,
    F: FnOnce(Arc<RevisionTreeInternal>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let Some(guard) = RevisionTreeGuard::enter(handle) else {
                let err = DispatchError::from(InvalidArguments {
                    reason: "revision tree handle is unknown or has been closed".into(),
                });
                execution_context().dispatcher.send_error(err);
                execution_context().dispatcher.complete(1).await;
                return 1;
            };

            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let internal = guard.internal_clone();
            let status = match command(internal, args).await {
                Ok(_) => 0,
                Err(err) => {
                    execution_context().dispatcher.send_error(err);
                    1
                }
            };

            log_command_done(&caller, time_start);
            execution_context().dispatcher.complete(status).await;
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

    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;

    use super::*;
    use crate::revision_tree::handle;
    use crate::revision_tree::handle::test_support;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error,
        Complete(i32),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(_) => Self::Error,
                LoreEvent::Complete(data) => Self::Complete(data.status),
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    #[tokio::test]
    async fn handle_miss_emits_error_and_completes_with_status_one() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = revision_tree_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            LoreRevisionTree::INVALID,
            (),
            "handle_miss_test",
            |_internal, _args: ()| async move { Ok::<_, DispatchError>(()) },
        )
        .await;
        assert_eq!(status, 1);
        let events = sink.lock().unwrap().clone();
        assert!(
            events.contains(&CapturedEvent::Error),
            "missing Error event, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(1)),
            "Complete event must carry status=1 matching returned value, got {events:?}"
        );
    }

    #[tokio::test]
    async fn happy_path_completes_with_status_zero_and_decrements_counter() {
        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);

        let invoked = Arc::new(AtomicU64::new(0));
        let invoked_clone = invoked.clone();

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = revision_tree_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            handle_value,
            (),
            "happy_path_test",
            move |internal_arc, _args: ()| async move {
                invoked_clone.fetch_add(1, Ordering::AcqRel);
                assert!(internal_arc.in_flight.load(Ordering::Acquire) >= 1);
                Ok::<_, DispatchError>(())
            },
        )
        .await;

        assert_eq!(status, 0);
        assert_eq!(invoked.load(Ordering::Acquire), 1);
        assert_eq!(
            internal.in_flight.load(Ordering::Acquire),
            0,
            "counter must return to zero after the verb"
        );
        let events = sink.lock().unwrap().clone();
        assert!(
            events.contains(&CapturedEvent::Complete(0)),
            "Complete event must carry status=0 matching returned value, got {events:?}"
        );
        handle::unregister(handle_value);
    }

    #[tokio::test]
    async fn verb_error_emits_error_and_completes_with_status_one() {
        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = revision_tree_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            handle_value,
            (),
            "verb_error_test",
            move |_internal, _args: ()| async move {
                Err::<(), _>(DispatchError::from(InvalidArguments {
                    reason: "simulated verb error".into(),
                }))
            },
        )
        .await;
        assert_eq!(status, 1);
        assert_eq!(
            internal.in_flight.load(Ordering::Acquire),
            0,
            "counter must return to zero even on error"
        );
        let events = sink.lock().unwrap().clone();
        assert!(events.contains(&CapturedEvent::Error));
        assert!(
            events.contains(&CapturedEvent::Complete(1)),
            "Complete event must carry status=1 matching returned value, got {events:?}"
        );
        handle::unregister(handle_value);
    }
}
