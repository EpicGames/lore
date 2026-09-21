// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_mutable_store` — write a mutable key's value.
//!
//! Each item targets either the local or the remote mutable store, selected the same way the
//! immutable ops select their backend: the default and `globals.local`/`globals.offline` act on
//! the handle's local mutable store; `globals.remote` (or a remote-bound handle) acts on the
//! remote store over the shared storage session. Storing the null value (`Hash::default()`)
//! removes the key. Each item resolves to one terminal `MUTABLE_STORE_ITEM_COMPLETE` carrying
//! `{id, error}`.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::store::event::LoreStorageMutableStoreItemCompleteEventData;
use lore_storage::StorageError;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::invalid_item;
use crate::storage::item_detail;
use crate::storage::store::EffectiveFlags;
use crate::storage::store::StoreInternal;

/// One `mutable_store` item — the `(partition, key, value, key_type)` to write.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreStorageMutableStoreItem {
    /// Caller-chosen id echoed back in `MUTABLE_STORE_ITEM_COMPLETE`
    pub id: u64,
    /// Partition (repository) to write to; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Key to write
    pub key: Hash,
    /// Value to store; the null value (`Hash::default()`) removes the key
    pub value: Hash,
    /// Kind of value the key refers to
    pub key_type: KeyType,
}

/// Arguments for `lore_storage_mutable_store`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(mutable_store_impl)]
pub struct LoreStorageMutableStoreArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Key-value pairs to write; each runs independently and emits its own `MUTABLE_STORE_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageMutableStoreItem>,
}

/// Write one or more mutable key-value pairs.
pub async fn mutable_store(
    globals: LoreGlobalArgs,
    args: LoreStorageMutableStoreArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, mutable_store_impl).await
}

async fn mutable_store_impl(
    globals: LoreGlobalArgs,
    args: LoreStorageMutableStoreArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        mutable_store,
        async move |store, args| {
            let items = args.items.as_slice();
            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }
            let effective = store.effective_flags(per_call)?;
            if effective.no_local && store.remote.is_none() {
                return Err(StorageError::from(InvalidArguments {
                    reason: "remote mutable_store requires a handle opened with `remote_config`"
                        .into(),
                }));
            }
            let mut reuse = crate::storage::store::SessionReuse::default();

            crate::storage::fan_out_items!(items, "mutable_store", |item| {
                let session = reuse.session_for(&store, item.partition, effective.no_local);
                let store = store.clone();
                async move { store_item(store, &item, effective, session).await }
            })
        },
    )
    .await
}

/// Resolve one store item against the selected backend. `effective.no_local` routes to the
/// remote mutable store via the handle's session; otherwise the local mutable store answers.
async fn store_item(
    store: Arc<StoreInternal>,
    item: &LoreStorageMutableStoreItem,
    effective: EffectiveFlags,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    if item.partition == Partition::default() {
        return emit_complete(item, Err(invalid_item("item names the default partition")));
    }

    if effective.no_local {
        let Some(session) = session else {
            return emit_complete(
                item,
                Err(StorageError::internal(
                    "remote-only store with no session on the handle",
                )),
            );
        };
        let stored = session
            .mutable_store(item.key, item.value, item.key_type)
            .await
            .forward("storing the mutable key on the remote");
        emit_complete(item, stored)
    } else {
        let stored = store
            .mutable
            .clone()
            .store(item.partition, item.key, item.value, item.key_type)
            .await
            .forward("storing the mutable key");
        emit_complete(item, stored)
    }
}

/// Emit the item's terminal event and return the outcome that was sent.
fn emit_complete(
    item: &LoreStorageMutableStoreItem,
    result: Result<(), StorageError>,
) -> Result<(), StorageError> {
    LoreEvent::StorageMutableStoreItemComplete(LoreStorageMutableStoreItemCompleteEventData {
        id: item.id,
        error: item_detail(&result),
    })
    .send();
    result
}
