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
//! - `GET_ITEM_COMPLETE { id, address, error }`.
//!
//! In streaming mode (`streaming=1`) the single `GET_DATA` is replaced by one event per leaf
//! fragment carrying a running `offset`. A failure partway through the tree ends the data early
//! and reports its own code on `GET_ITEM_COMPLETE`; as a backstop the cumulative byte count is
//! verified against the requested range and a shortfall surfaces as `Internal`. A streaming read
//! therefore never reports success for content it delivered only part of.
//!
//! With `data_out` supplied the range lands in the caller's buffer instead: `GET_HEADER` then
//! `GET_ITEM_COMPLETE`, no `GET_DATA`, and `streaming` ignored. A range exceeding the buffer's
//! stated capacity fails the item with `Oversized`.
//!
//! Ranges: `offset` and `length` select part of the content, `length = 0` meaning "to the
//! end". A zeroed pair is the whole content, so an item that sets neither reads exactly what
//! it always did. Both modes fetch only the fragments the range covers. A range reaching past
//! the end is clamped; a range *starting* past the end is `INVALID_ARGUMENTS`, because an
//! empty answer would otherwise be indistinguishable from empty content.
//!
//! Short-circuits: `address.hash == Hash::default()` emits an empty buffer with
//! an empty error detail, whatever range was asked for. Missing content yields
//! the address-not-found error.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreBytesMut;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::lore::execution_context;
use lore_revision::store::event::LoreStorageGetDataEventData;
use lore_revision::store::event::LoreStorageGetHeaderEventData;
use lore_revision::store::event::LoreStorageGetItemCompleteEventData;
use lore_storage::StorageError;
use lore_storage::read::read;
use lore_storage::read::read_stream;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::invalid_item;
use crate::storage::item_detail;
use crate::storage::offset_past_end;
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
    /// Writable buffer receiving the requested range, `len` stating its capacity. Zero-initialized
    /// selects `GET_DATA` delivery.
    ///
    /// The capacity is the limit: a range exceeding it fails the item with
    /// `Oversized` rather than truncating. `GET_HEADER` reports the whole
    /// content's size, which with `offset` and `length` gives the bytes written; no `GET_DATA`
    /// follows, and `streaming` is ignored. The buffer holds unspecified bytes when the item fails.
    #[serde(skip)]
    pub data_out: LoreBytesMut,
}

impl core::fmt::Debug for LoreStorageGetItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStorageGetItem")
            .field("id", &self.id)
            .field("offset", &self.offset)
            .field("length", &self.length)
            .field("streaming", &self.streaming)
            .field("local_cache", &self.local_cache)
            .field("data_out", &self.data_out)
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
            let items = args.items.as_slice();
            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }
            let effective = store.effective_flags(per_call)?;
            let mut reuse = crate::storage::store::SessionReuse::default();

            crate::storage::fan_out_items!(items, "get", |item| {
                let session = reuse.session_for(&store, item.partition, !effective.no_remote);
                let store = store.clone();
                async move { get_item(store, &item, effective, session).await }
            })
        },
    )
    .await
}

/// Read one item and emit its `HEADER` / `DATA…` / `ITEM_COMPLETE`
/// sequence. Returns the item's own error so the call-level reduction can pick the dominant
/// failure.
async fn get_item(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    if item.partition == Partition::default() {
        return emit_item_complete(item, Err(invalid_item("item names the default partition")));
    }

    if item.address.hash == Hash::default() {
        emit_header(item, 0);
        // An item delivering into its own buffer receives no `GET_DATA`, empty or otherwise.
        if !item.data_out.is_supplied() {
            emit_data(item, Bytes::new(), 0);
        }
        return emit_item_complete(item, Ok(()));
    }

    if item.data_out.is_supplied() {
        return get_item_into(store, item, effective, remote_session).await;
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
                return emit_item_complete(
                    item,
                    Err(offset_past_end(item.offset, fragment.size_content)),
                );
            }
            emit_header(item, fragment.size_content);
            emit_data(item, bytes, item.offset);
            emit_item_complete(item, Ok(()))
        }
        Err(err) => emit_item_complete(item, Err(err)),
    }
}

/// Counterpart of [`get_item`] delivering the requested range into `data_out`. Emits `GET_HEADER`
/// with the whole content's size, as the other modes do, then `GET_ITEM_COMPLETE`; a range
/// exceeding the stated capacity fails the item rather than truncating.
async fn get_item_into(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    let mut read_options = effective.read_options(remote_session.is_some());
    if item.local_cache != 0 {
        read_options = read_options.with_cache();
    }

    // SAFETY: `data_out` names caller memory that stays valid, and untouched by anyone else, for
    // the duration of the call this future is wholly within. `len` bounds the read.
    let mut dst = unsafe {
        lore_storage::CallerBuffer::new(item.data_out.ptr.cast::<u8>(), item.data_out.len)
    };

    match lore_storage::read_into_buffer(
        store.immutable.clone(),
        item.partition,
        item.address,
        crate::storage::item_content_range(item.offset, item.length),
        &mut dst,
        read_options,
        remote_session,
    )
    .await
    {
        Ok((fragment, _written)) => {
            // As in the buffered path: an empty delivery is indistinguishable from content that is
            // genuinely empty, so a start past the end is reported rather than clamped.
            if item.offset > fragment.size_content {
                return emit_item_complete(
                    item,
                    Err(offset_past_end(item.offset, fragment.size_content)),
                );
            }
            emit_header(item, fragment.size_content);
            emit_item_complete(item, Ok(()))
        }
        Err(err) => emit_item_complete(item, Err(err)),
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
    item: &LoreStorageGetItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
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
        Err(err) => return emit_item_complete(item, Err(err)),
    };

    // As in the buffered path. Nothing was spawned for an empty range, so dropping the
    // receiver here leaves no pipeline writing into a closed channel.
    if item.offset > fragment.size_content {
        return emit_item_complete(
            item,
            Err(offset_past_end(item.offset, fragment.size_content)),
        );
    }

    emit_header(item, fragment.size_content);

    let mut offset = streamed.start;
    let mut result: Result<(), StorageError> = Ok(());
    while let Some(chunk) = rx.recv().await {
        match chunk {
            Ok(chunk) => {
                let len = chunk.len() as u64;
                emit_data(item, chunk, offset);
                offset += len;
            }
            Err(err) => {
                result = Err(err);
                break;
            }
        }
    }

    if result.is_ok() && offset != streamed.end {
        result = Err(StorageError::internal(format!(
            "stream ended at {offset} with the range {}..{} requested",
            streamed.start, streamed.end
        )));
    }
    emit_item_complete(item, result)
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

/// Emit the item's terminal event and return the outcome that was sent.
fn emit_item_complete(
    item: &LoreStorageGetItem,
    result: Result<(), StorageError>,
) -> Result<(), StorageError> {
    let address = if result.is_ok() {
        item.address
    } else {
        Address::default()
    };
    LoreEvent::StorageGetItemComplete(LoreStorageGetItemCompleteEventData {
        id: item.id,
        address,
        error: item_detail(&result),
    })
    .send();
    result
}
