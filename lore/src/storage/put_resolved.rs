// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_put_resolved` — store a buffer and publish a mutable key naming it.
//!
//! The write side of `lore_storage_get_resolved`, and the only thing that makes a key resolvable:
//! it stores the content, then maps `key` to the content's hash under `KeyType::Resolve`.
//!
//! Backend selection matches `lore_storage_put` rather than the read ops: the local store always
//! receives both the content and the mapping, and `remote_write = 1` additionally publishes them
//! to the remote (unless `globals.offline`/`local` vetoes it). There is no local-then-remote
//! fallback here — that is a read concept.
//!
//! Per-item behaviour:
//! - `partition == Partition::default()`, a zero `key`, or `data.len > 0 && data.ptr == NULL`:
//!   rejects with `error_code = INVALID_ARGUMENTS`; other items run independently.
//! - `data.len == 0`: rejects with `INVALID_ARGUMENTS`. Unlike `put`, an empty buffer has no
//!   useful meaning here — publishing a key that resolves to the zero hash is what *deleting* it
//!   looks like to `get_resolved`, so it is refused rather than silently doing that.
//! - Otherwise: `write_resolved`, and the stored address is reported in `PUT_ITEM_COMPLETE`.
//!
//! Emits the same `PUT_ITEM_COMPLETE { id, address, error_code }` as `lore_storage_put`, where
//! `address` is the content the key now resolves to — so a caller can hand it straight to `get`.

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
use lore_revision::store::event::LoreStoragePutItemCompleteEventData;
use lore_storage::options::WriteOptions;
use lore_storage::write::write_resolved;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One put-resolved item — the buffer to store and the mutable key to publish it under.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreStoragePutResolvedItem {
    /// Caller-chosen id echoed back in `PUT_ITEM_COMPLETE`
    pub id: u64,
    /// Target partition; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Mutable key to publish the stored hash under; a zero key rejects with `INVALID_ARGUMENTS`
    pub key: Hash,
    /// Dedup tag stored alongside the content hash in the resulting address, and the context a
    /// later `get_resolved` must read the key at
    pub context: Context,
    /// Borrowed view into caller memory; bytes must live until `Complete` fires
    pub data: LoreBytes,
    /// Also publish the content and the mapping to the remote; ignored when the handle has no
    /// remote or the call is offline/local
    pub remote_write: u8,
    /// Tag the fragment with `PayloadLocalCachePriority` so future remote reads always cache it
    /// locally
    pub local_cache: u8,
    /// Leaf fragment size cap for large buffers; `0` lets the writer choose. Ignored for buffers
    /// under `FRAGMENT_SIZE_THRESHOLD`
    pub fixed_size_chunk: u64,
}

impl core::fmt::Debug for LoreStoragePutResolvedItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStoragePutResolvedItem")
            .field("id", &self.id)
            .field("remote_write", &self.remote_write)
            .field("local_cache", &self.local_cache)
            .field("fixed_size_chunk", &self.fixed_size_chunk)
            .finish()
    }
}

/// Arguments for `lore_storage_put_resolved`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(put_resolved_local)]
pub struct LoreStoragePutResolvedArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Buffers to store and publish; each runs independently and emits its own
    /// `PUT_ITEM_COMPLETE`
    pub items: LoreArray<LoreStoragePutResolvedItem>,
}

#[error_set]
enum PutResolvedError {
    InvalidArguments,
}

impl EventError for PutResolvedError {
    fn translated(&self) -> LoreError {
        match self {
            PutResolvedError::InvalidArguments(_) => LoreError::InvalidArguments,
            PutResolvedError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Store one or more buffers and publish a mutable key naming each.
pub async fn put_resolved(
    globals: LoreGlobalArgs,
    args: LoreStoragePutResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, put_resolved_local).await
}

async fn put_resolved_local(
    globals: LoreGlobalArgs,
    args: LoreStoragePutResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        put_resolved,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();

            if items.is_empty() {
                return Ok::<(), PutResolvedError>(());
            }

            let effective = store.effective_flags(per_call)?;

            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let session = reuse.session_for(
                    &store,
                    item.partition,
                    item.remote_write != 0 && !effective.no_remote,
                );
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    put_resolved_item(store, item, session).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "put_resolved")
        },
    )
    .await
}

/// Execute one item. Always emits a single `PUT_ITEM_COMPLETE` event.
async fn put_resolved_item(
    store: Arc<StoreInternal>,
    item: LoreStoragePutResolvedItem,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    let (address, error_code) = store_and_publish(store, item, session).await;
    LoreEvent::StoragePutItemComplete(LoreStoragePutItemCompleteEventData {
        id: item.id,
        address,
        error_code,
    })
    .send();
    error_code
}

async fn store_and_publish(
    store: Arc<StoreInternal>,
    item: LoreStoragePutResolvedItem,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> (Address, LoreErrorCode) {
    if item.partition == Partition::default() {
        return (Address::default(), LoreErrorCode::InvalidArguments);
    }

    if item.key == Hash::default() {
        // The mutable store reads a zero value as a tombstone, so a zero key is never storable.
        return (Address::default(), LoreErrorCode::InvalidArguments);
    }

    if item.data.len == 0 || item.data.ptr.is_null() {
        // `put` short-circuits an empty buffer to the zero hash, but publishing a key that
        // resolves to zero is indistinguishable from deleting it. Refuse instead.
        return (Address::default(), LoreErrorCode::InvalidArguments);
    }

    // SAFETY:
    // - `item.data.ptr` is non-null (checked above) and the FFI contract requires
    //   `item.data.len` valid bytes behind it.
    // - The `'static` lifetime is fudged exactly as in `put`: the buffer's real lifetime is
    //   bounded by the call's `Complete` event, which `storage_call` only emits after this
    //   future and every spawned task has resolved.
    let slice: &'static [u8] =
        unsafe { std::slice::from_raw_parts(item.data.ptr.cast::<u8>(), item.data.len) };
    let bytes = Bytes::from_static(slice);

    let mut write_options = WriteOptions::default();
    if item.fixed_size_chunk > 0 {
        write_options = write_options.with_fixed_size_chunk(item.fixed_size_chunk as usize);
    }
    if item.local_cache != 0 {
        write_options = write_options.with_local_cache_priority();
    }

    match write_resolved(
        store.immutable.clone(),
        store.mutable.clone(),
        item.partition,
        item.key,
        item.context,
        bytes,
        write_options,
        remote_session,
    )
    .await
    {
        Ok((address, _fragment)) => (address, LoreErrorCode::None),
        Err(err) => (
            Address::default(),
            crate::storage::storage_error_to_code(&err),
        ),
    }
}
