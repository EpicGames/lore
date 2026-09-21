// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_upload` — push locally-stored, not-yet-durable content to the remote store.
//!
//! Whole-call pre-dispatch rejects the call when the handle has no remote, when
//! `globals.offline=1`, or when `globals.local=1` — any of these makes the op vacuous, so
//! the call fails up front rather than producing `ADDRESS_NOT_FOUND` per item.
//!
//! Per-item:
//! - `partition == Partition::default()` → `INVALID_ARGUMENTS`.
//! - `address.hash == Hash::default()` → no-op success with `already_durable=1`.
//! - Local entry already carries `PayloadStoredDurable` → no remote call, `already_durable=1`.
//! - Local payload missing → `ADDRESS_NOT_FOUND`.
//! - Otherwise: load payload from local, then `store_fragment(.., remote_session=Some(..))`
//!   which uploads the bytes and sets `PayloadStoredDurable` on the local entry on success.

use std::sync::Arc;

use lore_base::error::AddressNotFound;
use lore_base::error::InvalidArguments;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::store::event::LoreStorageUploadItemCompleteEventData;
use lore_storage::StorageError;
use lore_storage::concurrency::acquire_fragment_memory_permit;
use lore_storage::options::ReadOptions;
use lore_storage::read::load_fragment;
use lore_storage::store_types::StoreMatch;
use lore_storage::write::store_fragment;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::invalid_item;
use crate::storage::item_detail;
use crate::storage::store::StoreInternal;

/// One upload item — the `(partition, address)` of locally-stored content to push to remote.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreStorageUploadItem {
    /// Caller-chosen id echoed back in `UPLOAD_ITEM_COMPLETE`
    pub id: u64,
    /// Partition of the local content to push; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Local content address to push; `hash == Hash::default()` is no-op success with `already_durable=1`
    pub address: Address,
}

/// Arguments for `lore_storage_upload`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(upload_local)]
pub struct LoreStorageUploadArgs {
    /// Open storage handle; must have been opened with `remote_config`
    pub handle: LoreStore,
    /// Addresses to push to remote; each runs independently and emits its own `UPLOAD_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageUploadItem>,
}

/// Push one or more `(partition, address)` entries to the remote store.
pub async fn upload(
    globals: LoreGlobalArgs,
    args: LoreStorageUploadArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, upload_local).await
}

async fn upload_local(
    globals: LoreGlobalArgs,
    args: LoreStorageUploadArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        upload,
        async move |store, args| {
            if store.remote.is_none() {
                return Err(StorageError::from(InvalidArguments {
                    reason: "upload requires a handle opened with `remote_config`".into(),
                }));
            }
            let effective = store.effective_flags(per_call)?;
            if effective.no_remote {
                return Err(StorageError::from(InvalidArguments {
                    reason: "upload incompatible with `offline`/`local` flag set on handle or call"
                        .into(),
                }));
            }

            let items = args.items.as_slice();
            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }

            let mut reuse = crate::storage::store::SessionReuse::default();

            crate::storage::fan_out_items!(items, "upload", |item| {
                let session = reuse.session_for(&store, item.partition, true);
                let store = store.clone();
                async move { upload_item(store, &item, session).await }
            })
        },
    )
    .await
}

async fn upload_item(
    store: Arc<StoreInternal>,
    item: &LoreStorageUploadItem,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    if item.partition == Partition::default() {
        return emit_complete(
            item,
            0,
            Err(invalid_item("item names the default partition")),
        );
    }
    if item.address.hash == Hash::default() {
        return emit_complete(item, 1, Ok(()));
    }

    // Anything weaker than `MatchFull` means the local entry is incomplete and must be
    // treated as a missing payload for upload purposes.
    let resolved =
        lore_storage::immutable_store::query_one(&store.immutable, item.partition, item.address)
            .await;

    let (already_durable, has_local_payload) = match &resolved {
        Ok(resolved) if resolved.match_made == StoreMatch::MatchFull => {
            (resolved.stored_durable, resolved.stored_local)
        }
        _ => (false, false),
    };

    if already_durable {
        return emit_complete(item, 1, Ok(()));
    }
    if !has_local_payload {
        return emit_complete(
            item,
            0,
            Err(StorageError::from(AddressNotFound::from(item.address))),
        );
    }

    // `no_remote()` is load-bearing: we must not pull from a third party to satisfy a
    // missing-local-payload upload.
    let load = load_fragment(
        store.immutable.clone(),
        item.partition,
        item.address,
        ReadOptions::default().no_remote(),
        None,
    )
    .await;
    let (fragment, payload) = match load {
        Ok(pair) => pair,
        Err(err) => {
            return emit_complete(item, 0, Err(err));
        }
    };

    let permit = acquire_fragment_memory_permit(payload.len()).await;

    let stored = store_fragment(
        store.immutable.clone(),
        item.partition,
        item.address,
        fragment,
        payload,
        true,
        session,
        lore_revision::immutable::counted_write_context(),
        permit,
    )
    .await
    .map(|_| ());
    emit_complete(item, 0, stored)
}

/// Emit the item's terminal event and return the outcome that was sent.
fn emit_complete(
    item: &LoreStorageUploadItem,
    already_durable: u8,
    result: Result<(), StorageError>,
) -> Result<(), StorageError> {
    let address = if result.is_ok() {
        item.address
    } else {
        Address::default()
    };
    LoreEvent::StorageUploadItemComplete(LoreStorageUploadItemCompleteEventData {
        id: item.id,
        address,
        already_durable,
        error: item_detail(&result),
    })
    .send();
    result
}
