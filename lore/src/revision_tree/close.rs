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
///
/// The handle is unregistered before the drain, so `handle::lookup` refuses new
/// ops immediately; ops that already took the handle hold their own `Arc` and the
/// drain waits them out.
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

        // A tree closed after its storage handle has writes nothing else will flush: the
        // handle's close flushed what had been written by then. Left flagged, they would keep
        // the store's flock past this tree — which may be the last thing claiming it — for as
        // long as the process runs. Flushed under claims joined while the tree still holds the
        // handle's.
        let store = &internal.store_internal;
        if store.invalid.load(std::sync::atomic::Ordering::Acquire) {
            let sync_data = lore_revision::lore::execution_context()
                .globals()
                .sync_data();
            let claims = crate::storage::close::claims_for_flush(store).await;
            crate::storage::close::spawn_flush_for_closed_tree(
                store.immutable.clone(),
                store.mutable.clone(),
                claims,
                sync_data,
            );
        }

        LoreEvent::RevisionTreeCloseComplete(LoreRevisionTreeCloseCompleteEventData {
            id: args.id,
            error_code: LoreErrorCode::None,
        })
        .send();

        Ok::<_, CloseError>(())
    })
}
