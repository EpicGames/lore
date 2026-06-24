// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_metadata` — fetch fragment metadata without payload bytes.
//!
//! Each item resolves to a single terminal `GET_METADATA_ITEM_COMPLETE` event carrying
//! `{id, address, fragment, error_code}`. On success `error_code == None` and `fragment`
//! carries `flags`, `size_payload`, and `size_content`. On miss `error_code ==
//! ADDRESS_NOT_FOUND` and `fragment` is the default value.
//!
//! Items run in parallel via `JoinSet` (mirroring `lore_storage_get`). Per item the
//! resolution path is:
//! 1. Local probe via `ImmutableStore::query(partition, address, MatchFull)`. On exact match,
//!    emit the resolved `Fragment` and short-circuit.
//! 2. On local miss, fall through to the configured remote (if any) via
//!    `StorageSession::get_metadata`. The wire op carries no payload bytes — only Fragment.
//! 3. On remote miss or no remote configured, emit `ADDRESS_NOT_FOUND`.
//!
//! Short-circuits: `address.hash == Hash::default()` emits an empty `Fragment` with
//! `error_code = None` and no store work — symmetric with `lore_storage_get`.
//!
//! Successful remote fetches are not cached locally — there is no payload to cache, and
//! re-fetching metadata is cheap.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::store::event::LoreStorageGetMetadataItemCompleteEventData;
use lore_storage::store_types::StoreMatch;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// One `get_metadata` item — the `(partition, address)` to look up.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize)]
pub struct LoreStorageGetMetadataItem {
    /// Caller-chosen id echoed back in `GET_METADATA_ITEM_COMPLETE`
    pub id: u64,
    /// Partition to look up; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Content address to look up; `hash == Hash::default()` short-circuits to an empty fragment
    pub address: Address,
}

/// Arguments for `lore_storage_get_metadata`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(get_metadata_local)]
pub struct LoreStorageGetMetadataArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Addresses to look up; each runs independently and emits its own `GET_METADATA_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageGetMetadataItem>,
}

#[error_set]
enum GetMetadataError {
    InvalidArguments,
}

impl EventError for GetMetadataError {
    fn translated(&self) -> LoreError {
        match self {
            GetMetadataError::InvalidArguments(_) => LoreError::InvalidArguments,
            GetMetadataError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Fetch fragment metadata for one or more addresses without paying the payload bytes.
pub async fn get_metadata(
    globals: LoreGlobalArgs,
    args: LoreStorageGetMetadataArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, get_metadata_local).await
}

async fn get_metadata_local(
    globals: LoreGlobalArgs,
    args: LoreStorageGetMetadataArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        get_metadata,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), GetMetadataError>(());
            }
            let effective = store.effective_flags(per_call)?;

            let total = items.len();
            // Local hits and rejections emit their terminal event in-line and push their code
            // straight into `codes`; only items that miss locally spawn into the JoinSet.
            let mut remote_tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            let mut codes: Vec<LoreErrorCode> = Vec::with_capacity(total);
            let mut reuse = crate::storage::store::SessionReuse::default();

            for item in items {
                if effective.no_local {
                    if let Some(session) = reuse.session_for(&store, item.partition, true) {
                        lore_spawn!(
                            remote_tasks,
                            async move { resolve_remote(session, item).await }
                        );
                    } else {
                        codes.push(emit_complete(
                            &item,
                            Fragment::default(),
                            LoreErrorCode::AddressNotFound,
                        ));
                    }
                    continue;
                }

                match resolve_local(&store, &item).await {
                    LocalOutcome::Done { code } => codes.push(code),
                    LocalOutcome::NeedRemote => {
                        if effective.no_remote {
                            codes.push(emit_complete(
                                &item,
                                Fragment::default(),
                                LoreErrorCode::AddressNotFound,
                            ));
                        } else if let Some(session) =
                            reuse.session_for(&store, item.partition, true)
                        {
                            lore_spawn!(remote_tasks, async move {
                                resolve_remote(session, item).await
                            });
                        } else {
                            codes.push(emit_complete(
                                &item,
                                Fragment::default(),
                                LoreErrorCode::AddressNotFound,
                            ));
                        }
                    }
                }
            }

            codes.extend(crate::storage::drain_codes(remote_tasks).await);
            crate::storage::build_call_error(&codes, total, "get_metadata")
        },
    )
    .await
}

/// Outcome of the in-line local probe: either the item is fully resolved (and its terminal
/// event already emitted), or it missed locally and the caller should consult the remote.
enum LocalOutcome {
    Done { code: LoreErrorCode },
    NeedRemote,
}

/// Local probe — runs in the calling task without spawning. Emits the terminal event on hit,
/// on invalid args, on zero-hash short-circuit, or on a non-not-found local error. Returns
/// `NeedRemote` only when the item missed locally with `AddressNotFound` and may still be
/// satisfied by the remote.
async fn resolve_local(
    store: &Arc<StoreInternal>,
    item: &LoreStorageGetMetadataItem,
) -> LocalOutcome {
    if item.partition == Partition::default() {
        let code = emit_complete(item, Fragment::default(), LoreErrorCode::InvalidArguments);
        return LocalOutcome::Done { code };
    }

    if item.address.hash == Hash::default() {
        let code = emit_complete(item, Fragment::default(), LoreErrorCode::None);
        return LocalOutcome::Done { code };
    }

    match store
        .immutable
        .clone()
        .query(item.partition, item.address, StoreMatch::MatchFull)
        .await
    {
        Ok(result) if result.match_made == StoreMatch::MatchFull => {
            let code = emit_complete(item, result.fragment, LoreErrorCode::None);
            LocalOutcome::Done { code }
        }
        Ok(_) => LocalOutcome::NeedRemote,
        Err(err) if err.is_address_not_found() => LocalOutcome::NeedRemote,
        Err(err) => {
            // Non-not-found local errors (slow-down, internal) shouldn't be masked by a
            // remote attempt — the operator wants to see them.
            let code = emit_complete(
                item,
                Fragment::default(),
                crate::storage::store_error_to_code(&err),
            );
            LocalOutcome::Done { code }
        }
    }
}

/// Remote-only resolution — runs in a spawned task per item that missed locally. Emits the
/// terminal event with the wire-fetched Fragment on success, or a mapped error code via the
/// canonical `protocol_error_to_storage` → `storage_error_to_code` chain on any failure.
async fn resolve_remote(
    session: Arc<lore_transport::StorageSession>,
    item: LoreStorageGetMetadataItem,
) -> LoreErrorCode {
    match session.get_metadata(&item.address).await {
        Ok(fragment) => emit_complete(&item, fragment, LoreErrorCode::None),
        Err(err) => {
            let storage_err = lore_storage::error::protocol_error_to_storage(err, item.address);
            emit_complete(
                &item,
                Fragment::default(),
                crate::storage::storage_error_to_code(&storage_err),
            )
        }
    }
}

/// Emit the item's terminal event and return the `error_code` that was sent, so callers can
/// `return emit_complete(..)` directly.
fn emit_complete(
    item: &LoreStorageGetMetadataItem,
    fragment: Fragment,
    error_code: LoreErrorCode,
) -> LoreErrorCode {
    let address = if error_code == LoreErrorCode::None {
        item.address
    } else {
        Address::default()
    };
    LoreEvent::StorageGetMetadataItemComplete(LoreStorageGetMetadataItemCompleteEventData {
        id: item.id,
        address,
        fragment,
        error_code,
    })
    .send();
    error_code
}
