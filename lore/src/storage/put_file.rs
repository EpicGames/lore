// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_put_file` — store a file's contents at a content address.
//!
//! Per item:
//! - `partition == Partition::default()` or an empty `path` → `INVALID_ARGUMENTS`.
//! - a `path` that does not exist or does not name a regular file → `INVALID_ARGUMENTS`, rejected
//!   on the first open rather than after the transient-failure back-off.
//! - a zero-length file → zero-hash short-circuit; the file is opened but never read.
//! - otherwise: `write_from_file` produces a single top-level address which lands in
//!   `PUT_ITEM_COMPLETE`.
//!
//! When `item.remote_write != 0` and the handle has a remote configured, the per-item
//! storage session is passed through to `write_from_file` so the local write and the remote
//! upload run within the call's lifecycle and `PUT_ITEM_COMPLETE` only fires after both
//! terminate — same contract as `lore_storage_put`.

use std::path::Path;
use std::sync::Arc;

use lore_base::types::Context;
use lore_base::types::Partition;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_storage::StorageError;
use lore_storage::options::WriteOptions;
use lore_storage::write::write_from_file;
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

/// One `put_file` item — read the file at `path` and store it at
/// `(partition, context)`.
#[repr(C)]
#[derive(Clone, PartialEq, Default, Deserialize, Serialize, ValidateText)]
pub struct LoreStoragePutFileItem {
    /// Caller-chosen id echoed back in `PUT_ITEM_COMPLETE`
    pub id: u64,
    /// Target partition; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Dedup tag stored alongside the content hash in the resulting address
    pub context: Context,
    /// Source path; empty, missing, or non-file rejects with `INVALID_ARGUMENTS`; a zero-length
    /// file maps to the zero-hash address
    pub path: LoreString,
    /// Opt into remote upload — honored on the remote path, ignored local-only
    pub remote_write: u8,
    /// Tag the resulting fragment with `PayloadLocalCachePriority` so future remote reads always cache it locally
    pub local_cache: u8,
    /// Leaf fragment size cap for large files; `0` lets `write_content` choose
    pub fixed_size_chunk: u64,
}

impl core::fmt::Debug for LoreStoragePutFileItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStoragePutFileItem")
            .field("id", &self.id)
            .field("path", &self.path.as_str())
            .field("remote_write", &self.remote_write)
            .field("fixed_size_chunk", &self.fixed_size_chunk)
            .finish()
    }
}

/// Arguments for `lore_storage_put_file`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(put_file_local)]
pub struct LoreStoragePutFileArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Files to store; each runs independently and emits its own `PUT_ITEM_COMPLETE`
    pub items: LoreArray<LoreStoragePutFileItem>,
}

/// Read one or more files into the content-addressed store.
pub async fn put_file(
    globals: LoreGlobalArgs,
    args: LoreStoragePutFileArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, put_file_local).await
}

async fn put_file_local(
    globals: LoreGlobalArgs,
    args: LoreStoragePutFileArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        put_file,
        async move |store, args| {
            let items = args.items.as_slice();
            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }
            let effective = store.effective_flags(per_call)?;
            let mut reuse = crate::storage::store::SessionReuse::default();

            crate::storage::fan_out_items!(items, "put_file", |item| {
                let session = reuse.session_for(
                    &store,
                    item.partition,
                    item.remote_write != 0 && !effective.no_remote,
                );
                let store = store.clone();
                async move { put_file_item(store, &item, session).await }
            })
        },
    )
    .await
}

async fn put_file_item(
    store: Arc<StoreInternal>,
    item: &LoreStoragePutFileItem,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    PutItemOutcome::emit(item.id, resolve_put_file_item(store, item, session).await)
}

async fn resolve_put_file_item(
    store: Arc<StoreInternal>,
    item: &LoreStoragePutFileItem,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<PutItemOutcome, StorageError> {
    if item.partition == Partition::default() {
        return Err(invalid_item("item names the default partition"));
    }

    let path_str = item.path.as_str();
    if path_str.is_empty() {
        return Err(invalid_item("item has an empty path"));
    }
    let mut write_options = WriteOptions::default();
    if item.fixed_size_chunk > 0 {
        write_options = write_options.with_fixed_size_chunk(item.fixed_size_chunk as usize);
    }
    if item.local_cache != 0 {
        write_options = write_options.with_local_cache_priority();
    }

    write_from_file(
        store.immutable.clone(),
        item.partition,
        &lore_storage::ContentSource::file(Path::new(path_str)),
        item.context,
        write_options,
        remote_session,
        lore_revision::immutable::counted_write_context(),
    )
    .await
    .map(PutItemOutcome::from_write)
}
