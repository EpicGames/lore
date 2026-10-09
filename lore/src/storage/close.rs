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
use lore_macro::LoreArgs;
use lore_revision::interface::ExecutionContext;
use lore_revision::lore::execution_context;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StorageError;

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
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(close_local)]
pub struct LoreStorageCloseArgs {
    /// Handle to release; from `LORE_EVENT_STORAGE_OPENED`
    pub handle: LoreStore,
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

fn close_local(
    globals: LoreGlobalArgs,
    args: LoreStorageCloseArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    no_repository_call(globals, callback, args, close, async move |args| {
        // Unregister first so concurrent `handle::lookup` returns None for new ops; ops that
        // already grabbed the handle still hold their `Arc` and the drain below waits them out.
        let Some(store) = handle::unregister(args.handle) else {
            return Err(StorageError::from(InvalidArguments {
                reason: "storage handle is unknown or already closed".into(),
            }));
        };

        store.mark_invalid_and_await().await;

        // Spawn flush after the drain so it sees a quiesced store.
        let sync_data = execution_context().globals().sync_data();
        spawn_flush_stores(store.immutable.clone(), store.mutable.clone(), sync_data);

        Ok::<_, StorageError>(())
    })
}
