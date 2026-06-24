// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_mutable_list` — list a partition's mutable key-value pairs of a given type.
//!
//! Listing acts on the handle's local mutable store only; a remote-targeted call
//! (`globals.remote`, or a remote-bound handle) is rejected with `INVALID_ARGUMENTS`. Each found
//! pair is emitted as a `MUTABLE_LIST_ENTRY` event `{id, key, value}`, followed by one terminal
//! `MUTABLE_LIST_ITEM_COMPLETE` event `{id, error_code}` for the item. A default/zero partition
//! lists across every partition the caller can access.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::store::event::LoreStorageMutableListEntryEventData;
use lore_revision::store::event::LoreStorageMutableListItemCompleteEventData;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One `mutable_list` item — the `(partition, key_type)` to list.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize)]
pub struct LoreStorageMutableListItem {
    /// Caller-chosen id echoed back on every entry and the terminal event
    pub id: u64,
    /// Partition (repository) to list; the zero/default partition lists every accessible partition
    pub partition: Partition,
    /// Kind of value to list
    pub key_type: KeyType,
}

/// Arguments for `lore_storage_mutable_list`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(mutable_list_local)]
pub struct LoreStorageMutableListArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Listings to perform; each runs independently and emits its own entries and terminal event
    pub items: LoreArray<LoreStorageMutableListItem>,
}

#[error_set]
enum MutableListError {
    InvalidArguments,
}

impl EventError for MutableListError {
    fn translated(&self) -> LoreError {
        match self {
            MutableListError::InvalidArguments(_) => LoreError::InvalidArguments,
            MutableListError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// List one or more partitions' mutable key-value pairs.
pub async fn mutable_list(
    globals: LoreGlobalArgs,
    args: LoreStorageMutableListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, mutable_list_local).await
}

async fn mutable_list_local(
    globals: LoreGlobalArgs,
    args: LoreStorageMutableListArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        mutable_list,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), MutableListError>(());
            }
            let effective = store.effective_flags(per_call)?;
            // Listing has no remote wire protocol, so a remote-targeted list is rejected up front
            // rather than attempting a per-item remote call.
            if effective.no_local {
                return Err(MutableListError::from(InvalidArguments {
                    reason: "mutable_list is only supported on the local store".into(),
                }));
            }
            let total = items.len();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let store = store.clone();
                lore_spawn!(tasks, async move { list_item(store, item).await });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "mutable_list")
        },
    )
    .await
}

/// List one item's local mutable key-value pairs. Entries are emitted as they arrive, then a
/// single terminal event closes the item. Remote listing is rejected before reaching here.
async fn list_item(store: Arc<StoreInternal>, item: LoreStorageMutableListItem) -> LoreErrorCode {
    match store
        .mutable
        .clone()
        .list(item.partition, item.key_type)
        .await
    {
        Ok(stream) => {
            let mut receiver = stream.channel();
            while let Some((key, value)) = receiver.recv().await {
                emit_entry(&item, key, value);
            }
            emit_complete(&item, LoreErrorCode::None)
        }
        Err(err) => emit_complete(&item, crate::storage::store_error_to_code(&err)),
    }
}

fn emit_entry(item: &LoreStorageMutableListItem, key: Hash, value: Hash) {
    LoreEvent::StorageMutableListEntry(LoreStorageMutableListEntryEventData {
        id: item.id,
        key,
        value,
    })
    .send();
}

/// Emit the item's terminal event and return the `error_code` that was sent, so callers can
/// `return emit_complete(..)` directly.
fn emit_complete(item: &LoreStorageMutableListItem, error_code: LoreErrorCode) -> LoreErrorCode {
    LoreEvent::StorageMutableListItemComplete(LoreStorageMutableListItemCompleteEventData {
        id: item.id,
        error_code,
    })
    .send();
    error_code
}
