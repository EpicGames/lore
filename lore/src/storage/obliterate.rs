// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_obliterate` — delete content at `(partition, address)`.
//!
//! Each item drives `ImmutableStore::obliterate` locally and, when the handle has a remote,
//! the transport's `Admin::obliterate` in parallel. The two sides report independently into
//! `local_success` / `remote_success` on `OBLITERATE_ITEM_COMPLETE`; `error_code` is set when
//! either side fails. With no remote configured, `remote_success=1` reflects "no remote was
//! consulted, treat as no-op success".
//!
//! The op is idempotent on both sides: an address not present on a side reports success for
//! that side — the local store surfaces `AddressNotFound` and the transport surfaces
//! `ProtocolError::NotFound`; both are mapped to success at this layer.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::store::event::LoreStorageObliterateItemCompleteEventData;
use lore_storage::store_types::StoreObliterateStats;
use lore_transport::ProtocolError;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One obliterate item — the `(partition, address)` to delete.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize)]
pub struct LoreStorageObliterateItem {
    /// Caller-chosen id echoed back in `OBLITERATE_ITEM_COMPLETE`
    pub id: u64,
    /// Partition to delete from; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Content address to delete; absence on a side is idempotent success for that side
    pub address: Address,
}

/// Arguments for `lore_storage_obliterate`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(obliterate_local)]
pub struct LoreStorageObliterateArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Addresses to delete; each runs independently and emits its own `OBLITERATE_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageObliterateItem>,
}

#[error_set]
enum ObliterateError {
    InvalidArguments,
}

impl EventError for ObliterateError {
    fn translated(&self) -> LoreError {
        match self {
            ObliterateError::InvalidArguments(_) => LoreError::InvalidArguments,
            ObliterateError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Delete one or more `(partition, address)` entries.
pub async fn obliterate(
    globals: LoreGlobalArgs,
    args: LoreStorageObliterateArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, obliterate_local).await
}

async fn obliterate_local(
    globals: LoreGlobalArgs,
    args: LoreStorageObliterateArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        obliterate,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), ObliterateError>(());
            }
            let effective = store.effective_flags(per_call)?;
            let total = items.len();
            let mut tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            for item in items {
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    obliterate_item(store, item, effective).await
                });
            }
            let codes = crate::storage::drain_codes(tasks).await;
            crate::storage::build_call_error(&codes, total, "obliterate")
        },
    )
    .await
}

/// Per-leg outcome of an obliterate. `Skipped` means the leg was suppressed by bound or
/// per-call flags before any work ran; the caller distinguishes this from `Success` so the
/// terminal event reports the truth (`local_success=0, local_skipped=1` instead of a
/// misleading `local_success=1`).
enum LegOutcome {
    Success,
    Skipped,
    Failed(LoreErrorCode),
}

/// Resolve one obliterate item. With no remote configured the remote side is reported as
/// `Skipped`. With a remote, the local + remote calls run in parallel via `tokio::join!`
/// so neither blocks the other.
///
/// Bound flags suppress one side: `effective.no_remote` reports the remote leg as skipped;
/// `effective.no_local` reports the local leg as skipped. Skipped legs do not count as
/// errors, but they also do not falsely report success.
async fn obliterate_item(
    store: Arc<StoreInternal>,
    item: LoreStorageObliterateItem,
    effective: crate::storage::store::EffectiveFlags,
) -> LoreErrorCode {
    if item.partition == Partition::default() {
        return emit_complete(
            &item,
            LegOutcome::Failed(LoreErrorCode::InvalidArguments),
            LegOutcome::Skipped,
            LoreErrorCode::InvalidArguments,
        );
    }

    let store_for_local = store.clone();
    let local_fut = async move {
        if effective.no_local {
            return LegOutcome::Skipped;
        }
        let stats = Arc::new(StoreObliterateStats::default());
        match store_for_local
            .immutable
            .clone()
            .obliterate(item.partition, item.address, stats)
            .await
        {
            Ok(()) => LegOutcome::Success,
            // Idempotent: absent address is success.
            Err(err) if err.is_address_not_found() => LegOutcome::Success,
            Err(err) => LegOutcome::Failed(crate::storage::store_error_to_code(&err)),
        }
    };

    let remote_fut = async {
        if effective.no_remote {
            return LegOutcome::Skipped;
        }
        let Some(remote) = store.remote.clone() else {
            return LegOutcome::Skipped;
        };
        // The admin obliterate handler authorizes via JWT-scoped `can_obliterate`, not the
        // per-connection session map, so no `ensure_partition_authorized` round-trip needed.
        let admin = match remote.admin(item.partition).await {
            Ok(admin) => admin,
            Err(err) => {
                let storage_err = lore_storage::error::protocol_error_to_storage(err, item.address);
                return LegOutcome::Failed(crate::storage::storage_error_to_code(&storage_err));
            }
        };
        match admin.obliterate(item.address).await {
            Ok(()) | Err(ProtocolError::NotFound(_)) => LegOutcome::Success,
            Err(err) => {
                let storage_err = lore_storage::error::protocol_error_to_storage(err, item.address);
                if storage_err.is_address_not_found() {
                    LegOutcome::Success
                } else {
                    LegOutcome::Failed(crate::storage::storage_error_to_code(&storage_err))
                }
            }
        }
    };

    let (local_outcome, remote_outcome) = tokio::join!(local_fut, remote_fut);

    // Local errors win on tie because they're usually the more actionable signal (disk full,
    // lock contention) compared to typically-transient remote ones.
    let error_code = match (&local_outcome, &remote_outcome) {
        (LegOutcome::Failed(code), _) | (_, LegOutcome::Failed(code)) => *code,
        _ => LoreErrorCode::None,
    };
    emit_complete(&item, local_outcome, remote_outcome, error_code)
}

/// Emit the item's terminal event and return the `error_code` that was sent, so callers can
/// `return emit_complete(..)` directly.
fn emit_complete(
    item: &LoreStorageObliterateItem,
    local: LegOutcome,
    remote: LegOutcome,
    error_code: LoreErrorCode,
) -> LoreErrorCode {
    let local_success = u8::from(matches!(local, LegOutcome::Success));
    let local_skipped = u8::from(matches!(local, LegOutcome::Skipped));
    let remote_success = u8::from(matches!(remote, LegOutcome::Success));
    let remote_skipped = u8::from(matches!(remote, LegOutcome::Skipped));
    let address = if error_code == LoreErrorCode::None {
        item.address
    } else {
        Address::default()
    };
    LoreEvent::StorageObliterateItemComplete(LoreStorageObliterateItemCompleteEventData {
        id: item.id,
        address,
        local_success,
        remote_success,
        local_skipped,
        remote_skipped,
        error_code,
    })
    .send();
    error_code
}
