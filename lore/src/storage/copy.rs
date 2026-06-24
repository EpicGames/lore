// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_copy` — copy content between `(partition, context)` tuples in the same store.
//!
//! Each item rejects zero-partitions and identical destination tuples (same partition AND same
//! context) with `INVALID_ARGUMENTS`. Same partition with a different `target_context` IS a
//! valid request — that's the in-partition payload-duplication case where a single payload
//! gets tagged under a new dedup `Context` without ever moving the bytes.
//!
//! For remote-usable handles the op walks a three-tier ladder:
//! 1. **Server-side** — `StorageSession::copy` against the destination's session.
//! 2. **Upload-fallback** — entered only when tier 1 returns `NotFound` (source genuinely
//!    missing on the peer) or `NotAuthorized` (caller has lost access to the source server-side
//!    but may still have the bytes locally). Loads from the local store and uploads via
//!    `StorageSession::put`. Other tier-1 errors (transport-level, internal, oversized, …) are
//!    surfaced directly without a fallback attempt — they would just fail again over the same
//!    connection.
//! 3. **Fail** — no local payload, source genuinely gone → `ADDRESS_NOT_FOUND`.
//!
//! When tiers 1 or 2 succeed, the local entry is mirrored via `ImmutableStore::copy(.., durable=true)`
//! on a best-effort basis: any local-mirror failure is benign (the destination tuple is durable on
//! the peer; clients fetch on demand). Per-item failures are not surfaced to the caller; the
//! call-level closure tallies them and emits a single debug log when the batch finishes.
//!
//! Local-only handles take a single `ImmutableStore::copy(.., durable=false)` step: source's
//! payload pointer and content-describing flags are adopted (encoding follows the bytes it
//! describes, otherwise reads decode against the wrong codec); source's `PayloadStoredDurable`
//! is masked off and the target's pre-existing flag (if any) is preserved.
//!
//! `COPY_ITEM_COMPLETE` carries the original `(source_partition, target_partition,
//! source_address, target_context)` so callers can correlate failures without re-walking
//! their input array.
//! Idempotency falls out of the underlying store and the server: a second copy with the same
//! arguments produces the same flag state and yields no observable change.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::lore_debug;
use lore_revision::store::event::LoreStorageCopyItemCompleteEventData;
use lore_storage::options::ReadOptions;
use lore_storage::read::load_fragment;
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

/// One copy item — relocate content from `(source_partition, source_address)` to
/// `(target_partition, source_address.hash, target_context)`, preserving the content hash.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize)]
pub struct LoreStorageCopyItem {
    /// Caller-chosen id echoed back in `COPY_ITEM_COMPLETE`
    pub id: u64,
    /// Source partition; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub source_partition: Partition,
    /// Destination partition; zero/default rejects, as does an exact `(source_partition, source
    /// context)` match (no-op) — a different `target_context` enables in-partition duplication
    pub target_partition: Partition,
    /// Source content address; its `hash` carries over to the destination address unchanged
    pub source_address: Address,
    /// Dedup tag for the destination address `(target_partition, source_address.hash,
    /// target_context)`; may match the source tag or re-tag the payload
    pub target_context: Context,
}

/// Arguments for `lore_storage_copy`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(copy_local)]
pub struct LoreStorageCopyArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Copy requests; each runs independently and emits its own `COPY_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageCopyItem>,
}

#[error_set]
enum CopyError {
    InvalidArguments,
}

impl EventError for CopyError {
    fn translated(&self) -> LoreError {
        match self {
            CopyError::InvalidArguments(_) => LoreError::InvalidArguments,
            CopyError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Copy content between partitions for one or more items.
pub async fn copy(
    globals: LoreGlobalArgs,
    args: LoreStorageCopyArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, copy_local).await
}

async fn copy_local(
    globals: LoreGlobalArgs,
    args: LoreStorageCopyArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        copy,
        async move |store, args| {
            let items = args.items.as_slice().to_vec();
            if items.is_empty() {
                return Ok::<(), CopyError>(());
            }
            let effective = store.effective_flags(per_call)?;

            // Server keeps authorized_repos for the connection's lifetime, so authorize each
            // unique source partition once instead of paying the session-start round-trip per
            // item. source == target needs no separate authz — the destination session covers it.
            if store.remote.is_some() && !effective.no_remote {
                let mut unique_sources: std::collections::HashSet<Partition> =
                    std::collections::HashSet::new();
                for item in &items {
                    if item.source_partition != Partition::default()
                        && item.source_partition != item.target_partition
                    {
                        unique_sources.insert(item.source_partition);
                    }
                }
                for source_partition in unique_sources {
                    store.ensure_partition_authorized(source_partition).await;
                }
            }

            let total = items.len();
            let mut reuse = crate::storage::store::SessionReuse::default();
            let mut tasks: JoinSet<CopyOutcome> = JoinSet::new();
            for item in items {
                let session =
                    reuse.session_for(&store, item.target_partition, !effective.no_remote);
                let store = store.clone();
                lore_spawn!(tasks, async move {
                    copy_item(store, item, effective, session).await
                });
            }
            let mut codes: Vec<LoreErrorCode> = Vec::with_capacity(total);
            let mut local_mirror_errors = 0usize;
            while let Some(result) = tasks.join_next().await {
                let outcome = result.unwrap_or(CopyOutcome::failed(LoreErrorCode::Internal));
                codes.push(outcome.code);
                if outcome.local_mirror_failed {
                    local_mirror_errors += 1;
                }
            }
            if local_mirror_errors > 0 {
                lore_debug!(
                    "copy: {local_mirror_errors}/{total} items had benign local-mirror failures (remote was authoritative)"
                );
            }
            crate::storage::build_call_error(
                &codes,
                total,
                "copy",
            )
        },
    )
    .await
}

/// Per-item outcome carried back to the call's batch-level aggregator. `code` is the item-level
/// `LoreErrorCode` (`None` on success); `local_mirror_failed` is the swallowed local-copy
/// failure after a successful remote round-trip — see `mirror_local_durable`.
struct CopyOutcome {
    code: LoreErrorCode,
    local_mirror_failed: bool,
}

impl CopyOutcome {
    fn ok() -> Self {
        Self {
            code: LoreErrorCode::None,
            local_mirror_failed: false,
        }
    }

    fn failed(code: LoreErrorCode) -> Self {
        Self {
            code,
            local_mirror_failed: false,
        }
    }
}

/// Resolve one copy item. Same-partition and zero-partition cases are rejected at the
/// storage-API layer; the underlying `ImmutableStore::copy` handles the actual relocation.
async fn copy_item(
    store: Arc<StoreInternal>,
    item: LoreStorageCopyItem,
    effective: crate::storage::store::EffectiveFlags,
    session: Option<Arc<lore_transport::StorageSession>>,
) -> CopyOutcome {
    if item.source_partition == Partition::default()
        || item.target_partition == Partition::default()
    {
        return CopyOutcome::failed(emit_complete(&item, LoreErrorCode::InvalidArguments));
    }
    if item.source_partition == item.target_partition
        && item.source_address.context == item.target_context
    {
        return CopyOutcome::failed(emit_complete(&item, LoreErrorCode::InvalidArguments));
    }

    let Some(session) = session else {
        match store
            .immutable
            .clone()
            .copy(
                item.source_partition,
                item.source_address,
                item.target_partition,
                item.target_context,
                false,
            )
            .await
        {
            Ok(()) => {
                emit_complete(&item, LoreErrorCode::None);
                return CopyOutcome::ok();
            }
            Err(err) => {
                return CopyOutcome::failed(emit_complete(
                    &item,
                    crate::storage::store_error_to_code(&err),
                ));
            }
        }
    };

    match session
        .copy(
            item.source_partition,
            item.source_address,
            item.target_context,
        )
        .await
    {
        Ok(()) => return mirror_local_durable(&store, &item, effective).await,
        Err(ProtocolError::NotFound(_) | ProtocolError::NotAuthorized(_)) => {
            if effective.no_local {
                return CopyOutcome::failed(emit_complete(&item, LoreErrorCode::AddressNotFound));
            }
        }
        Err(err) => {
            return CopyOutcome::failed(emit_complete(
                &item,
                crate::storage::protocol_error_to_code(&err),
            ));
        }
    }

    // Pull the source from local only — never pull from a third party to satisfy a copy.
    let load = load_fragment(
        store.immutable.clone(),
        item.source_partition,
        item.source_address,
        ReadOptions::default().no_remote(),
        None,
    )
    .await;
    let (fragment, payload) = match load {
        Ok(pair) => pair,
        Err(err) if err.is_address_not_found() || err.is_payload_not_found() => {
            return CopyOutcome::failed(emit_complete(&item, LoreErrorCode::AddressNotFound));
        }
        Err(err) => {
            return CopyOutcome::failed(emit_complete(
                &item,
                crate::storage::storage_error_to_code(&err),
            ));
        }
    };
    if let Err(err) = session
        .put(item.source_address, fragment, Some(payload))
        .await
    {
        return CopyOutcome::failed(emit_complete(
            &item,
            crate::storage::protocol_error_to_code(&err),
        ));
    }
    mirror_local_durable(&store, &item, effective).await
}

/// Best-effort local mirror after a successful remote round-trip. The destination tuple is
/// already durable on the peer, so any local-copy failure (most commonly `AddressNotFound`
/// when source isn't locally present at all) is benign — clients can fetch the bytes on
/// demand. The op-level outcome is success either way; the local-failure flag is propagated
/// up so the call's aggregator can emit a single batch-level debug log instead of one per
/// item, keeping the log signal-to-noise high under high-frequency copy traffic.
///
/// Skipped entirely when the handle is bound to remote-only mode — there's no local cache
/// to mirror to.
async fn mirror_local_durable(
    store: &Arc<StoreInternal>,
    item: &LoreStorageCopyItem,
    effective: crate::storage::store::EffectiveFlags,
) -> CopyOutcome {
    if effective.no_local {
        emit_complete(item, LoreErrorCode::None);
        return CopyOutcome::ok();
    }
    let local_failed = store
        .immutable
        .clone()
        .copy(
            item.source_partition,
            item.source_address,
            item.target_partition,
            item.target_context,
            true,
        )
        .await
        .is_err();
    emit_complete(item, LoreErrorCode::None);
    CopyOutcome {
        code: LoreErrorCode::None,
        local_mirror_failed: local_failed,
    }
}

/// Emit the item's terminal event and return the `error_code` that was sent, so callers can
/// fold the emit into the return (e.g. `CopyOutcome::failed(emit_complete(..))`).
fn emit_complete(item: &LoreStorageCopyItem, error_code: LoreErrorCode) -> LoreErrorCode {
    let source_address = if error_code == LoreErrorCode::None {
        item.source_address
    } else {
        Address::default()
    };
    LoreEvent::StorageCopyItemComplete(LoreStorageCopyItemCompleteEventData {
        id: item.id,
        source_partition: item.source_partition,
        target_partition: item.target_partition,
        source_address,
        target_context: item.target_context,
        error_code,
    })
    .send();
    error_code
}
