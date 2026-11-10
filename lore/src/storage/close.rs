// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_close` — release a handle acquired via `lore_storage_open`.
//!
//! Sequence:
//! 1. Atomically remove the handle from the registry. Any subsequent `op_enter` against the same
//!    handle returns `None` → the op rejects with `InvalidArguments`.
//! 2. Mark the store invalid and await the in-flight counter → 0. In-flight ops that were past
//!    `op_enter` at step 1 run to completion; new ops bounce off the invalid flag.
//! 3. Join the handle's store claims and spawn a fire-and-forget flush task that holds them while
//!    it calls `immutable.flush()` + `mutable.flush()`. For in-memory stores these are no-ops; for
//!    disk-backed they honor `globals.sync_data` and `globals.fast_flush`.
//! 4. In a process that retains stores, keep disk-backed stores for the keep-alive window without
//!    claims, so an open of the same repository in that window reuses them.
//!
//! Close does not block on the flush. `Complete` fires once the claims are joined; the flush task
//! outlives the call, and the claims with it, as all work spawned to outlive a storage op holds
//! claims of its own until it ends.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn_guarded;
use lore_base::runtime::LORE_CONTEXT;
use lore_macro::LoreArgs;
use lore_revision::interface::ExecutionContext;
use lore_revision::lore::execution_context;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StorageError;
use lore_storage::local::store_lock::StoreHold;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
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

/// Fire-and-forget flush of a closing handle's stores, holding `claims` until it is done — see
/// [`claims_for_flush`]. Stops the stores' garbage collection first. Runs without the caller's
/// execution context; errors go to void. Disk-backed stores flush as `options` asks; in-memory
/// stores no-op.
pub(crate) fn spawn_flush_stores(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    claims: StoreHold,
    options: FlushOptions,
) {
    spawn_without_context(async move {
        immutable_store.clone().stop_gc(false).await;
        flush_then_release(immutable_store, mutable_store, claims, options).await;
    });
}

/// Fire-and-forget flush of the stores a revision tree wrote after its storage handle closed,
/// holding `claims` until it is done — see [`claims_for_flush`]. Leaves garbage collection
/// alone: the handle's close already stopped it. Otherwise as [`spawn_flush_stores`].
pub(crate) fn spawn_flush_for_closed_tree(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    claims: StoreHold,
    options: FlushOptions,
) {
    spawn_without_context(flush_then_release(
        immutable_store,
        mutable_store,
        claims,
        options,
    ));
}

/// Flushes both stores, then releases `claims`: only now, since the flushes ran inside them.
async fn flush_then_release(
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    claims: StoreHold,
    options: FlushOptions,
) {
    let _ = immutable_store.flush(options).await;
    let _ = mutable_store.flush(options).await;
    drop(claims);
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

/// Spawns `work` outside the caller's execution context, as a task shutdown waits for.
fn spawn_without_context(work: impl Future<Output = ()> + Send + 'static) {
    LORE_CONTEXT.sync_scope(
        Arc::new(ExecutionContext::default()) as Arc<dyn std::any::Any + Send + Sync>,
        || {
            lore_spawn_guarded!(work);
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
        let options = execution_context().globals().flush_options();
        let claims = claims_for_flush(&store);
        retain_closed_stores(&store, &claims);
        spawn_flush_stores(
            store.immutable.clone(),
            store.mutable.clone(),
            claims,
            options,
        );

        Ok::<_, StorageError>(())
    })
}
