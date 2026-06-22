// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_flush` — flush pending writes through the open handle.
//!
//! Disk-backed stores call `ImmutableStore::flush(sync_data)` followed by `MutableStore::flush`.
//! In-memory stores satisfy the trait with a no-op implementation, so the same path serves both
//! backends — the in-memory case completes successfully without touching disk.
//!
//! `sync_data` is sourced from `globals.sync_data` and routed straight into the trait method,
//! matching the close-time flush behavior.

use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::interface::LoreError;
use lore_revision::lore::execution_context;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;

/// Arguments for `lore_storage_flush`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(flush_local)]
pub struct LoreStorageFlushArgs {
    /// Open handle whose pending writes to flush
    pub handle: LoreStore,
}

#[error_set]
enum FlushError {}

impl EventError for FlushError {
    fn translated(&self) -> LoreError {
        match self {
            Self::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Flush pending writes through the handle's stores. Disk-backed stores
/// fsync; in-memory stores no-op.
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
            let sync_data = execution_context().globals().sync_data();
            store
                .immutable
                .clone()
                .flush(sync_data)
                .await
                .internal("immutable store flush")?;
            store
                .mutable
                .clone()
                .flush(sync_data)
                .await
                .internal("mutable store flush")?;
            Ok::<(), FlushError>(())
        },
    )
    .await
}
