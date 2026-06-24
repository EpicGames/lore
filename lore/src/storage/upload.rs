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

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::store::event::LoreStorageUploadItemCompleteEventData;
use lore_storage::concurrency::acquire_fragment_memory_permit;
use lore_storage::options::ReadOptions;
use lore_storage::read::load_fragment;
use lore_storage::store_types::StoreMatch;
use lore_storage::write::store_fragment;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One upload item — the `(partition, address)` of locally-stored content to push to remote.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize)]
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

#[error_set]
enum UploadError {
    InvalidArguments,
}

impl EventError for UploadError {
    fn translated(&self) -> LoreError {
        match self {
            UploadError::InvalidArguments(_) => LoreError::InvalidArguments,
            UploadError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
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
                return Err(UploadError::from(InvalidArguments {
                    reason: "upload requires a handle opened with `remote_config`".into(),
                }));
            }
            let effective = store.effective_flags(per_call)?;
            if effective.no_remote {
                return Err(UploadError::from(InvalidArguments {
                    reason: "upload incompatible with `offline`/`local` flag set on handle or call"
                        .into(),
                }));
            }

            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), UploadError>(());
            }

            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let session = reuse.session_for(&store, item.partition, true);
                let store = store.clone();
                lore_spawn!(
                    tasks,
                    async move { upload_item(store, item, session).await }
                );
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "upload")
        },
    )
    .await
}

async fn upload_item(
    store: Arc<StoreInternal>,
    item: LoreStorageUploadItem,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    if item.partition == Partition::default() {
        return emit_complete(&item, 0, LoreErrorCode::InvalidArguments);
    }
    if item.address.hash == Hash::default() {
        return emit_complete(&item, 1, LoreErrorCode::None);
    }

    // Anything weaker than `MatchFull` means the local entry is incomplete and must be
    // treated as a missing payload for upload purposes.
    let query = store
        .immutable
        .clone()
        .query(item.partition, item.address, StoreMatch::MatchFull)
        .await;

    let (already_durable, has_local_payload) = match &query {
        Ok(qr) if qr.match_made == StoreMatch::MatchFull => (
            qr.fragment.flags & FragmentFlags::PayloadStoredDurable != 0,
            qr.fragment.flags & FragmentFlags::PayloadStoredLocal != 0,
        ),
        _ => (false, false),
    };

    if already_durable {
        return emit_complete(&item, 1, LoreErrorCode::None);
    }
    if !has_local_payload {
        return emit_complete(&item, 0, LoreErrorCode::AddressNotFound);
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
            return emit_complete(&item, 0, crate::storage::storage_error_to_code(&err));
        }
    };

    let permit = acquire_fragment_memory_permit(payload.len()).await;

    match store_fragment(
        store.immutable.clone(),
        item.partition,
        item.address,
        fragment,
        payload,
        true,
        session,
        None,
        permit,
    )
    .await
    {
        Ok(_) => emit_complete(&item, 0, LoreErrorCode::None),
        Err(err) => emit_complete(&item, 0, crate::storage::storage_error_to_code(&err)),
    }
}

/// Emit the item's terminal event and return the `error_code` that was sent, so callers can
/// `return emit_complete(..)` directly.
fn emit_complete(
    item: &LoreStorageUploadItem,
    already_durable: u8,
    error_code: LoreErrorCode,
) -> LoreErrorCode {
    let address = if error_code == LoreErrorCode::None {
        item.address
    } else {
        Address::default()
    };
    LoreEvent::StorageUploadItemComplete(LoreStorageUploadItemCompleteEventData {
        id: item.id,
        address,
        already_durable,
        error_code,
    })
    .send();
    error_code
}
