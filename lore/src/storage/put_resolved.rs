// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_put_resolved` — store a buffer and publish a mutable key naming it.
//!
//! The write side of `lore_storage_get_resolved`: it stores the content, then maps `key` to the
//! content's hash under `KeyType::Resolve`. Other writers reach that mapping too —
//! `lore_storage_mutable_store`, a fragmented publish's trailing mutable write, `get_resolved`'s
//! caching — but this is the only one that guarantees the content is stored before the key names
//! it.
//!
//! Publishing is last-writer-wins, deliberately: concurrent publishers all succeed, the last
//! write is what the key names, and nobody is told they were overwritten. Either mapping is valid
//! for the foreign-keyed caching this exists for. A caller who must detect a lost update pairs
//! `lore_storage_put` with `lore_storage_mutable_compare_and_swap`, paying the second round trip.
//! Folding the swap in here is not available: a failed swap would strand the content just stored,
//! worst under exactly the contention that would motivate it.
//!
//! Backend selection matches `lore_storage_put`, not the read ops: the local store always
//! receives both content and mapping, and `remote_write = 1` additionally publishes them unless
//! `globals.offline`/`local` vetoes it. There is no local-then-remote fallback — that is a read
//! concept.
//!
//! Per-item behaviour:
//! - `partition == Partition::default()`, a zero `key`, or `data.len > 0 && data.ptr == NULL`:
//!   rejects with `InvalidArguments`; other items run independently.
//! - `data.len == 0`: **removes** the mapping, by setting the key to the zero hash the mutable
//!   store reads as a tombstone. `get_resolved` then reports `ADDRESS_NOT_FOUND`, and the
//!   terminal event carries the zero address. With `remote_write = 0` this evicts only the
//!   *local* mapping, which is a cache rather than an authority, so a remotely published key
//!   reappears on the next resolve; deleting one requires `remote_write = 1`.
//! - Otherwise: `write_resolved`, and the stored address is reported in `PUT_ITEM_COMPLETE`.
//!
//! Emits the same `PUT_ITEM_COMPLETE` as `lore_storage_put`: `address` is the content the key now
//! resolves to, ready to hand to `get`, and `stored_local` / `stored_remote` report where it
//! landed. `stored_remote` gates the remote mapping — a failed upload still leaves a good local
//! write, so the publish is skipped rather than name content the server does not hold. A caller
//! needing the key visible remotely checks that field, not the error detail.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreBytes;
use lore_revision::interface::LoreArray;
use lore_storage::StorageError;
use lore_storage::options::WriteOptions;
use lore_storage::write::write_resolved;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::PutItemOutcome;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::invalid_item;
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
    /// Borrowed view into caller memory; bytes must live until `Complete` fires. A zero-length
    /// buffer removes the key's mapping instead of publishing one
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
            let items = args.items.as_slice();

            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }

            let effective = store.effective_flags(per_call)?;
            let mut reuse = crate::storage::store::SessionReuse::default();

            crate::storage::fan_out_items!(items, "put_resolved", |item| {
                let session = reuse.session_for(
                    &store,
                    item.partition,
                    item.remote_write != 0 && !effective.no_remote,
                );
                let store = store.clone();
                async move { put_resolved_item(store, &item, session).await }
            })
        },
    )
    .await
}

/// Execute one item. Always emits a single `PUT_ITEM_COMPLETE` event.
async fn put_resolved_item(
    store: Arc<StoreInternal>,
    item: &LoreStoragePutResolvedItem,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    PutItemOutcome::emit(item.id, store_and_publish(store, item, session).await)
}

async fn store_and_publish(
    store: Arc<StoreInternal>,
    item: &LoreStoragePutResolvedItem,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<PutItemOutcome, StorageError> {
    if item.partition == Partition::default() {
        return Err(invalid_item("item names the default partition"));
    }

    if item.key == Hash::default() {
        return Err(invalid_item("item names the zero key"));
    }

    if item.data.len > 0 && item.data.ptr.is_null() {
        return Err(invalid_item(
            "item declares a non-empty buffer behind a null pointer",
        ));
    }

    let bytes = if item.data.len == 0 {
        Bytes::new()
    } else {
        // SAFETY:
        // - `item.data.ptr` is non-null (checked above) and the FFI contract requires
        //   `item.data.len` valid bytes behind it.
        // - The `'static` lifetime is fudged exactly as in `put`: the buffer's real lifetime is
        //   bounded by the call's `Complete` event, which `storage_call` only emits after the op's
        //   future, and every task it spawned, has resolved.
        let slice: &'static [u8] =
            unsafe { std::slice::from_raw_parts(item.data.ptr.cast::<u8>(), item.data.len) };
        Bytes::from_static(slice)
    };

    let mut write_options = WriteOptions::default();
    if item.fixed_size_chunk > 0 {
        write_options = write_options.with_fixed_size_chunk(item.fixed_size_chunk as usize);
    }
    if item.local_cache != 0 {
        write_options = write_options.with_local_cache_priority();
    }

    write_resolved(
        store.immutable.clone(),
        store.mutable.clone(),
        item.partition,
        item.key,
        item.context,
        bytes,
        write_options,
        remote_session,
        lore_revision::immutable::counted_write_context(),
    )
    .await
    .map(PutItemOutcome::from_write)
}
