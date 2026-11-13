// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_close` — release a handle acquired via `lore_storage_open`.
//!
//! Sequence:
//! 1. Atomically remove the handle from the registry. Any subsequent `op_enter` against the same
//!    handle returns `None` → the op rejects with `InvalidArguments`.
//! 2. Mark the store invalid and await the in-flight counter → 0. In-flight ops that were past
//!    `op_enter` at step 1 run to completion; new ops bounce off the invalid flag.
//! 3. Join the handle's store claims. In a process that retains stores, keep disk-backed stores
//!    for the keep-alive window without claims, so an open of the same repository in that window
//!    reuses them.
//! 4. Unless `skip_flush` is set, flush the stores under the joined claims, as [`spawn_flush`]
//!    describes, honoring `globals.sync_data` and `globals.fast_flush`.
//! 5. Release the handle's state and await the flush's result.
//!
//! `Complete` fires once the flush has finished, with the flush's error if it failed. The handle is
//! released either way, and a close dropped once its flush has started leaves it to run to its end.

use std::sync::Arc;

use futures::FutureExt;
use lore_base::error::InvalidArguments;
use lore_base::lore_spawn_guarded;
use lore_base::lore_warn;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::interface::ExecutionContext;
use lore_revision::lore::execution_context;
use lore_storage::FlushOptions;
use lore_storage::StorageError;
use lore_storage::local::store_lock::StoreHold;
use tokio::sync::oneshot;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::flush::flush_stores;
use crate::storage::handle;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// Joins the store claims a closing handle holds, for the flush that outlives it.
///
/// Taken while the handle still holds its own, so each joins a claim already held. The
/// handle's claims end with its state, which close does not keep for its flush: without claims
/// of its own, a store holding only access stamps would release its flock before the flush
/// ran, and another process writing in that gap would have the flush reload the stamps away. A
/// claim that cannot be joined is left out, since the flush claims what it writes regardless.
#[lore_macro::test_pub]
pub(crate) fn claims_for_flush(store: &StoreInternal) -> StoreHold {
    StoreHold::new(
        [
            store.immutable.try_join_claim(),
            store.mutable.try_join_claim(),
        ]
        .into_iter()
        .flatten()
        .collect(),
    )
}

/// Flushes `store`'s stores in a task shutdown waits for, holding `claims` until it is done — see
/// [`claims_for_flush`] — and sends the result to the returned receiver. The task waits for the
/// immutable store's eviction and compaction passes in flight to give up and for a verification
/// in flight to end, then flushes the stores through [`flush_stores`] as `options` asks; in-memory
/// stores no-op. A failed flush no receiver takes is logged. Runs without the caller's execution
/// context.
pub(crate) fn spawn_flush(
    store: &StoreInternal,
    claims: StoreHold,
    options: FlushOptions,
) -> oneshot::Receiver<Result<(), StorageError>> {
    let (sender, receiver) = oneshot::channel();
    let immutable_store = store.immutable.clone();
    let mutable_store = store.mutable.clone();
    let flush = async move {
        immutable_store.clone().stop_gc(false).await;
        let flushed = flush_stores(&immutable_store, &mutable_store, options).await;
        drop(claims);
        flushed
    }
    .map(move |flushed| {
        if let Err(Err(err)) = sender.send(flushed) {
            lore_warn!("Flush of a closed storage handle's stores failed: {err}");
        }
    });
    LORE_CONTEXT.sync_scope(
        Arc::new(ExecutionContext::default()) as Arc<dyn std::any::Any + Send + Sync>,
        || {
            lore_spawn_guarded!(flush);
        },
    );
    receiver
}

/// Keeps a closing handle's stores for the keep-alive window, without claims, in a process that
/// retains stores ([`lore_revision::store::retain_stores`]), so an open of the same repository in
/// that window reuses them. Stores with nothing on disk, for which `claims` holds nothing, are
/// not kept: no open reaches them again.
fn retain_closed_stores(store: &StoreInternal, claims: &StoreHold) {
    if claims.is_empty() || !lore_revision::store::retain_stores() {
        return;
    }
    if let Some(duration) = execution_context().globals().store_keep_alive_duration() {
        lore_revision::repository::keep_stores_alive(
            store.immutable.clone(),
            store.mutable.clone(),
            duration,
        );
    }
}

/// Arguments for `lore_storage_close`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(close_local)]
pub struct LoreStorageCloseArgs {
    /// Handle to release; from `LORE_EVENT_STORAGE_OPENED`
    pub handle: LoreStore,
    /// Non-zero to release the handle without flushing its stores. What the handle, and the
    /// revision trees closed while it was open, wrote that no flush has written is lost once
    /// nothing holds the stores.
    pub skip_flush: u8,
}

/// Release a content-addressed storage handle.
///
/// Subsequent calls against the same handle return `InvalidArguments`. A second `close` on an
/// already-closed handle also returns `InvalidArguments`.
///
/// Revision tree handles loaded against this store are neither closed nor reported: each
/// holds its own reference to the store and stays usable, reads and commits included.
/// Releasing them is the caller's job, with `lore_revision_tree_close`. Background eviction
/// and compaction, where the open enabled them, keep their schedule for as long as anything
/// holds the store.
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

        let claims = claims_for_flush(&store);
        retain_closed_stores(&store, &claims);
        if args.skip_flush != 0 {
            return Ok(());
        }
        // Flush after the drain so the flush sees a quiesced store.
        let options = execution_context().globals().flush_options();
        let flushed = spawn_flush(&store, claims, options);
        drop(store);
        flushed
            .await
            .internal("awaiting the flush of the closed handle's stores")?
    })
}
