// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Content-addressed storage API.
//!
//! Exposes the `ImmutableStore` / `MutableStore` primitives as a first-class
//! C ABI surface that can be driven without a filesystem working tree.
//!
//! C ABI types referenced by the API live in their original crates:
//! `Partition` in `lore_base::types`, `StoreMatch` in
//! `lore_storage::store_types`, `LoreBytes` and `LoreErrorCode` in
//! `lore_revision::event`. The handle type [`handle::LoreStore`] is defined
//! here.
//!
//! # Callback contract
//!
//! Every entry point in this module accepts a `LoreEventCallback` that the runtime invokes
//! synchronously to deliver per-item events, the terminal `Complete`, and the final `End`.
//! The callback **must not re-enter the storage API on the same handle**. Re-entry —
//! invoking another `lore_storage_*` call from inside the callback — risks deadlock against
//! the in-flight counter and the per-handle dispatch path. Callers that need to chain ops
//! should record the result and dispatch from the caller's own thread after `End` fires.
//!
//! Re-entry on a different handle is allowed but discouraged: the second call's events
//! interleave with the first's on the caller's event stream and there is no runtime
//! enforcement that prevents the deadlock if both calls happen to share resources. Treat
//! the callback as a notification sink, not a control point.
//!
//! # Example
//!
//! ```ignore
//! use std::sync::Mutex;
//! use lore_revision::event::LoreEvent;
//! use lore_revision::interface::LoreEventCallback;
//!
//! let captured: std::sync::Arc<Mutex<Vec<LoreEvent>>> = Default::default();
//! let captured_for_cb = captured.clone();
//! let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
//!     // Record the event — DO NOT call back into lore::storage::* here.
//!     captured_for_cb.lock().unwrap().push(event.clone());
//! }));
//! // ... pass `callback` to lore::storage::put / get / etc.
//! ```

pub(crate) mod call;
pub mod close;
pub mod copy;
pub mod flush;
pub mod get;
pub mod get_file;
pub mod get_metadata;
pub mod handle;
pub mod mutable_compare_and_swap;
pub mod mutable_list;
pub mod mutable_load;
pub mod mutable_store;
pub mod obliterate;
pub mod open;
pub mod put;
pub mod put_file;
pub(crate) mod remote;
pub(crate) mod store;
pub mod upload;

use lore_base::error::InvalidArguments;
use lore_error_set::internal::SupportsInternalError;
use lore_revision::event::LoreErrorCode;
use lore_storage::StorageError;
use lore_storage::StoreError;
use lore_transport::ProtocolError;

/// Close every storage handle currently registered with the library.
///
/// Drains the registry atomically (no new ops can find the handles after this call enters)
/// and runs the close sequence — mark invalid, await in-flight counter, spawn flush — for
/// each one. Per-handle drains run in parallel: with N handles, wall time is the slowest
/// drain rather than the sum. Returns once every handle is invalidated and drained; the
/// per-handle flush tasks continue in the background and rely on the runtime staying alive
/// long enough for them to terminate. The library shutdown path calls this before tearing
/// down the runtime.
///
/// Per-handle flush spawns pass `sync_data = false` (no fsync). Blocking on per-store fsync
/// at shutdown would defeat the fire-and-forget contract; explicit `lore_storage_close`
/// calls remain the path for callers that want a sync'd flush.
pub async fn close_all_handles() {
    drain_in_parallel(handle::drain_all()).await;
}

/// Close every storage handle whose owning IPC connection matches `connection_id`.
///
/// The IPC dispatcher invokes this on connection teardown — when a server-side connection
/// drops without an explicit `lore_storage_close` for handles it opened, the dispatcher hands
/// the connection identifier here and the registry walks itself, draining matching entries
/// and running the close sequence on each. Client-mode handles (no connection id recorded)
/// are unaffected. Per-handle drains run in parallel.
///
/// IPC buffer-bearing args policy: `lore_storage_put`, `lore_storage_get`,
/// `lore_storage_put_file`, `lore_storage_get_file`, and `lore_storage_upload` all carry
/// `LoreBytes` views into caller memory that have no natural cross-process representation.
/// Their args fail to deserialize on the server side; the dispatcher must reject them with
/// `InvalidArguments` rather than attempt to round-trip the payload bytes through IPC.
/// Service-mode callers route those ops directly against the local backend.
pub async fn close_for_connection(connection_id: u64) {
    drain_in_parallel(handle::drain_for_connection(connection_id)).await;
}

/// Run the close sequence for each entry concurrently: every drain fires its own task so the
/// total wall time is bounded by the slowest drain, not the sum. The flush spawn inside is
/// already fire-and-forget; only the in-flight-counter await is parallelized here.
///
/// Exposed at `pub(crate)` so the unit tests can exercise the close logic on an explicit
/// entry list rather than the process-global registry — running the test against the live
/// registry would close handles owned by other concurrent tests.
pub(crate) async fn drain_in_parallel(entries: Vec<(u64, std::sync::Arc<store::StoreInternal>)>) {
    use tokio::task::JoinSet;
    let mut tasks: JoinSet<()> = JoinSet::new();
    for (_, store) in entries {
        lore_base::lore_spawn!(tasks, async move {
            store.mark_invalid_and_await().await;
            close::spawn_flush_stores(store.immutable.clone(), store.mutable.clone(), false);
        });
    }
    while tasks.join_next().await.is_some() {}
}

/// Map a `StorageError` to the external `LoreErrorCode` surface used by per-item completion
/// events. Shared by every op that goes through the higher-level storage pipeline so the
/// translation stays consistent. `SlowDown` surfaces back-pressure; oversized writes are
/// caller-fixable and reported as `InvalidArguments`; an address lookup miss maps to
/// `AddressNotFound`; everything else is `Internal`.
pub(crate) fn storage_error_to_code(err: &StorageError) -> LoreErrorCode {
    if err.is_slow_down() {
        LoreErrorCode::SlowDown
    } else if err.is_oversized() {
        LoreErrorCode::InvalidArguments
    } else if err.is_address_not_found() || err.is_payload_not_found() {
        LoreErrorCode::AddressNotFound
    } else {
        LoreErrorCode::Internal
    }
}

/// Map a low-level `StoreError` to the external `LoreErrorCode`. Variants align with
/// [`storage_error_to_code`]; `StoreError` is the trait-method error type and so does not carry
/// the `NotConnected` / `Disconnected` cases.
pub(crate) fn store_error_to_code(err: &StoreError) -> LoreErrorCode {
    if err.is_slow_down() {
        LoreErrorCode::SlowDown
    } else if err.is_oversized() {
        LoreErrorCode::InvalidArguments
    } else if err.is_address_not_found() || err.is_payload_not_found() {
        LoreErrorCode::AddressNotFound
    } else {
        LoreErrorCode::Internal
    }
}

/// Map a transport-layer `ProtocolError` to the external `LoreErrorCode`. Sites that
/// previously reached for `protocol_error_to_storage` followed by `storage_error_to_code`
/// just to fill out a per-item event can use this directly — the `StorageError` round-trip
/// only mattered when callers needed the address-bearing `AddressNotFound` struct.
/// `NotFound` and `NoRemote` both map to `AddressNotFound` because at this layer "the peer
/// doesn't have it / the peer isn't reachable for this address" is the actionable signal
/// callers want; transport-level back-pressure surfaces as `SlowDown`; everything else
/// (disconnected, internal, not-authorized, etc.) is `Internal`.
pub(crate) fn protocol_error_to_code(err: &ProtocolError) -> LoreErrorCode {
    if err.is_slow_down() {
        LoreErrorCode::SlowDown
    } else if err.is_oversized() {
        LoreErrorCode::InvalidArguments
    } else if err.is_not_found() || err.is_no_remote() {
        LoreErrorCode::AddressNotFound
    } else {
        LoreErrorCode::Internal
    }
}

/// Pick the most actionable per-item code from `codes` to use as the call-level error
/// summary. Severity ordering, most-actionable first:
///   `InvalidArguments` > `Internal` > `SlowDown` > `AddressNotFound`
///
/// The reasoning: `InvalidArguments` is a caller bug, the user wants to see it first. An
/// `Internal` failure points to a server- or store-side issue. `SlowDown` is a hint to
/// retry. `AddressNotFound` is the most expected, most graceful failure mode. Any
/// `LoreErrorCode::None` entries are skipped — we summarise only the failures.
///
/// Returns `None` if every entry is `None` (i.e. nothing failed).
pub(crate) fn aggregate_error_code(
    codes: impl IntoIterator<Item = LoreErrorCode>,
) -> Option<LoreErrorCode> {
    fn severity(code: LoreErrorCode) -> u8 {
        match code {
            LoreErrorCode::InvalidArguments => 4,
            LoreErrorCode::Internal => 3,
            LoreErrorCode::SlowDown => 2,
            LoreErrorCode::AddressNotFound => 1,
            LoreErrorCode::None => 0,
        }
    }
    codes
        .into_iter()
        .filter(|c| *c != LoreErrorCode::None)
        .max_by_key(|c| severity(*c))
}

/// Drain a `JoinSet<LoreErrorCode>` into a `Vec<LoreErrorCode>`, mapping `JoinError` (task
/// panic / cancellation) to `LoreErrorCode::Internal` so the per-item slot is never lost.
/// The capacity hint comes from the `JoinSet`'s pending count at entry, before any task has
/// joined, so it always matches the spawned-item total.
pub(crate) async fn drain_codes(
    mut tasks: tokio::task::JoinSet<LoreErrorCode>,
) -> Vec<LoreErrorCode> {
    let mut codes: Vec<LoreErrorCode> = Vec::with_capacity(tasks.len());
    while let Some(result) = tasks.join_next().await {
        codes.push(result.unwrap_or(LoreErrorCode::Internal));
    }
    codes
}

/// Build the call-level `Result<(), E>` from a per-item code slice plus two error-builder
/// closures. `op_name` lands in the `"{failed}/{total} {op_name} items failed"` reason
/// string. The dispatch:
/// - All entries `None` → `Ok(())`.
/// - Aggregate severity `InvalidArguments` → `Err(E::from(InvalidArguments { reason }))`.
/// - Anything else → `Err(E::internal(reason))`.
pub(crate) fn build_call_error<E: From<InvalidArguments> + SupportsInternalError>(
    codes: &[LoreErrorCode],
    total: usize,
    op_name: &str,
) -> Result<(), E> {
    let failed = codes.iter().filter(|c| **c != LoreErrorCode::None).count();
    match aggregate_error_code(codes.iter().copied()) {
        None => Ok(()),
        Some(LoreErrorCode::InvalidArguments) => Err(E::from(InvalidArguments {
            reason: format!("{failed}/{total} {op_name} items failed"),
        })),
        Some(_) => Err(E::internal(format!(
            "{failed}/{total} {op_name} items failed"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::handle;
    use crate::storage::store::StoreInternal;
    use crate::storage::store::in_memory_for_tests;

    /// `drain_in_parallel` is the worker `close_all_handles` and `close_for_connection`
    /// invoke after they collect entries. Driving it directly with a hand-built entry list
    /// avoids racing against other tests through the process-global registry. Each entry's
    /// store gets marked invalid and a flush task spawns.
    #[tokio::test]
    async fn drain_in_parallel_marks_each_store_invalid() {
        let s1 = in_memory_for_tests("drain-1").await;
        let s2 = in_memory_for_tests("drain-2").await;
        let w1 = Arc::downgrade(&s1);
        let w2 = Arc::downgrade(&s2);

        drain_in_parallel(vec![(1, s1), (2, s2)]).await;

        // The flush task holds an Arc clone, so the strong count may still be > 0 after the
        // drain returns — assert against the invalid flag instead.
        for w in [w1, w2] {
            let invalid = w
                .upgrade()
                .is_none_or(|s| s.invalid.load(std::sync::atomic::Ordering::Acquire));
            assert!(invalid, "store should be marked invalid after drain");
        }
    }

    /// `handle::drain_for_connection` returns only the entries whose `connection_id` matches.
    /// The full `close_for_connection` flow then funnels them through `drain_in_parallel`;
    /// this test verifies the registry-side filter without sweeping the live registry's
    /// other entries.
    #[tokio::test]
    async fn drain_for_connection_filter_only_returns_matching_entries() {
        let conn_7_a = build_connection_store("conn-7-a", 7).await;
        let conn_7_b = build_connection_store("conn-7-b", 7).await;
        let conn_8 = build_connection_store("conn-8", 8).await;
        let client = in_memory_for_tests("client").await;

        let h_a = handle::register(conn_7_a);
        let h_b = handle::register(conn_7_b);
        let h_8 = handle::register(conn_8);
        let h_client = handle::register(client);

        let drained = handle::drain_for_connection(7);
        let drained_ids: std::collections::HashSet<u64> =
            drained.iter().map(|(id, _)| *id).collect();
        assert!(drained_ids.contains(&h_a.handle_id));
        assert!(drained_ids.contains(&h_b.handle_id));
        assert!(!drained_ids.contains(&h_8.handle_id));
        assert!(!drained_ids.contains(&h_client.handle_id));
        assert!(handle::immutable_for_test(h_8).is_some());
        assert!(handle::immutable_for_test(h_client).is_some());

        for h in [h_8, h_client] {
            handle::unregister(h);
        }
    }

    async fn build_connection_store(identity: &str, connection_id: u64) -> Arc<StoreInternal> {
        let bare = in_memory_for_tests(identity).await;
        let immutable = bare.immutable.clone();
        let mutable = bare.mutable.clone();
        Arc::new(
            StoreInternal::new(
                identity,
                immutable,
                mutable,
                None,
                crate::storage::store::BoundFlags::default(),
            )
            .with_connection_id(connection_id),
        )
    }
}
