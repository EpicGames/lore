// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_close` — release a handle acquired via
//! `lore_revision_tree_load`. Drain semantics mirror `lore_storage_close`:
//! unregister, mark invalid, await the in-flight counter, flush the stores of a
//! closed storage handle, drop.

use std::sync::atomic::Ordering;
use std::sync::atomic::fence;

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeCloseCompleteEventData;
use lore_revision::interface::LoreError;
use lore_revision::lore::execution_context;
use lore_storage::FlushOptions;
use lore_storage::StorageError;
use tokio::sync::oneshot;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::handle;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::storage::close::claims_for_flush;
use crate::storage::close::spawn_flush;
use crate::storage::store::StoreInternal;

/// Arguments for `lore_revision_tree_close`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(close_impl)]
pub struct LoreRevisionTreeCloseArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Revision-tree handle to release
    pub handle: LoreRevisionTree,
    /// Non-zero to release the handle without flushing the stores of a closed storage handle.
    /// What no flush has written is lost once nothing holds the stores.
    pub skip_flush: u8,
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
///
/// The handle is unregistered before the drain, so `handle::lookup` refuses new
/// ops immediately; ops that already took the handle hold their own `Arc` and the
/// drain waits them out. A tree whose storage handle closed first is flushed as
/// [`spawn_flush_after_storage_close`] describes, unless `skip_flush` is set, and
/// the close returns the flush's result.
pub async fn close(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCloseArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, close_impl).await
}

fn close_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCloseArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    no_repository_call(globals, callback, args, close, async move |args| {
        let closed = close_tree(args.handle, args.skip_flush != 0).await;
        LoreEvent::RevisionTreeCloseComplete(LoreRevisionTreeCloseCompleteEventData {
            id: args.id,
            error_code: close_error_code(&closed),
        })
        .send();
        closed
    })
}

/// Unregisters and drains the tree, then, unless `skip_flush`, flushes the stores of a closed
/// storage handle.
async fn close_tree(tree: LoreRevisionTree, skip_flush: bool) -> Result<(), CloseError> {
    let Some(internal) = handle::unregister(tree) else {
        return Err(CloseError::from(InvalidArguments {
            reason: "revision tree handle is unknown or has been closed".into(),
        }));
    };
    internal.mark_invalid_and_await().await;
    let flushed = if skip_flush {
        None
    } else {
        spawn_flush_after_storage_close(&internal.store_internal, || {
            execution_context().globals().flush_options()
        })
    };
    drop(internal);
    match flushed {
        Some(flushed) => flushed
            .await
            .internal("awaiting the flush of the closed storage handle's stores")?
            .forward_any::<CloseError>("flushing the closed storage handle's stores"),
        None => Ok(()),
    }
}

/// Returns `None` while the tree's storage handle is open: that handle's close flushes the
/// stores, unless it skips the flush. Otherwise starts the flush as [`spawn_flush`] does, with
/// the options `options` returns, under claims joined while the tree still holds the handle's.
/// Writes no flush has written keep the store's lock while anything holds the stores, and are
/// lost once nothing does.
///
/// A store-buffering shape decides whether the tree's writes are flushed: they set the stores'
/// `dirty` flags, `Relaxed`, before this reads the handle's `invalid`, and the handle's close sets
/// `invalid` before its flush reads the flags, also `Relaxed`. With nothing more, both reads can
/// miss the other side's store: this starts no flush, and the handle's flush finds the flags
/// clear, so nothing flushes what the tree wrote. A `SeqCst` fence before this read, and the one
/// [`spawn_flush`] runs before its flush reads the flags, rule that out: of the two sides, the one
/// whose fence comes second in the fences' total order reads the other's store or a later one.
pub(crate) fn spawn_flush_after_storage_close(
    store: &StoreInternal,
    options: impl FnOnce() -> FlushOptions,
) -> Option<oneshot::Receiver<Result<(), StorageError>>> {
    fence(Ordering::SeqCst);
    if !store.invalid.load(Ordering::Relaxed) {
        return None;
    }
    Some(spawn_flush(store, claims_for_flush(store), options()))
}

/// The code `LORE_EVENT_REVISION_TREE_CLOSE_COMPLETE` reports for a finished close.
fn close_error_code(closed: &Result<(), CloseError>) -> LoreErrorCode {
    match closed {
        Ok(()) => LoreErrorCode::None,
        Err(CloseError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(CloseError::Internal(_)) => LoreErrorCode::Internal,
    }
}
