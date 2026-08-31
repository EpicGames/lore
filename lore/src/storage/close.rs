// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_close` — release a handle acquired via `lore_storage_open`.
//!
//! Sequence:
//! 1. Atomically remove the handle from the registry. Any subsequent `op_enter` against the same
//!    handle returns `None` → the op rejects with `InvalidArguments`.
//! 2. Mark the store invalid and await the in-flight counter → 0. In-flight ops that were past
//!    `op_enter` at step 1 run to completion; new ops bounce off the invalid flag.
//! 3. Spawn a fire-and-forget flush task that calls `immutable.flush()` + `mutable.flush()`. For
//!    in-memory stores these are no-ops; for disk-backed they honor `globals.sync_data`.
//!
//! Close does not block on step 3. `Complete` fires after steps 1 and 2; the flush task outlives
//! the call — this is the only place where background work outlives a storage op.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn_guarded;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreError;
use lore_revision::lore::execution_context;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use serde::Deserialize;
use serde::Serialize;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::handle;
use crate::storage::handle::LoreStore;

/// Fire-and-forget flush of a closing handle's stores. Runs without the caller's execution
/// context; errors go to void. Disk-backed stores honor `sync_data`; in-memory stores no-op.
pub(crate) fn spawn_flush_stores(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    sync_data: bool,
) {
    LORE_CONTEXT.sync_scope(
        Arc::new(ExecutionContext::default()) as Arc<dyn std::any::Any + Send + Sync>,
        || {
            lore_spawn_guarded!(async move {
                immutable_store.clone().stop_gc(false).await;
                let _ = immutable_store.flush(sync_data).await;
                let _ = mutable_store.flush(sync_data).await;
            });
        },
    );
}

/// Arguments for `lore_storage_close`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(close_local)]
pub struct LoreStorageCloseArgs {
    /// Handle to release; from `LORE_EVENT_STORAGE_OPENED`
    pub handle: LoreStore,
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

/// Release a content-addressed storage handle.
///
/// Subsequent calls against the same handle return `InvalidArguments`. A second `close` on an
/// already-closed handle also returns `InvalidArguments`.
///
/// Revision tree handles loaded against this store are neither closed nor reported: each
/// holds its own reference to the store and stays usable, reads and commits included.
/// Releasing them is the caller's job, with `lore_revision_tree_close`. Background eviction
/// and compaction stop here and do not restart, so a tree that keeps writing afterwards
/// runs without cache-size enforcement.
pub async fn close(
    globals: LoreGlobalArgs,
    args: LoreStorageCloseArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, close_local).await
}

async fn close_local(
    globals: LoreGlobalArgs,
    args: LoreStorageCloseArgs,
    callback: LoreEventCallback,
) -> i32 {
    no_repository_call(globals, callback, args, close, async move |args| {
        // Unregister first so concurrent `handle::lookup` returns None for new ops; ops that
        // already grabbed the handle still hold their `Arc` and the drain below waits them out.
        let Some(store) = handle::unregister(args.handle) else {
            return Err(CloseError::from(InvalidArguments {
                reason: "storage handle is unknown or already closed".into(),
            }));
        };

        store.mark_invalid_and_await().await;

        // Spawn flush after the drain so it sees a quiesced store.
        let sync_data = execution_context().globals().sync_data();
        spawn_flush_stores(store.immutable.clone(), store.mutable.clone(), sync_data);

        Ok::<_, CloseError>(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreEvent;

    use super::*;
    use crate::revision_tree::handle as tree_handle;
    use crate::revision_tree::handle::LoreRevisionTree;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::storage::store::OpGuard;
    use crate::storage::store::in_memory_for_tests;

    /// Load a revision tree against an already-registered storage handle.
    async fn load_revision_tree(
        store_handle: LoreStore,
        repository: Partition,
    ) -> LoreRevisionTree {
        let loaded: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let sink = loaded.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::RevisionTreeLoaded(data) = event {
                *sink.lock().unwrap() = Some(data.handle_id);
            }
        }));
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository,
                revision_hash: Hash::default(),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "loading the revision tree fixture must succeed");
        let handle_id = loaded
            .lock()
            .unwrap()
            .expect("load must emit RevisionTreeLoaded");
        LoreRevisionTree { handle_id }
    }

    /// Close must block until the in-flight counter drains. An `OpGuard` held by the test keeps
    /// the counter > 0; close's `mark_invalid_and_await` must not complete until the guard
    /// drops.
    #[allow(clippy::disallowed_methods)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_waits_for_in_flight_counter_to_drain() {
        let store = in_memory_for_tests("close-wait-test").await;
        let store_handle = handle::register(store.clone());
        let guard = OpGuard::enter(store_handle).expect("enter must succeed");

        let close_task = tokio::spawn(async move {
            close(
                LoreGlobalArgs::default(),
                LoreStorageCloseArgs {
                    handle: store_handle,
                },
                None,
            )
            .await
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while handle::lookup(store_handle).is_some() {
            if Instant::now() > deadline {
                panic!("close never unregistered the handle");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // The guard pins the in-flight counter at 1, so close must still be parked inside
        // `mark_invalid_and_await`.
        assert!(
            !close_task.is_finished(),
            "close must block while the in-flight counter is non-zero",
        );

        drop(guard);

        let status = close_task.await.expect("close task join");
        assert_eq!(
            status, 0,
            "close should report success after the counter drains"
        );
    }

    /// The store outlives its storage handle for exactly as long as a revision tree still
    /// references it. That reference is what makes reads work after a parent close, so it
    /// has to be the last one dropped, not merely present.
    #[tokio::test]
    async fn the_store_tears_down_only_once_the_last_revision_handle_closes() {
        let store = in_memory_for_tests("close-refcount").await;
        let alive = Arc::downgrade(&store);
        let store_handle = handle::register(store);
        let tree = load_revision_tree(store_handle, Partition::from([0x1Au8; 16])).await;

        let status = close(
            LoreGlobalArgs::default(),
            LoreStorageCloseArgs {
                handle: store_handle,
            },
            None,
        )
        .await;
        assert_eq!(status, 0, "closing the storage handle must succeed");
        assert!(
            alive.upgrade().is_some(),
            "the revision tree's reference must hold the store up",
        );

        let internal = tree_handle::unregister(tree).expect("the tree must still be registered");
        drop(internal);
        assert!(
            alive.upgrade().is_none(),
            "and dropping it must be what finally tears the store down",
        );
    }
}
