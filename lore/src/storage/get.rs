// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get` — read content-addressed buffers from a store.
//!
//! Per-item event sequence in single-buffer mode (`streaming=0`):
//! - `GET_HEADER { id, address, size_content }` — the size of the whole content, not of what
//!   this read returns. A ranged read gets the same header as a whole read of the same address.
//! - `GET_DATA { id, address, offset, bytes }` — the requested range in one event, `offset`
//!   being where it starts in the content. `LoreBytes` points into a `Bytes` buffer that the
//!   dispatcher keeps alive for the callback invocation via `send_with_bytes`.
//! - `GET_ITEM_COMPLETE { id, address, error_code }`.
//!
//! In streaming mode (`streaming=1`) the single `GET_DATA` is replaced by one event per leaf
//! fragment carrying a running `offset`. A failure partway through the tree ends the data early
//! and reports its own code on `GET_ITEM_COMPLETE`; as a backstop the cumulative byte count is
//! verified against the requested range and a shortfall surfaces as `Internal`. A streaming read
//! therefore never reports success for content it delivered only part of.
//!
//! Ranges: `offset` and `length` select part of the content, `length = 0` meaning "to the
//! end". A zeroed pair is the whole content, so an item that sets neither reads exactly what
//! it always did. Both modes fetch only the fragments the range covers. A range reaching past
//! the end is clamped; a range *starting* past the end is `INVALID_ARGUMENTS`, because an
//! empty answer would otherwise be indistinguishable from empty content.
//!
//! Short-circuits: `address.hash == Hash::default()` emits an empty buffer with
//! `error_code = NONE`, whatever range was asked for. Missing content yields
//! `error_code = ADDRESS_NOT_FOUND`.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
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
use lore_storage::read::read;
use lore_storage::read::read_stream;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One get item — the `(partition, address)` to read, and the range of it to read.
#[repr(C)]
#[derive(Copy, Clone, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreStorageGetItem {
    /// Caller-chosen id echoed back in every event for this item
    pub id: u64,
    /// Partition to read from; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Content address to read; `hash == Hash::default()` short-circuits to an empty buffer
    pub address: Address,
    /// First content byte to read, counted from the start of the decompressed content.
    /// Past the end of the content rejects with `INVALID_ARGUMENTS`
    pub offset: u64,
    /// Content bytes to read from `offset`; `0` reads to the end. A range reaching past the
    /// end is clamped to it, so `GET_DATA` may carry fewer bytes than asked for
    pub length: u64,
    /// Stream one `GET_DATA` per leaf fragment instead of a single reassembled buffer. A read
    /// that fails partway reports the failure on `GET_ITEM_COMPLETE` rather than ending short
    /// with a success code
    pub streaming: u8,
    /// Cache fetched bytes back to the local store even without the producer's
    /// `PayloadLocalCachePriority` hint
    pub local_cache: u8,
}

impl core::fmt::Debug for LoreStorageGetItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStorageGetItem")
            .field("id", &self.id)
            .field("offset", &self.offset)
            .field("length", &self.length)
            .field("streaming", &self.streaming)
            .field("local_cache", &self.local_cache)
            .finish()
    }
}

/// Arguments for `lore_storage_get`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(get_local)]
pub struct LoreStorageGetArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Addresses to read; each runs independently and emits its own event sequence
    pub items: LoreArray<LoreStorageGetItem>,
}

#[error_set]
enum GetError {
    InvalidArguments,
}

impl EventError for GetError {
    fn translated(&self) -> LoreError {
        match self {
            GetError::InvalidArguments(_) => LoreError::InvalidArguments,
            GetError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Read one or more content-addressed buffers.
pub async fn get(
    globals: LoreGlobalArgs,
    args: LoreStorageGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, get_local).await
}

async fn get_local(
    globals: LoreGlobalArgs,
    args: LoreStorageGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        get,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), GetError>(());
            }
            let effective = store.effective_flags(per_call)?;

            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let session = reuse.session_for(&store, item.partition, !effective.no_remote);
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    get_item(store, item, effective, session).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "get")
        },
    )
    .await
}

/// Read one item and emit its `HEADER` / `DATA…` / `ITEM_COMPLETE`
/// sequence. Returns the per-item `LoreErrorCode` for the call-level aggregator.
async fn get_item(
    store: Arc<StoreInternal>,
    item: LoreStorageGetItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    if item.partition == Partition::default() {
        emit_item_complete(&item, LoreErrorCode::InvalidArguments);
        return LoreErrorCode::InvalidArguments;
    }

    if item.address.hash == Hash::default() {
        emit_header(&item, 0);
        emit_data(&item, Bytes::new(), 0);
        emit_item_complete(&item, LoreErrorCode::None);
        return LoreErrorCode::None;
    }

    if item.streaming != 0 {
        return get_item_streaming(store, item, effective, remote_session).await;
    }

    let mut read_options = effective.read_options(remote_session.is_some());
    if item.local_cache != 0 {
        read_options = read_options.with_cache();
    }

    match read(
        store.immutable.clone(),
        item.partition,
        item.address,
        crate::storage::item_content_range(item.offset, item.length),
        read_options,
        remote_session,
    )
    .await
    {
        Ok((fragment, bytes)) => {
            // Empty bytes here would be indistinguishable from content that is genuinely
            // empty, so a start past the end is reported rather than clamped.
            if item.offset > fragment.size_content {
                emit_item_complete(&item, LoreErrorCode::InvalidArguments);
                return LoreErrorCode::InvalidArguments;
            }
            emit_header(&item, fragment.size_content);
            emit_data(&item, bytes, item.offset);
            emit_item_complete(&item, LoreErrorCode::None);
            LoreErrorCode::None
        }
        Err(err) => {
            let code = crate::storage::storage_error_to_code(&err);
            emit_item_complete(&item, code);
            code
        }
    }
}

/// Streaming-mode read: emit one `GET_DATA` per leaf fragment with a running offset.
/// `read_stream` returns the whole content's fragment and the content range that will arrive
/// once the root fragment has loaded but before the leaf chunks finish flowing through the
/// channel — we await that future first, emit `GET_HEADER` ahead of any data, then drain the
/// channel. The cumulative byte count is verified against the range — a mismatch surfaces as
/// `Internal`.
///
/// A ranged stream fetches only the leaves the range touches, so the memory and the work are
/// both proportional to the range rather than to the content.
async fn get_item_streaming(
    store: Arc<StoreInternal>,
    item: LoreStorageGetItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, lore_storage::StorageError>>(256);
    let mut read_options = effective.read_options(remote_session.is_some());
    if item.local_cache != 0 {
        read_options = read_options.with_cache();
    }
    let stream_future = read_stream(
        store.immutable.clone(),
        item.partition,
        item.address,
        crate::storage::item_content_range(item.offset, item.length),
        read_options,
        tx,
        remote_session,
    );

    let (fragment, streamed) = match stream_future.await {
        Ok(started) => started,
        Err(err) => {
            let code = crate::storage::storage_error_to_code(&err);
            emit_item_complete(&item, code);
            return code;
        }
    };

    // As in the buffered path. Nothing was spawned for an empty range, so dropping the
    // receiver here leaves no pipeline writing into a closed channel.
    if item.offset > fragment.size_content {
        emit_item_complete(&item, LoreErrorCode::InvalidArguments);
        return LoreErrorCode::InvalidArguments;
    }

    emit_header(&item, fragment.size_content);

    let mut offset = streamed.start;
    let mut code = LoreErrorCode::None;
    while let Some(chunk) = rx.recv().await {
        match chunk {
            Ok(chunk) => {
                let len = chunk.len() as u64;
                emit_data(&item, chunk, offset);
                offset += len;
            }
            Err(err) => {
                code = crate::storage::storage_error_to_code(&err);
                break;
            }
        }
    }

    if code == LoreErrorCode::None && offset != streamed.end {
        code = LoreErrorCode::Internal;
    }
    emit_item_complete(&item, code);
    code
}

fn emit_header(item: &LoreStorageGetItem, size_content: u64) {
    LoreEvent::StorageGetHeader(LoreStorageGetHeaderEventData {
        id: item.id,
        address: item.address,
        size_content,
    })
    .send();
}

/// Emit a `GET_DATA` event whose `LoreBytes` view points into `bytes`, with `bytes` attached to
/// the event as the callback-lifetime keepalive. The dispatcher holds the `Bytes` clone until
/// the callback returns, then drops it — the view is valid for the full callback invocation.
fn emit_data(item: &LoreStorageGetItem, bytes: Bytes, offset: u64) {
    let data = LoreBytes {
        ptr: bytes.as_ptr().cast(),
        len: bytes.len(),
    };
    let event = LoreEvent::StorageGetData(LoreStorageGetDataEventData {
        id: item.id,
        address: item.address,
        offset,
        bytes: data,
    });
    execution_context().dispatcher.send_with_bytes(event, bytes);
}

fn emit_item_complete(item: &LoreStorageGetItem, error_code: LoreErrorCode) {
    let address = if error_code == LoreErrorCode::None {
        item.address
    } else {
        Address::default()
    };
    LoreEvent::StorageGetItemComplete(LoreStorageGetItemCompleteEventData {
        id: item.id,
        address,
        error_code,
    })
    .send();
}
