// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_put_file_resolved` — store a file's contents and publish a mutable key naming
//! them.
//!
//! `lore_storage_put_resolved` taking its content from a path rather than a buffer. Publish
//! ordering, last-writer-wins, `remote_write` and retraction are that operation's semantics; what
//! differs is the source and what it costs to read. A file at or below the fragment threshold is
//! read once into the single fragment it becomes, a larger one chunks straight off disk, so
//! residency follows the fragment rather than the file and the caller holds neither.
//!
//! Per item:
//! - `partition == Partition::default()`, a zero `key`, or an empty `path` → `INVALID_ARGUMENTS`.
//! - a `path` that does not exist or does not name a regular file → `INVALID_ARGUMENTS`, rejected
//!   on the first open rather than after the transient-failure back-off. It is never taken for a
//!   delete: a typo, or a directory whose reported size happens to be zero, would otherwise
//!   retract a live key.
//! - a zero-length file **retracts** `key`, exactly as a zero-length `data` does in
//!   `lore_storage_put_resolved`.
//! - otherwise: `write_resolved_from_file`, and the stored address is reported in
//!   `PUT_ITEM_COMPLETE`.

use std::path::Path;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
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
use lore_revision::store::event::LoreStoragePutItemCompleteEventData;
use lore_storage::options::WriteOptions;
use lore_storage::write::write_resolved_from_file;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::PutItemOutcome;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One `put_file_resolved` item — the file to store and the mutable key to publish it under.
#[repr(C)]
#[derive(Clone, PartialEq, Default, Deserialize, Serialize, ValidateText)]
pub struct LoreStoragePutFileResolvedItem {
    /// Caller-chosen id echoed back in `PUT_ITEM_COMPLETE`
    pub id: u64,
    /// Target partition; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Mutable key to publish the stored hash under; a zero key rejects with `INVALID_ARGUMENTS`
    pub key: Hash,
    /// Dedup tag stored alongside the content hash in the resulting address, and the context a
    /// later `get_file_resolved` must read the key at
    pub context: Context,
    /// Source path; empty, missing, or non-file rejects with `INVALID_ARGUMENTS`. A zero-length
    /// file removes the key's mapping instead of publishing one
    pub path: LoreString,
    /// Also publish the content and the mapping to the remote; ignored when the handle has no
    /// remote or the call is offline/local
    pub remote_write: u8,
    /// Tag the fragments with `PayloadLocalCachePriority` so future remote reads always cache them
    /// locally
    pub local_cache: u8,
    /// Leaf fragment size cap for large files; `0` lets the writer choose. Ignored for files under
    /// `FRAGMENT_SIZE_THRESHOLD`
    pub fixed_size_chunk: u64,
}

impl core::fmt::Debug for LoreStoragePutFileResolvedItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoreStoragePutFileResolvedItem")
            .field("id", &self.id)
            .field("path", &self.path.as_str())
            .field("remote_write", &self.remote_write)
            .field("local_cache", &self.local_cache)
            .field("fixed_size_chunk", &self.fixed_size_chunk)
            .finish()
    }
}

/// Arguments for `lore_storage_put_file_resolved`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(put_file_resolved_local)]
pub struct LoreStoragePutFileResolvedArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Files to store and publish; each runs independently and emits its own `PUT_ITEM_COMPLETE`
    pub items: LoreArray<LoreStoragePutFileResolvedItem>,
}

#[error_set]
enum PutFileResolvedError {
    InvalidArguments,
}

impl EventError for PutFileResolvedError {
    fn translated(&self) -> LoreError {
        match self {
            PutFileResolvedError::InvalidArguments(_) => LoreError::InvalidArguments,
            PutFileResolvedError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Store one or more files and publish a mutable key naming each.
pub async fn put_file_resolved(
    globals: LoreGlobalArgs,
    args: LoreStoragePutFileResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, put_file_resolved_local).await
}

async fn put_file_resolved_local(
    globals: LoreGlobalArgs,
    args: LoreStoragePutFileResolvedArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        put_file_resolved,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), PutFileResolvedError>(());
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
                    put_file_resolved_item(store, item, session).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "put_file_resolved")
        },
    )
    .await
}

async fn put_file_resolved_item(
    store: Arc<StoreInternal>,
    item: LoreStoragePutFileResolvedItem,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    let outcome = resolve_put_file_resolved_item(store, &item, session).await;
    LoreEvent::StoragePutItemComplete(LoreStoragePutItemCompleteEventData {
        id: item.id,
        address: outcome.address,
        error_code: outcome.error_code,
        stored_local: u8::from(outcome.stored_local),
        stored_remote: u8::from(outcome.stored_remote),
    })
    .send();
    outcome.error_code
}

async fn resolve_put_file_resolved_item(
    store: Arc<StoreInternal>,
    item: &LoreStoragePutFileResolvedItem,
    remote_session: Option<Arc<lore_transport::StorageSession>>,
) -> PutItemOutcome {
    if item.partition == Partition::default() {
        return PutItemOutcome::failed(LoreErrorCode::InvalidArguments);
    }

    if item.key == Hash::default() {
        return PutItemOutcome::failed(LoreErrorCode::InvalidArguments);
    }

    let path_str = item.path.as_str();
    if path_str.is_empty() {
        return PutItemOutcome::failed(LoreErrorCode::InvalidArguments);
    }
    let mut write_options = WriteOptions::default();
    if item.fixed_size_chunk > 0 {
        write_options = write_options.with_fixed_size_chunk(item.fixed_size_chunk as usize);
    }
    if item.local_cache != 0 {
        write_options = write_options.with_local_cache_priority();
    }

    PutItemOutcome::from_write(
        write_resolved_from_file(
            store.immutable.clone(),
            store.mutable.clone(),
            item.partition,
            item.key,
            item.context,
            Path::new(path_str),
            write_options,
            remote_session,
            lore_revision::immutable::counted_write_context(),
        )
        .await,
    )
}
