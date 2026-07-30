// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_resolved` — `lore_storage_mutable_load` + `lore_storage_get` performed
//! server-side, saving one round trip.
//!
//! Per-item event sequence, identical to `lore_storage_get`:
//! - `GET_HEADER { id, address, size_content }`
//! - `GET_DATA { id, address, offset: 0, bytes }`
//! - `GET_ITEM_COMPLETE { id, address, error_code }`
//!
//! `address` is the resolved address (`{ resolved_hash, context }`), so callers may cache the
//! key->hash mapping from the event stream.
//!
//! Keys are always resolved as `KeyType::Resolve`, so no key type is supplied.
//!
//! Backend selection matches `lore_storage_get`: local first, remote on a miss, narrowed by the
//! handle's bound and per-call `offline`/`local`/`remote` flags. A missing key, or one resolving
//! to absent content, yields `ADDRESS_NOT_FOUND`.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::EventError;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::lore::execution_context;
use lore_revision::store::event::LoreStorageGetDataEventData;
use lore_revision::store::event::LoreStorageGetHeaderEventData;
use lore_revision::store::event::LoreStorageGetItemCompleteEventData;
use lore_storage::read::read_resolved;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One get-resolved item — the mutable key to resolve and the context to read it in.
#[repr(C)]
#[derive(Copy, Clone, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreStorageGetResolvedItem {
    /// Caller-chosen id echoed back in every event for this item
    pub id: u64,
    /// Partition to resolve and read within; the zero/default partition rejects with
    /// `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Mutable key to resolve, always read as `KeyType::Resolve`
    pub key: Hash,
    /// Paired with the resolved hash to address the immutable read; the mutable store yields
    /// only a hash.
    pub context: Context,
    /// Cache fetched bytes back to the local store even without the producer's
    /// `PayloadLocalCachePriority` hint
    pub local_cache: u8,
    /// Reserved bitmask forwarded to the server, low 24 bits only; 0 for default behaviour.
    /// Unknown bits are rejected.
    pub flags: u32,
}

impl core::fmt::Debug for LoreStorageGetResolvedItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStorageGetResolvedItem")
            .field("id", &self.id)
            .field("local_cache", &self.local_cache)
            .field("flags", &self.flags)
            .finish()
    }
}

/// Arguments for `lore_storage_get_resolved`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(get_resolved_local)]
pub struct LoreStorageGetResolvedArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Keys to resolve and read; each runs independently and emits its own event sequence
    pub items: LoreArray<LoreStorageGetResolvedItem>,
}

#[error_set]
enum GetResolvedError {
    InvalidArguments,
}

impl EventError for GetResolvedError {
    fn translated(&self) -> LoreError {
        match self {
            GetResolvedError::InvalidArguments(_) => LoreError::InvalidArguments,
            GetResolvedError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Resolve one or more mutable keys and read the content they point at.
pub async fn get_resolved(
    globals: LoreGlobalArgs,
    args: LoreStorageGetResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, get_resolved_local).await
}

async fn get_resolved_local(
    globals: LoreGlobalArgs,
    args: LoreStorageGetResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        get_resolved,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), GetResolvedError>(());
            }
            let effective = store.effective_flags(per_call)?;

            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let session = reuse.session_for(&store, item.partition, !effective.no_remote);
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    get_resolved_item(store, item, effective, session).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "get_resolved")
        },
    )
    .await
}

/// Resolve and read one item, emitting the `HEADER` / `DATA` / `ITEM_COMPLETE` sequence.
/// Returns the per-item `LoreErrorCode` for the call-level aggregator.
async fn get_resolved_item(
    store: Arc<StoreInternal>,
    item: LoreStorageGetResolvedItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    if item.partition == Partition::default() {
        emit_item_complete(&item, Address::default(), LoreErrorCode::InvalidArguments);
        return LoreErrorCode::InvalidArguments;
    }

    if item.key == Hash::default() {
        // A zero key can never be stored, so this is a caller bug rather than a miss.
        emit_item_complete(&item, Address::default(), LoreErrorCode::InvalidArguments);
        return LoreErrorCode::InvalidArguments;
    }

    let mut read_options = effective.read_options(remote_session.is_some());
    if item.local_cache != 0 {
        read_options = read_options.with_cache();
    }

    match read_resolved(
        store.immutable.clone(),
        store.mutable.clone(),
        item.partition,
        item.key,
        item.context,
        item.flags,
        None,
        read_options,
        remote_session,
    )
    .await
    {
        Ok((resolved, bytes)) => {
            let address = Address {
                hash: resolved,
                context: item.context,
            };
            let size = bytes.len() as u64;
            emit_header(&item, address, size);
            emit_data(&item, address, bytes, 0);
            emit_item_complete(&item, address, LoreErrorCode::None);
            LoreErrorCode::None
        }
        Err(err) => {
            let code = crate::storage::storage_error_to_code(&err);
            emit_item_complete(&item, Address::default(), code);
            code
        }
    }
}

fn emit_header(item: &LoreStorageGetResolvedItem, address: Address, size_content: u64) {
    LoreEvent::StorageGetHeader(LoreStorageGetHeaderEventData {
        id: item.id,
        address,
        size_content,
    })
    .send();
}

/// Emit `GET_DATA` with `bytes` attached as the callback-lifetime keepalive; same contract as
/// `get`'s `emit_data`.
fn emit_data(item: &LoreStorageGetResolvedItem, address: Address, bytes: Bytes, offset: u64) {
    let data = LoreBytes {
        ptr: bytes.as_ptr().cast(),
        len: bytes.len(),
    };
    let event = LoreEvent::StorageGetData(LoreStorageGetDataEventData {
        id: item.id,
        address,
        offset,
        bytes: data,
    });
    execution_context().dispatcher.send_with_bytes(event, bytes);
}

fn emit_item_complete(
    item: &LoreStorageGetResolvedItem,
    address: Address,
    error_code: LoreErrorCode,
) {
    LoreEvent::StorageGetItemComplete(LoreStorageGetItemCompleteEventData {
        id: item.id,
        address,
        error_code,
    })
    .send();
}
