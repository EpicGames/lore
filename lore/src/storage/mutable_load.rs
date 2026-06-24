// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_mutable_load` — read a mutable key's value.
//!
//! Each item targets either the local or the remote mutable store, selected the same way the
//! immutable ops select their backend: the default and `globals.local`/`globals.offline` act on
//! the handle's local mutable store; `globals.remote` (or a remote-bound handle) acts on the
//! remote store over the shared storage session — the same session-based authz the immutable
//! ops use. Each item resolves to one terminal `MUTABLE_LOAD_ITEM_COMPLETE` carrying `{id,
//! value, error_code}`; a key with no stored value reports `error_code == ADDRESS_NOT_FOUND`.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::store::event::LoreStorageMutableLoadItemCompleteEventData;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::EffectiveFlags;
use crate::storage::store::StoreInternal;

/// One `mutable_load` item — the `(partition, key, key_type)` to read.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize)]
pub struct LoreStorageMutableLoadItem {
    /// Caller-chosen id echoed back in `MUTABLE_LOAD_ITEM_COMPLETE`
    pub id: u64,
    /// Partition (repository) to read from; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Key to read
    pub key: Hash,
    /// Kind of value the key refers to
    pub key_type: KeyType,
}

/// Arguments for `lore_storage_mutable_load`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(mutable_load_impl)]
pub struct LoreStorageMutableLoadArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Keys to read; each runs independently and emits its own `MUTABLE_LOAD_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageMutableLoadItem>,
}

#[error_set]
enum MutableLoadError {
    InvalidArguments,
}

impl EventError for MutableLoadError {
    fn translated(&self) -> LoreError {
        match self {
            MutableLoadError::InvalidArguments(_) => LoreError::InvalidArguments,
            MutableLoadError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Read one or more mutable key values.
pub async fn mutable_load(
    globals: LoreGlobalArgs,
    args: LoreStorageMutableLoadArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, mutable_load_impl).await
}

async fn mutable_load_impl(
    globals: LoreGlobalArgs,
    args: LoreStorageMutableLoadArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        mutable_load,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), MutableLoadError>(());
            }
            let effective = store.effective_flags(per_call)?;
            if effective.no_local && store.remote.is_none() {
                return Err(MutableLoadError::from(InvalidArguments {
                    reason: "remote mutable_load requires a handle opened with `remote_config`"
                        .into(),
                }));
            }
            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let session = reuse.session_for(&store, item.partition, effective.no_local);
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    load_item(store, item, effective, session).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "mutable_load")
        },
    )
    .await
}

/// Resolve one load item against the selected backend. `effective.no_local` routes to the
/// remote mutable store via the handle's session; otherwise the local mutable store answers.
async fn load_item(
    store: Arc<StoreInternal>,
    item: LoreStorageMutableLoadItem,
    effective: EffectiveFlags,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> LoreErrorCode {
    if item.partition == Partition::default() {
        return emit_complete(&item, Hash::default(), LoreErrorCode::InvalidArguments);
    }

    if effective.no_local {
        let Some(session) = session else {
            return emit_complete(&item, Hash::default(), LoreErrorCode::Internal);
        };
        match session.mutable_load(&item.key, item.key_type).await {
            Ok(value) => emit_complete(&item, value, LoreErrorCode::None),
            Err(err) => emit_complete(
                &item,
                Hash::default(),
                crate::storage::protocol_error_to_code(&err),
            ),
        }
    } else {
        match store
            .mutable
            .clone()
            .load(item.partition, item.key, item.key_type)
            .await
        {
            Ok(value) => emit_complete(&item, value, LoreErrorCode::None),
            Err(err) => emit_complete(
                &item,
                Hash::default(),
                crate::storage::store_error_to_code(&err),
            ),
        }
    }
}

/// Emit the item's terminal event and return the `error_code` that was sent, so callers can
/// `return emit_complete(..)` directly.
fn emit_complete(
    item: &LoreStorageMutableLoadItem,
    value: Hash,
    error_code: LoreErrorCode,
) -> LoreErrorCode {
    let value = if error_code == LoreErrorCode::None {
        value
    } else {
        Hash::default()
    };
    LoreEvent::StorageMutableLoadItemComplete(LoreStorageMutableLoadItemCompleteEventData {
        id: item.id,
        value,
        error_code,
    })
    .send();
    error_code
}
