// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_file` — write content at an address to a file.
//!
//! Per item:
//! - `partition == Partition::default()` → `INVALID_ARGUMENTS`.
//! - `address.hash == Hash::default()` → create/truncate the target to zero bytes; success with
//!   an empty error detail.
//! - missing content → `ADDRESS_NOT_FOUND`.
//! - a file write failure reports its own error, `INTERNAL` where the filesystem gave no code.
//! - `offset` past the end of the content → `INVALID_ARGUMENTS`, with `path` left untouched.
//! - otherwise: `read_into_file` writes the reassembled payload.
//!
//! Ranges: `offset` and `length` select part of the content, `length = 0` meaning "to the
//! end"; a zeroed pair writes the whole content, as it always did. The file holds exactly the
//! requested range starting at its own first byte — it is *not* a sparse copy of the original
//! at the original offsets — and only the fragments the range covers are fetched.
//!
//! Unlike `get`, no `GET_HEADER` or `GET_DATA` events are emitted; only the terminal
//! `GET_ITEM_COMPLETE`.
//!
//! Multi-fragment writes go through a temp file at `<path>.loretmp` (or `<path>.<ext>.loretmp`
//! if `path` already has an extension); the rename to the final target is atomic. A failure
//! mid-write removes the temp file, so the target is either the finished range or untouched.

use std::path::Path;
use std::sync::Arc;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::store::event::LoreStorageGetItemCompleteEventData;
use lore_storage::StorageError;
use lore_storage::read::read_into_file;
use lore_storage::read::write_all_to_file;
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

/// One `get_file` item — read content at `(partition, address)` and
/// write it to the file at `path`.
#[repr(C)]
#[derive(Clone, PartialEq, Default, Deserialize, Serialize, ValidateText)]
pub struct LoreStorageGetFileItem {
    /// Caller-chosen id echoed back in `GET_ITEM_COMPLETE`
    pub id: u64,
    /// Partition to read from; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Content address to read; `hash == Hash::default()` truncates `path` to zero bytes
    pub address: Address,
    /// Destination path; empty rejects with `INVALID_ARGUMENTS`. Multi-fragment writes
    /// stage via `<path>.loretmp` then atomically rename
    pub path: LoreString,
    /// First content byte to write, counted from the start of the decompressed content.
    /// Past the end of the content rejects with `INVALID_ARGUMENTS`
    pub offset: u64,
    /// Content bytes to write from `offset`; `0` writes to the end. The file holds exactly the
    /// requested range starting at its own first byte, and is sized to it
    pub length: u64,
    /// Cache fetched fragments back to the local store, not just write them to `path`
    pub local_cache: u8,
}

impl core::fmt::Debug for LoreStorageGetFileItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStorageGetFileItem")
            .field("id", &self.id)
            .field("path", &self.path.as_str())
            .field("offset", &self.offset)
            .field("length", &self.length)
            .finish()
    }
}

/// Arguments for `lore_storage_get_file`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(get_file_local)]
pub struct LoreStorageGetFileArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Addresses and destination paths; each runs independently
    pub items: LoreArray<LoreStorageGetFileItem>,
}

/// Write one or more content-addressed payloads to filesystem paths.
pub async fn get_file(
    globals: LoreGlobalArgs,
    args: LoreStorageGetFileArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, get_file_local).await
}

async fn get_file_local(
    globals: LoreGlobalArgs,
    args: LoreStorageGetFileArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        get_file,
        async move |store, args| {
            let items = args.items.as_slice();
            if items.is_empty() {
                return Ok::<(), StorageError>(());
            }
            let effective = store.effective_flags(per_call)?;
            let mut reuse = crate::storage::store::SessionReuse::default();

            crate::storage::fan_out_items!(items, "get_file", |item| {
                let session = reuse.session_for(&store, item.partition, !effective.no_remote);
                let store = store.clone();
                async move { get_file_item(store, &item, effective, session).await }
            })
        },
    )
    .await
}

async fn get_file_item(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetFileItem,
    effective: crate::storage::store::EffectiveFlags,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    let result = resolve_get_file_item(store, item, effective, session).await;
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

/// Read one item's content into its file.
///
/// A start past the end of the content is a caller mistake rather than an empty read, as in `get`,
/// and is rejected rather than answered with an empty file. `read_into_file` leaves the target
/// alone in that case, so a destination that was already there survives.
async fn resolve_get_file_item(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetFileItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> Result<(), StorageError> {
    if item.partition == Partition::default() {
        return Err(invalid_item("item names the default partition"));
    }
    let path_str = item.path.as_str();
    if path_str.is_empty() {
        return Err(invalid_item("item has an empty path"));
    }
    if item.address.hash == Hash::default() {
        write_all_to_file(Path::new(path_str), bytes::Bytes::new(), false)
            .await
            .internal("writing the empty file for a zero-hash item")?;
        return Ok(());
    }

    let mut read_options = effective.read_options(remote_session.is_some());
    if item.local_cache != 0 {
        read_options = read_options.with_cache();
    }

    match read_into_file(
        store.immutable.clone(),
        item.partition,
        item.address,
        Path::new(path_str),
        ".loretmp",
        crate::storage::item_content_range(item.offset, item.length),
        read_options,
        remote_session,
    )
    .await
    {
        Ok((fragment, _)) if item.offset > fragment.size_content => {
            Err(offset_past_end(item.offset, fragment.size_content))
        }
        Ok(_) => Ok(()),
        Err(err) => Err(err),
    }
}
