// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_file` — write content at an address to a file.
//!
//! Per item:
//! - `partition == Partition::default()` → `INVALID_ARGUMENTS`.
//! - `address.hash == Hash::default()` → create/truncate the target to zero bytes; success with
//!   `error_code = NONE`.
//! - missing content → `ADDRESS_NOT_FOUND`.
//! - file write failure → `INTERNAL`.
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

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::store::event::LoreStorageGetItemCompleteEventData;
use lore_storage::read::read_into_file;
use lore_storage::read::write_all_to_file;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
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

#[error_set]
enum GetFileError {
    InvalidArguments,
}

impl EventError for GetFileError {
    fn translated(&self) -> LoreError {
        match self {
            GetFileError::InvalidArguments(_) => LoreError::InvalidArguments,
            GetFileError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
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
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), GetFileError>(());
            }
            let effective = store.effective_flags(per_call)?;
            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let session = reuse.session_for(&store, item.partition, !effective.no_remote);
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    get_file_item(store, item, effective, session).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "get_file")
        },
    )
    .await
}

async fn get_file_item(
    store: Arc<StoreInternal>,
    item: LoreStorageGetFileItem,
    effective: crate::storage::store::EffectiveFlags,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    let error_code = resolve_get_file_item(store, &item, effective, session).await;
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
    error_code
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
) -> LoreErrorCode {
    if item.partition == Partition::default() {
        return LoreErrorCode::InvalidArguments;
    }
    let path_str = item.path.as_str();
    if path_str.is_empty() {
        return LoreErrorCode::InvalidArguments;
    }
    if item.address.hash == Hash::default() {
        return match write_all_to_file(Path::new(path_str), bytes::Bytes::new(), false).await {
            Ok(_) => LoreErrorCode::None,
            Err(_) => LoreErrorCode::Internal,
        };
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
        Ok((fragment, _)) if item.offset > fragment.size_content => LoreErrorCode::InvalidArguments,
        Ok(_) => LoreErrorCode::None,
        Err(err) => crate::storage::storage_error_to_code(&err),
    }
}
