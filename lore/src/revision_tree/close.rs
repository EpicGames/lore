// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_close` — release a handle acquired via
//! `lore_revision_tree_load`. Drain semantics mirror `lore_storage_close`:
//! unregister, mark invalid, await the in-flight counter, drop.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeCloseCompleteEventData;
use lore_revision::interface::LoreError;
use serde::Deserialize;
use serde::Serialize;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::handle;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_close`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(close_impl)]
pub struct LoreRevisionTreeCloseArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Revision-tree handle to release
    pub handle: LoreRevisionTree,
}

#[error_set]
enum CloseError {
    InvalidArguments,
}

impl EventError for CloseError {
    fn translated(&self) -> LoreError {
        match self {
            CloseError::InvalidArguments(_) => LoreError::InvalidArguments,
            CloseError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Release a memory-based revision tree handle.
///
/// Subsequent calls against the same handle return `InvalidArguments`. A
/// second `close` on an already-closed handle also returns
/// `InvalidArguments`. The call blocks until every in-flight op against the
/// handle has paired its decrement, then drops the underlying
/// `Arc<RevisionTreeInternal>` (which in turn releases the `Arc<StoreInternal>`
/// borrowed from the parent storage handle).
pub async fn close(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCloseArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, close_impl).await
}

async fn close_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCloseArgs,
    callback: LoreEventCallback,
) -> i32 {
    no_repository_call(globals, callback, args, close, async move |args| {
        // Unregister first so concurrent `handle::lookup` returns None for new ops; ops that
        // already grabbed the handle still hold their `Arc` and the drain below waits them out.
        let Some(internal) = handle::unregister(args.handle) else {
            LoreEvent::RevisionTreeCloseComplete(LoreRevisionTreeCloseCompleteEventData {
                id: args.id,
                error_code: LoreErrorCode::InvalidArguments,
            })
            .send();
            return Err(CloseError::from(InvalidArguments {
                reason: "revision tree handle is unknown or has been closed".into(),
            }));
        };

        internal.mark_invalid_and_await().await;

        LoreEvent::RevisionTreeCloseComplete(LoreRevisionTreeCloseCompleteEventData {
            id: args.id,
            error_code: LoreErrorCode::None,
        })
        .send();

        Ok::<_, CloseError>(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::handle::RevisionTreeGuard;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error(u32),
        Complete(i32),
        RevisionTreeLoaded(u64),
        RevisionTreeCloseComplete(u64, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeCloseComplete(data) => {
                    Self::RevisionTreeCloseComplete(data.id, data.error_code)
                }
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    async fn load_handle(label: &str, repository: Partition) -> (LoreRevisionTree, u64) {
        let store = in_memory_for_tests(label).await;
        let store_handle = storage_handle::register(store);
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository,
                revision_hash: Hash::default(),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "load fixture must succeed");
        let id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                CapturedEvent::RevisionTreeLoaded(id) => Some(*id),
                _ => None,
            })
            .expect("load fixture must emit RevisionTreeLoaded");
        (LoreRevisionTree { handle_id: id }, store_handle.handle_id)
    }

    #[tokio::test]
    async fn close_releases_handle_and_subsequent_close_returns_invalid_arguments() {
        let (handle_value, store_handle_id) =
            load_handle("close-releases", Partition::from([0x11u8; 16])).await;

        let sink_first: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status_first = close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 42,
                handle: handle_value,
            },
            make_callback(sink_first.clone()),
        )
        .await;
        assert_eq!(status_first, 0, "first close must succeed");
        let events_first = sink_first.lock().unwrap().clone();
        assert!(
            events_first.contains(&CapturedEvent::RevisionTreeCloseComplete(
                42,
                LoreErrorCode::None
            )),
            "first close must emit RevisionTreeCloseComplete with the caller id, got {events_first:?}"
        );
        assert!(
            events_first.contains(&CapturedEvent::Complete(0)),
            "first close must complete with status=0, got {events_first:?}"
        );
        let close_complete_pos = events_first
            .iter()
            .position(|e| matches!(e, CapturedEvent::RevisionTreeCloseComplete(_, _)))
            .expect("RevisionTreeCloseComplete must be present");
        let complete_pos = events_first
            .iter()
            .position(|e| matches!(e, CapturedEvent::Complete(_)))
            .expect("Complete must be present");
        assert!(
            close_complete_pos < complete_pos,
            "RevisionTreeCloseComplete must fire before Complete, got {events_first:?}"
        );
        assert!(
            rt_handle::lookup(handle_value).is_none(),
            "handle must be unregistered after close",
        );

        let sink_second: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status_second = close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 43,
                handle: handle_value,
            },
            make_callback(sink_second.clone()),
        )
        .await;
        assert_eq!(
            status_second, 1,
            "second close on the same handle must fail"
        );
        let events_second = sink_second.lock().unwrap().clone();
        // A failed close signals its error through the close-complete event's
        // error code and the terminal `Complete` status, not a separate `Error`.
        assert!(
            events_second.contains(&CapturedEvent::RevisionTreeCloseComplete(
                43,
                LoreErrorCode::InvalidArguments
            )),
            "second close must emit RevisionTreeCloseComplete with InvalidArguments, got {events_second:?}"
        );
        assert!(
            events_second.contains(&CapturedEvent::Complete(1)),
            "second close must complete with status=1, got {events_second:?}"
        );

        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn close_with_unknown_handle_returns_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 7,
                handle: LoreRevisionTree::INVALID,
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 1, "close on INVALID must fail with status=1");
        let events = sink.lock().unwrap().clone();
        assert!(
            events.contains(&CapturedEvent::RevisionTreeCloseComplete(
                7,
                LoreErrorCode::InvalidArguments
            )),
            "close on INVALID must emit RevisionTreeCloseComplete with InvalidArguments, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(1)),
            "close on INVALID must complete with status=1, got {events:?}"
        );
    }

    #[tokio::test]
    async fn close_with_unknown_handle_emits_close_complete_with_invalid_arguments_error_code() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 4242,
                handle: LoreRevisionTree::INVALID,
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 1, "close on INVALID must fail with status=1");
        let events = sink.lock().unwrap().clone();
        assert!(
            events.contains(&CapturedEvent::RevisionTreeCloseComplete(
                4242,
                LoreErrorCode::InvalidArguments
            )),
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

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_clone = sink.clone();
        let close_task = tokio::spawn(async move {
            close(
                LoreGlobalArgs::default(),
                LoreRevisionTreeCloseArgs {
                    id: 99,
                    handle: handle_value,
                },
                make_callback(sink_clone),
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
        let events = sink.lock().unwrap().clone();
        assert!(
            events.contains(&CapturedEvent::RevisionTreeCloseComplete(
                99,
                LoreErrorCode::None
            )),
            "drained close must emit RevisionTreeCloseComplete, got {events:?}"
        );

        storage_handle::unregister(crate::storage::handle::LoreStore {
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
            [0, 1],
            "exactly one close must succeed (0) and one must fail (1), got {statuses:?}"
        );
        assert!(
            rt_handle::lookup(handle_value).is_none(),
            "handle must be unregistered after the race",
        );

        storage_handle::unregister(crate::storage::handle::LoreStore {
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

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let load_status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository: Partition::from([0x66u8; 16]),
                revision_hash: Hash::default(),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(load_status, 0, "load must succeed");
        let handle_id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                CapturedEvent::RevisionTreeLoaded(id) => Some(*id),
                _ => None,
            })
            .expect("load must emit RevisionTreeLoaded");
        let tree_handle = LoreRevisionTree { handle_id };

        let close_status = close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs {
                id: 1,
                handle: tree_handle,
            },
            None,
        )
        .await;
        assert_eq!(close_status, 0, "revision tree close must succeed");
        assert!(
            rt_handle::lookup(tree_handle).is_none(),
            "revision tree handle must be deregistered after close",
        );

        let storage_close_status = crate::storage::close::close(
            LoreGlobalArgs::default(),
            crate::storage::close::LoreStorageCloseArgs {
                handle: store_handle,
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

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let load_status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository: Partition::from([0x77u8; 16]),
                revision_hash: Hash::default(),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(load_status, 0, "load must succeed");
        let handle_id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                CapturedEvent::RevisionTreeLoaded(id) => Some(*id),
                _ => None,
            })
            .expect("load must emit RevisionTreeLoaded");
        let tree_handle = LoreRevisionTree { handle_id };

        let storage_close_status = crate::storage::close::close(
            LoreGlobalArgs::default(),
            crate::storage::close::LoreStorageCloseArgs {
                handle: store_handle,
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
            },
            None,
        )
        .await;
        assert_eq!(close_status, 0, "revision tree close must still succeed");
    }
}
