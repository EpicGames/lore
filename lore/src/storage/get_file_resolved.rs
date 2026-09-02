// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_file_resolved` — resolve a mutable key and write the content it names to a
//! file.
//!
//! `lore_storage_get_resolved` writing to a path rather than to the callback. Composing that with
//! `lore_storage_get_file` is not open to a caller holding a key rather than a hash, and would put
//! the whole content through its memory. Here neither side holds it: the resolve shares a round
//! trip with the read of the root fragment, and a fragment list is written leaf by leaf at its own
//! offset.
//!
//! Per item:
//! - `partition == Partition::default()`, a zero `key`, or an empty `path` → `INVALID_ARGUMENTS`.
//! - a key with no mapping, or one naming absent content → `ADDRESS_NOT_FOUND`, with `path` left
//!   untouched. There is no zero-hash truncation as in `lore_storage_get_file`: a resolve that
//!   finds nothing is a miss, not an address for empty content.
//! - `offset` past the end of the content → `INVALID_ARGUMENTS`, with `path` left untouched.
//! - file write failure → `INTERNAL`.
//!
//! Ranges and temp-file staging behave as in `lore_storage_get_file`. Only the terminal
//! `GET_ITEM_COMPLETE` is emitted, and its `address` is the *resolved* address, so a caller still
//! learns the key-to-hash mapping.
//!
//! Backend selection matches `lore_storage_get_resolved`: local first, remote on a miss, narrowed
//! by the handle's bound and the per-call `offline`/`local`/`remote` flags.

use std::path::Path;
use std::sync::Arc;

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
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::store::event::LoreStorageGetItemCompleteEventData;
use lore_storage::read::read_resolved_into_file;
use lore_transport::quic::storage_service::get_resolved_flags;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One `get_file_resolved` item — the mutable key to resolve and the file to write the content it
/// names to.
#[repr(C)]
#[derive(Clone, PartialEq, Default, Deserialize, Serialize, ValidateText)]
pub struct LoreStorageGetFileResolvedItem {
    /// Caller-chosen id echoed back in `GET_ITEM_COMPLETE`
    pub id: u64,
    /// Partition to resolve and read within; the zero/default partition rejects with
    /// `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Mutable key to resolve, always read as `KeyType::Resolve`; a zero key rejects with
    /// `INVALID_ARGUMENTS`
    pub key: Hash,
    /// Paired with the resolved hash to address the immutable read; the mutable store yields only
    /// a hash
    pub context: Context,
    /// Destination path; empty rejects with `INVALID_ARGUMENTS`. Multi-fragment writes stage via
    /// `<path>.loretmp` then atomically rename
    pub path: LoreString,
    /// First content byte to write, counted from the start of the decompressed content. Past the
    /// end of the content rejects with `INVALID_ARGUMENTS`
    pub offset: u64,
    /// Content bytes to write from `offset`; `0` writes to the end. The file holds exactly the
    /// requested range starting at its own first byte, and is sized to it
    pub length: u64,
    /// Cache fetched fragments and the mapping back to the local store, not just write the content
    /// to `path`
    pub local_cache: u8,
}

impl core::fmt::Debug for LoreStorageGetFileResolvedItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStorageGetFileResolvedItem")
            .field("id", &self.id)
            .field("path", &self.path.as_str())
            .field("offset", &self.offset)
            .field("length", &self.length)
            .field("local_cache", &self.local_cache)
            .finish()
    }
}

/// Arguments for `lore_storage_get_file_resolved`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(get_file_resolved_local)]
pub struct LoreStorageGetFileResolvedArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Keys to resolve and destination paths; each runs independently and emits its own
    /// `GET_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageGetFileResolvedItem>,
}

#[error_set]
enum GetFileResolvedError {
    InvalidArguments,
}

impl EventError for GetFileResolvedError {
    fn translated(&self) -> LoreError {
        match self {
            GetFileResolvedError::InvalidArguments(_) => LoreError::InvalidArguments,
            GetFileResolvedError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Resolve one or more mutable keys and write the content they name to filesystem paths.
pub async fn get_file_resolved(
    globals: LoreGlobalArgs,
    args: LoreStorageGetFileResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, get_file_resolved_local).await
}

async fn get_file_resolved_local(
    globals: LoreGlobalArgs,
    args: LoreStorageGetFileResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        get_file_resolved,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), GetFileResolvedError>(());
            }
            let effective = store.effective_flags(per_call)?;
            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let session = reuse.session_for(&store, item.partition, !effective.no_remote);
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    get_file_resolved_item(store, item, effective, session).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "get_file_resolved")
        },
    )
    .await
}

async fn get_file_resolved_item(
    store: Arc<StoreInternal>,
    item: LoreStorageGetFileResolvedItem,
    effective: crate::storage::store::EffectiveFlags,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    let (address, error_code) =
        resolve_get_file_resolved_item(store, &item, effective, session).await;
    LoreEvent::StorageGetItemComplete(LoreStorageGetItemCompleteEventData {
        id: item.id,
        address,
        error_code,
    })
    .send();
    error_code
}

/// Resolve and write one item. Reports the resolved address alongside the code, because unlike
/// `get_file` the address is an *output* here — the caller supplied a key, not a hash — and a
/// failure has none to report.
///
/// A start past the end of the content is a caller mistake rather than an empty read, as in
/// `get_file`, and is rejected rather than answered with an empty file. `read_resolved_into_file`
/// leaves the target alone in that case, so a destination that was already there survives.
async fn resolve_get_file_resolved_item(
    store: Arc<StoreInternal>,
    item: &LoreStorageGetFileResolvedItem,
    effective: crate::storage::store::EffectiveFlags,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> (Address, LoreErrorCode) {
    if item.partition == Partition::default() {
        return (Address::default(), LoreErrorCode::InvalidArguments);
    }
    if item.key == Hash::default() {
        return (Address::default(), LoreErrorCode::InvalidArguments);
    }
    let path_str = item.path.as_str();
    if path_str.is_empty() {
        return (Address::default(), LoreErrorCode::InvalidArguments);
    }

    let mut read_options = effective.read_options(remote_session.is_some());
    if item.local_cache != 0 {
        read_options = read_options.with_cache();
    }

    match read_resolved_into_file(
        store.immutable.clone(),
        store.mutable.clone(),
        item.partition,
        item.key,
        item.context,
        get_resolved_flags::NONE,
        Path::new(path_str),
        ".loretmp",
        crate::storage::item_content_range(item.offset, item.length),
        read_options,
        remote_session,
    )
    .await
    {
        Ok((_, fragment)) if item.offset > fragment.size_content => {
            (Address::default(), LoreErrorCode::InvalidArguments)
        }
        Ok((resolved, _)) => (
            Address {
                hash: resolved,
                context: item.context,
            },
            LoreErrorCode::None,
        ),
        Err(err) => (
            Address::default(),
            crate::storage::storage_error_to_code(&err),
        ),
    }
}
