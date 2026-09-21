// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_put` — write content-addressed buffers to a store.
//!
//! Per-item behaviour:
//! - `data.len == 0`: zero-hash short-circuit; no storage work, the terminal event carries
//!   `address = (Hash::default(), item.context)` and an empty error detail.
//! - `data.len > 0 && data.ptr == NULL`: rejects with `InvalidArguments`; other items run
//!   independently.
//! - `partition == Partition::default()`: rejects with `InvalidArguments`.
//! - Otherwise: `write_content` with `remote_session = None` and `WriteOptions` derived from the
//!   item's `fixed_size_chunk`; the computed address is reported back in `PUT_ITEM_COMPLETE`.
//!
//! All per-item work is awaited before the closure returns, so none of it outlives the call.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreBytes;
use lore_revision::interface::LoreArray;
use lore_storage::StorageError;
use lore_storage::options::WriteOptions;
use lore_storage::write::write_content;
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

/// One put item — a buffer to hash and store at `(partition, context)`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreStoragePutItem {
    /// Caller-chosen id echoed back in `PUT_ITEM_COMPLETE`
    pub id: u64,
    /// Target partition; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Dedup tag stored alongside the content hash in the resulting address
    pub context: Context,
    /// Borrowed view into caller memory; bytes must live until `Complete` fires
    pub data: LoreBytes,
    /// Opt into remote upload — honored on the remote path, ignored local-only
    pub remote_write: u8,
    /// Tag the fragment with `PayloadLocalCachePriority` so future remote reads always cache it locally
    pub local_cache: u8,
    /// Leaf fragment size cap for large buffers; `0` lets `write_content` choose. Ignored
    /// for buffers under `FRAGMENT_SIZE_THRESHOLD`
    pub fixed_size_chunk: u64,
}

impl core::fmt::Debug for LoreStoragePutItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStoragePutItem")
            .field("id", &self.id)
            .field("data_len", &self.data.len)
            .field("remote_write", &self.remote_write)
            .field("fixed_size_chunk", &self.fixed_size_chunk)
            .finish()
    }
}

/// Arguments for `lore_storage_put`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(put_local)]
pub struct LoreStoragePutArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Buffers to store; each runs independently and emits its own `PUT_ITEM_COMPLETE`
    pub items: LoreArray<LoreStoragePutItem>,
}

/// Store one or more content-addressed buffers.
pub async fn put(
    globals: LoreGlobalArgs,
    args: LoreStoragePutArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, put_local).await
}

async fn put_local(
    globals: LoreGlobalArgs,
    args: LoreStoragePutArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        put,
        async move |store, args| {
            let items = args.items.as_slice();

            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }

            let effective = store.effective_flags(per_call)?;
            let mut reuse = crate::storage::store::SessionReuse::default();

            crate::storage::fan_out_items!(items, "put", |item| {
                let session = reuse.session_for(
                    &store,
                    item.partition,
                    item.remote_write != 0 && !effective.no_remote,
                );
                let store = store.clone();
                async move { put_item(store, &item, session).await }
            })
        },
    )
    .await
}

/// Execute one item. Always emits a single `PUT_ITEM_COMPLETE` event.
/// Returns the item's own error so the call-level reduction can pick the dominant failure.
async fn put_item(
    store: Arc<StoreInternal>,
    item: &LoreStoragePutItem,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    PutItemOutcome::emit(item.id, resolve_put_item(store, item, session).await)
}

async fn resolve_put_item(
    store: Arc<StoreInternal>,
    item: &LoreStoragePutItem,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<PutItemOutcome, StorageError> {
    if item.partition == Partition::default() {
        return Err(invalid_item("item names the default partition"));
    }

    if item.data.len == 0 {
        // Zero-hash short-circuit: succeeds without storing anything, so both placement flags
        // stay clear even though the item succeeded.
        return Ok(PutItemOutcome {
            address: Address {
                hash: Hash::default(),
                context: item.context,
            },
            stored_local: false,
            stored_remote: false,
        });
    }

    if item.data.ptr.is_null() {
        return Err(invalid_item(
            "item declares a non-empty buffer behind a null pointer",
        ));
    }

    // SAFETY:
    // - `item.data.ptr` is non-null (checked above) and the FFI contract requires
    //   `item.data.len` valid bytes behind it.
    // - The `'static` lifetime is fudged: the buffer's real lifetime is bounded by the
    //   call's `Complete` event. `storage_call` only emits `Complete` after the op's future, and
    //   every task it spawned, has resolved, so the slice outlives every read of the `Bytes`.
    //   `Bytes::from_static` stores ptr+len verbatim without trying to free the memory.
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

    write_content(
        store.immutable.clone(),
        item.partition,
        item.context,
        bytes,
        write_options,
        remote_session,
        lore_revision::immutable::counted_write_context(),
        None,
    )
    .await
    .map(PutItemOutcome::from_write)
}
