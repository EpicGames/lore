// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_resolved` — `lore_storage_mutable_load` + `lore_storage_get` performed
//! server-side, saving one round trip.
//!
//! Per-item event sequence in single-buffer mode (`streaming=0`), identical to
//! `lore_storage_get`:
//! - `GET_HEADER { id, address, size_content }`
//! - `GET_DATA { id, address, offset: 0, bytes }`
//! - `GET_ITEM_COMPLETE { id, address, error }`
//!
//! In streaming mode (`streaming=1`) the single `GET_DATA` is replaced by one event per leaf
//! fragment with an advancing `offset`, so peak memory follows the fragment size rather than the
//! content size. `GET_HEADER` still precedes the data, but it follows the resolve — unlike
//! `lore_storage_get`, the address is not known until the key resolves. A failure partway through
//! the tree ends the data early and reports its own code on `GET_ITEM_COMPLETE`, with a
//! byte-count check against `size_content` as a backstop, so a partial delivery is never
//! reported as success.
//!
//! With `data_out` supplied the content lands in the caller's buffer instead: `GET_HEADER` then
//! `GET_ITEM_COMPLETE`, no `GET_DATA`, and `streaming` ignored. Content exceeding the buffer's
//! stated capacity fails the item with `Oversized`.
//!
//! With `fragments` set the leaves arrive as stored: `GET_HEADER`, then one
//! `GET_FRAGMENT { id, address, offset, fragment, bytes }` per leaf in content order, then
//! `GET_ITEM_COMPLETE`. `fragment` describes `bytes`. No leaf is expanded or checked against its
//! hash. `offset` and the byte-count backstop count content, as in streaming mode.
//!
//! `address` is the resolved address (`{ resolved_hash, context }`), so callers may cache the
//! key->hash mapping from the event stream.
//!
//! Keys are always resolved as `KeyType::Resolve`, so no key type is supplied. The QUIC request
//! carries a `flags` word to keep its length a multiple of four, but no bit is defined and the
//! value is not a caller's to choose, so it is not part of the item — the server rejects a
//! non-zero value from any peer that sends one.
//!
//! Backend selection matches `lore_storage_get`: local first, remote on a miss, narrowed by the
//! handle's bound and per-call `offline`/`local`/`remote` flags. A missing key, or one resolving
//! to absent content, yields `ADDRESS_NOT_FOUND`.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreBytesMut;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::lore::execution_context;
use lore_revision::store::event::LoreStorageGetDataEventData;
use lore_revision::store::event::LoreStorageGetFragmentEventData;
use lore_revision::store::event::LoreStorageGetHeaderEventData;
use lore_revision::store::event::LoreStorageGetItemCompleteEventData;
use lore_storage::StorageError;
use lore_storage::read::read_resolved;
use lore_storage::read::read_resolved_stream;
use lore_transport::quic::storage_service::get_resolved_flags;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::invalid_item;
use crate::storage::store::StoreInternal;

/// One get-resolved item — the mutable key to resolve and the context to read it in.
#[repr(C)]
#[derive(Copy, Clone, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode)]
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
    /// Stream one `GET_DATA` per leaf fragment instead of a single reassembled buffer, as
    /// `lore_storage_get` does. Peak memory then follows the fragment size rather than the
    /// content size, which is what makes a key naming something large usable. A read that fails
    /// partway reports the failure on `GET_ITEM_COMPLETE` rather than ending short with a
    /// success code
    pub streaming: u8,
    /// Cache fetched bytes in the local store, as they always are for a fragment written with
    /// `PayloadLocalCachePriority` or one of revision state. A fetched fragment whose bytes are not
    /// cached is still written to the local store as an entry without its payload, so a put of
    /// the same content uploads nothing. Either is marked stored durable
    pub local_cache: u8,
    /// Deliver one `GET_FRAGMENT` per leaf fragment in content order in place of `GET_DATA`, each
    /// carrying the leaf's fragment and its payload as stored, with `streaming` ignored. No leaf
    /// is expanded or checked against its hash. An item with `data_out` supplied rejects with
    /// `INVALID_ARGUMENTS`
    pub fragments: u8,
    /// Writable buffer receiving the content, `len` stating its capacity. Zero-initialized selects
    /// delivery in events.
    ///
    /// The capacity is the limit: content exceeding it fails the item with
    /// `Oversized` rather than truncating. `GET_HEADER` reports the content
    /// size, no `GET_DATA` follows, and `streaming` is ignored. The buffer holds unspecified bytes
    /// when the item fails.
    #[bitcode(skip)]
    pub data_out: LoreBytesMut,
}

impl core::fmt::Debug for LoreStorageGetResolvedItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStorageGetResolvedItem")
            .field("id", &self.id)
            .field("streaming", &self.streaming)
            .field("local_cache", &self.local_cache)
            .field("fragments", &self.fragments)
            .field("data_out", &self.data_out)
            .finish()
    }
}

/// Arguments for `lore_storage_get_resolved`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(get_resolved_local)]
pub struct LoreStorageGetResolvedArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Keys to resolve and read; each runs independently and emits its own event sequence
    pub items: LoreArray<LoreStorageGetResolvedItem>,
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
            let items = args.items.as_slice();
            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }
            let effective = store.effective_flags(per_call)?;
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut lent = crate::storage::lent::LentBuffers::default();

            let result = crate::storage::fan_out_items!(items, "get_resolved", |item| {
                let session = reuse.session_for(&store, item.partition, !effective.no_remote);
                let store = store.clone();
                let lender = lent.lender(&item.data_out);
                async move { get_resolved_item(store, &item, effective, session, lender).await }
            });
            lent.returned().await;
            result
        },
    )
    .await
}

/// Resolve and read one item, emitting its `HEADER` ... `ITEM_COMPLETE` sequence, into the item's
/// `data_out` through `lender` when it supplies one. Returns the item's own error so the call-level
/// reduction can pick the dominant failure.
async fn get_resolved_item(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetResolvedItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
    lender: Option<crate::storage::lent::ItemLender>,
) -> Result<(), StorageError> {
    if item.partition == Partition::default() {
        return emit_item_complete(
            item,
            Address::default(),
            Err(invalid_item("item names the default partition")),
        );
    }

    if item.key == Hash::default() {
        return emit_item_complete(
            item,
            Address::default(),
            Err(invalid_item("item names the zero key")),
        );
    }

    let mut read_options = effective.read_options(remote_session.is_some());
    if item.local_cache != 0 {
        read_options = read_options.with_cache();
    }

    if item.fragments != 0 {
        if item.data_out.is_supplied() {
            return emit_item_complete(
                item,
                Address::default(),
                Err(invalid_item("fragment delivery has no caller-buffer form")),
            );
        }
        return get_resolved_item_streaming(
            store,
            item,
            fragments_read_options(read_options),
            remote_session,
            emit_fragment,
        )
        .await;
    }

    if let Some(lender) = lender {
        return get_resolved_item_into(store, item, read_options, remote_session, lender).await;
    }

    if item.streaming != 0 {
        return get_resolved_item_streaming(
            store,
            item,
            read_options,
            remote_session,
            |item, address, (_fragment, bytes), offset| {
                let len = bytes.len() as u64;
                emit_data(item, address, bytes, offset);
                len
            },
        )
        .await;
    }

    match read_resolved(
        store.immutable.clone(),
        store.mutable.clone(),
        item.partition,
        item.key,
        item.context,
        get_resolved_flags::NONE,
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
            emit_header(item, address, size);
            emit_data(item, address, bytes, 0);
            emit_item_complete(item, address, Ok(()))
        }
        Err(err) => emit_item_complete(item, Address::default(), Err(err)),
    }
}

/// The read options of `fragments` delivery: payloads as stored, from a remote trusted, as the
/// caller checks what it takes, so no leaf is decoded or hashed on its way.
#[lore_macro::test_pub]
fn fragments_read_options(options: lore_storage::ReadOptions) -> lore_storage::ReadOptions {
    options.no_decompress().trusting_remote()
}

/// Counterpart of [`get_resolved_item`] delivering the content into `data_out`. Emits
/// `GET_HEADER` with the number of bytes written, then `GET_ITEM_COMPLETE`; content exceeding the
/// stated capacity fails the item rather than truncating.
async fn get_resolved_item_into(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetResolvedItem,
    read_options: lore_storage::ReadOptions,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
    lender: crate::storage::lent::ItemLender,
) -> Result<(), StorageError> {
    let mut dst = lender.lend(&item.data_out);
    match lore_storage::read_resolved_into_buffer(
        store.immutable.clone(),
        store.mutable.clone(),
        item.partition,
        item.key,
        item.context,
        get_resolved_flags::NONE,
        &mut dst,
        read_options,
        remote_session,
    )
    .await
    {
        Ok((resolved, written)) => {
            let address = Address {
                hash: resolved,
                context: item.context,
            };
            emit_header(item, address, written as u64);
            emit_item_complete(item, address, Ok(()))
        }
        Err(err) => emit_item_complete(item, Address::default(), Err(err)),
    }
}

/// Streaming counterpart of [`get_resolved_item`]: one event per leaf instead of a single
/// reassembled buffer, mirroring `get`'s streaming worker. `emit` sends a leaf at the content offset
/// given and returns the content it covers. The resolved address is not known until the key
/// resolves, so `GET_HEADER` follows the resolve rather than preceding it.
async fn get_resolved_item_streaming(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetResolvedItem,
    read_options: lore_storage::options::ReadOptions,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
    emit: impl Fn(&LoreStorageGetResolvedItem, Address, (Fragment, Bytes), u64) -> u64,
) -> Result<(), StorageError> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(Fragment, Bytes), StorageError>>(256);
    let stream_future = read_resolved_stream(
        store.immutable.clone(),
        store.mutable.clone(),
        item.partition,
        item.key,
        item.context,
        get_resolved_flags::NONE,
        read_options,
        tx,
        remote_session,
    );

    let (resolved, size_content) = match stream_future.await {
        Ok(result) => result,
        Err(err) => return emit_item_complete(item, Address::default(), Err(err)),
    };

    let address = Address {
        hash: resolved,
        context: item.context,
    };
    emit_header(item, address, size_content);

    let mut offset: u64 = 0;
    let mut result: Result<(), StorageError> = Ok(());
    while let Some(leaf) = rx.recv().await {
        match leaf {
            Ok(leaf) => offset += emit(item, address, leaf, offset),
            Err(err) => {
                result = Err(err);
                break;
            }
        }
    }

    if result.is_ok() && offset != size_content {
        result = Err(StorageError::internal(format!(
            "stream ended at {offset} with {size_content} bytes of content requested"
        )));
    }
    emit_item_complete(item, address, result)
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

/// Emit `GET_FRAGMENT` with the payload attached as the callback-lifetime keepalive, as
/// [`emit_data`] does. Returns the content the leaf covers.
fn emit_fragment(
    item: &LoreStorageGetResolvedItem,
    address: Address,
    (fragment, bytes): (Fragment, Bytes),
    offset: u64,
) -> u64 {
    let data = LoreBytes {
        ptr: bytes.as_ptr().cast(),
        len: bytes.len(),
    };
    let event = LoreEvent::StorageGetFragment(LoreStorageGetFragmentEventData {
        id: item.id,
        address,
        offset,
        fragment,
        bytes: data,
    });
    execution_context().dispatcher.send_with_bytes(event, bytes);
    fragment.size_content
}

/// Emit the item's terminal event and return the outcome that was sent.
fn emit_item_complete(
    item: &LoreStorageGetResolvedItem,
    address: Address,
    result: Result<(), StorageError>,
) -> Result<(), StorageError> {
    LoreEvent::StorageGetItemComplete(LoreStorageGetItemCompleteEventData {
        id: item.id,
        address,
        error: LoreErrorDetail::from_result(&result),
    })
    .send();
    result
}
