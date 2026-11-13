// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_flush` — flush pending writes through the open handle.
//!
//! Disk-backed stores call `ImmutableStore::flush` followed by `MutableStore::flush`.
//! In-memory stores satisfy the trait with a no-op implementation, so the same path serves both
//! backends — the in-memory case completes successfully without touching disk.
//!
//! The flush options come from `globals.sync_data` and `globals.fast_flush`, as the close-time
//! flush's do.

use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::lore::execution_context;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StorageError;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;

/// Arguments for `lore_storage_flush`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(flush_local)]
pub struct LoreStorageFlushArgs {
    /// Open handle whose pending writes to flush
    pub handle: LoreStore,
}

/// Flush pending writes through the handle's stores. Disk-backed stores
/// write as `globals.sync_data` and `globals.fast_flush` ask; in-memory stores
/// no-op.
pub async fn flush(
    globals: LoreGlobalArgs,
    args: LoreStorageFlushArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, flush_local).await
}

async fn flush_local(
    globals: LoreGlobalArgs,
    args: LoreStorageFlushArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    storage_call(
        globals,
        callback,
        handle,
        args,
        flush,
        async move |store, _args| {
            let options = execution_context().globals().flush_options();
            flush_stores(&store.immutable, &store.mutable, options).await
        },
    )
    .await
}

/// Flushes `immutable`, then `mutable`, as `options` asks. The mutable store is flushed only once
/// the immutable store's flush has succeeded, since its values name content the immutable store
/// holds.
pub(crate) async fn flush_stores(
    immutable: &Arc<dyn ImmutableStore>,
    mutable: &Arc<dyn MutableStore>,
    options: FlushOptions,
) -> Result<(), StorageError> {
    immutable
        .clone()
        .flush(options)
        .await
        .forward_any::<StorageError>("immutable store flush")?;
    mutable
        .clone()
        .flush(options)
        .await
        .forward_any::<StorageError>("mutable store flush")
}
