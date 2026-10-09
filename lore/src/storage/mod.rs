// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Content-addressed storage API.
//!
//! Exposes the `ImmutableStore` / `MutableStore` primitives as a first-class
//! C ABI surface that can be driven without a filesystem working tree.
//!
//! C ABI types referenced by the API live in their original crates:
//! `Partition` in `lore_base::types`, `StoreMatch` in
//! `lore_storage::store_types`, `LoreBytes` and `LoreErrorDetail` in
//! `lore_revision::event`. The handle type [`handle::LoreStore`] is defined
//! here.
//!
//! # Item fan-out
//!
//! The item-taking entry points fan out through `fan_out_items`: one task per item for a batch of
//! several, the calling task for a batch of one. `lore_storage_copy` and
//! `lore_storage_get_metadata` fan out by hand — the first carries a per-item outcome wider than
//! the item's result, the second spawns only the items its local probe missed.
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

#[cfg(not(feature = "test-util"))]
pub(crate) mod call;
#[cfg(feature = "test-util")]
pub mod call;
pub mod close;
pub mod copy;
pub mod flush;
pub mod get;
pub mod get_file;
pub mod get_file_resolved;
pub mod get_metadata;
pub mod get_resolved;
pub mod handle;
pub mod mutable_compare_and_swap;
pub mod mutable_list;
pub mod mutable_load;
pub mod mutable_store;
pub mod obliterate;
pub mod open;
pub mod put;
pub mod put_file;
pub mod put_file_resolved;
pub mod put_resolved;
pub(crate) mod remote;
#[cfg(not(feature = "test-util"))]
pub(crate) mod store;
#[cfg(feature = "test-util")]
pub mod store;
pub mod upload;

use lore_base::types::Address;
use lore_error_set::prelude::*;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::store::event::LoreStoragePutItemCompleteEventData;
use lore_storage::StorageError;
use lore_storage::StoreResult;

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
/// Buffers across the IPC boundary: the `LoreBytes` a `lore_storage_put` or
/// `lore_storage_put_resolved` item views crosses as its bytes, which the service reads in the
/// message they arrived in. A get item's `data_out` names memory the service cannot write, so it
/// does not cross, and a relayed get answers with `GET_DATA` events instead.
///
/// Revision tree handles loaded against a drained storage handle are closed too, and this
/// is the only path that closes one for its caller: elsewhere a tree outlives its parent by
/// design, but a dropped connection leaves nobody to release it. The cascade runs per
/// storage handle, each draining its own trees in parallel.
pub async fn close_for_connection(connection_id: u64) {
    let entries = handle::drain_for_connection(connection_id);
    for (storage_handle_id, _) in &entries {
        crate::revision_tree::close_for_storage_handle(*storage_handle_id).await;
    }
    drain_in_parallel(entries).await;
}

/// Run the close sequence for each entry concurrently: every drain fires its own task so the
/// total wall time is bounded by the slowest drain, not the sum. The flush spawn inside is
/// already fire-and-forget; only the in-flight-counter await is parallelized here.
///
/// Exposed at `pub(crate)` so the unit tests can exercise the close logic on an explicit
/// entry list rather than the process-global registry — running the test against the live
/// registry would close handles owned by other concurrent tests.
#[lore_macro::test_pub]
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

/// The content range an item asks for, or `None` for the whole content.
///
/// `length == 0` reads to the end of the content, which makes a zeroed pair — what a caller
/// that has never heard of ranges passes, and what `Default` gives — mean the whole content.
/// So the range fields are inert until someone sets them.
///
/// Both fields are `u64` because content is: `Fragment::size_content` is a `u64` and a
/// repository may hold blobs past 4 GiB. Saturating rather than wrapping on the way down to
/// `usize` keeps a 32-bit target reading to the end of what it can address instead of wrapping
/// to a short read; the storage layer clamps to the content that exists either way.
pub(crate) fn item_content_range(offset: u64, length: u64) -> Option<std::ops::Range<usize>> {
    if offset == 0 && length == 0 {
        return None;
    }
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let end = if length == 0 {
        usize::MAX
    } else {
        start.saturating_add(usize::try_from(length).unwrap_or(usize::MAX))
    };
    Some(start..end)
}

/// What writing one item produced, in the shape `PUT_ITEM_COMPLETE` reports it.
///
/// Shared by `put`, `put_file`, `put_resolved` and `put_file_resolved`: each resolves an item to
/// exactly this and then emits one `LoreStoragePutItemCompleteEventData` from it. One type rather
/// than four identical tuples, so a field cannot be read out of position and a new field lands in
/// every op at once.
pub(crate) struct PutItemOutcome {
    /// Address the content is stored under.
    pub(crate) address: Address,
    /// Whether the local store holds the content.
    pub(crate) stored_local: bool,
    /// Whether the content reached the remote, or was already durable there. Named for the event
    /// field it feeds; the write path calls the same thing `stored_durable`.
    pub(crate) stored_remote: bool,
}

impl PutItemOutcome {
    /// The outcome of a completed write, whichever write function produced it — `write_content`,
    /// `write_from_file` and `write_resolved` all report a `StoreResult`.
    ///
    /// A remote leg that failed is not an error here: the write returns `Ok` as long as the local
    /// store took the content, and `stored_remote` is what tells the two apart.
    pub(crate) fn from_write(written: StoreResult) -> Self {
        Self {
            address: written.address,
            stored_local: written.stored_local,
            stored_remote: written.stored_durable,
        }
    }

    /// Emit the item's `PUT_ITEM_COMPLETE` and return the outcome that was sent. A failed item
    /// reports no address and neither placement flag.
    pub(crate) fn emit(id: u64, result: Result<Self, StorageError>) -> Result<(), StorageError> {
        let placed = result.as_ref().ok();
        LoreEvent::StoragePutItemComplete(LoreStoragePutItemCompleteEventData {
            id,
            address: placed.map_or_else(Address::default, |outcome| outcome.address),
            error: item_detail(&result),
            stored_local: u8::from(placed.is_some_and(|outcome| outcome.stored_local)),
            stored_remote: u8::from(placed.is_some_and(|outcome| outcome.stored_remote)),
        })
        .send();
        result.map(|_| ())
    }
}

/// An item rejected on its own arguments, before any store work. `reason` becomes the item event's
/// error message.
#[lore_macro::test_pub]
pub(crate) fn invalid_item(reason: impl Into<String>) -> StorageError {
    StorageError::from(lore_base::error::InvalidArguments {
        reason: reason.into(),
    })
}

/// The error for a range whose start lies beyond the content it names.
pub(crate) fn offset_past_end(offset: u64, size_content: u64) -> StorageError {
    invalid_item(format!(
        "item offset {offset} starts past the {size_content} byte content"
    ))
}

/// The detail an item's terminal event carries: the empty default on success, and on failure the
/// error's own FFI code, message and trace. The default detail allocates nothing, so only a
/// failure costs anything.
pub(crate) fn item_detail<T>(result: &Result<T, StorageError>) -> LoreErrorDetail {
    result
        .as_ref()
        .err()
        .map_or_else(LoreErrorDetail::default, LoreErrorDetail::from_error)
}

/// How actionable a failure is, highest first. A rejected argument is the caller's own bug and
/// ranks above everything. An internal failure points at the store or the server. `SlowDown` asks
/// for a retry. A lookup miss is the most expected failure, so it ranks last.
///
/// Every way a lookup can miss has to be named here. A remote miss arrives as `NotFound` or
/// `NoRemote` rather than `AddressNotFound`, and leaving those to the internal fallthrough would
/// let one absent key outrank, and so hide, a `SlowDown` raised by another item of the same batch.
fn severity(err: &StorageError) -> u8 {
    const CALLER_FIXABLE: u8 = 4;
    const INTERNAL: u8 = 3;
    const RETRYABLE: u8 = 2;
    const MISS: u8 = 1;

    if err.is_invalid_arguments() || err.is_oversized() {
        CALLER_FIXABLE
    } else if err.is_slow_down() {
        RETRYABLE
    } else if err.is_address_not_found()
        || err.is_payload_not_found()
        || err.is_not_found()
        || err.is_no_remote()
    {
        MISS
    } else {
        INTERNAL
    }
}

/// Run one batched op's items and reduce their outcomes to the call-level result.
///
/// A batch of several runs one task per item and awaits them all before returning; a batch of one
/// runs on the calling task. Spawning a single item would hand it to a worker thread and wait to be
/// woken — a thread round trip to do work the calling thread is already blocked waiting for, and one
/// address or key is the shape most calls arrive in. `LORE_CONTEXT` is a task-local and the work
/// stays in the caller's task, so it needs no propagating; `ObservedTask` is skipped because there
/// is no task to report the lifecycle of.
///
/// `$items` is the op's item slice. `$item` binds the item `$future` is to run: borrowed straight
/// out of `$items` for a batch of one, and an owned clone per spawned item, which a `'static` task
/// has to have. `$future` therefore takes it as `&$item`, and the per-item function takes the item
/// by reference — a batch of one then copies nothing, which for an item owning a `LoreString` would
/// otherwise cost an allocation on the path this exists to make cheap.
///
/// `$future` is evaluated once per item and must produce a `Send + 'static` future resolving to
/// that item's `Result<(), StorageError>`; per-item setup that borrows the op's locals — resolving
/// a session out of a `SessionReuse`, cloning the store — belongs inside it, as it runs before the
/// future is spawned. That setup must be infallible: a `?` or `return` part-way through the loop
/// would drop the `JoinSet` and abort the items already in flight.
///
/// Every spawned item is joined before returning, and a task that yields no result counts as an
/// internal failure, so no item's slot is lost.
///
/// A macro rather than a function because `ObservedTask` records `Location::caller()` and the server
/// labels its task metrics with it: expanding at the op's own line keeps one label per op, where a
/// shared function body would report every op at a single location.
macro_rules! fan_out_items {
    ($items:expr, $op_name:literal, |$item:ident| $future:expr) => {{
        let items = $items;
        let total = items.len();
        if let [single] = items {
            let $item = single;
            let mut outcomes = $crate::storage::ItemOutcomes::default();
            // `&$item` is a re-borrow only here, where the binding is already a reference; the
            // spawned arm needs that borrow to reach its owned clone.
            #[allow(clippy::needless_borrow)]
            let result = $future.await;
            outcomes.push(result);
            outcomes.into_call_result(total, $op_name)
        } else {
            let mut tasks: ::tokio::task::JoinSet<
                ::std::result::Result<(), ::lore_storage::StorageError>,
            > = ::tokio::task::JoinSet::new();
            for $item in items.iter().cloned() {
                ::lore_base::lore_spawn!(tasks, $future);
            }
            $crate::storage::ItemOutcomes::drain(tasks)
                .await
                .into_call_result(total, $op_name)
        }
    }};
}
pub(crate) use fan_out_items;

/// Reduces the per-item outcomes to the call-level result as they arrive.
///
/// Only the dominant failure and the failure count are kept, not every item's result. A
/// `StorageError` carries its own trace, so holding one per item would make the call's peak memory
/// follow the item count.
#[lore_macro::test_pub]
#[derive(Default)]
pub(crate) struct ItemOutcomes {
    failed: usize,
    dominant: Option<StorageError>,
}

impl ItemOutcomes {
    /// Fold one item's outcome in.
    #[lore_macro::test_pub]
    pub(crate) fn push(&mut self, result: Result<(), StorageError>) {
        if let Err(err) = result {
            self.failed += 1;
            self.consider(err);
        }
    }

    /// Fold another reduction in, for an op whose items resolve down two paths. The result matches
    /// pushing every item to one accumulator.
    #[lore_macro::test_pub]
    pub(crate) fn absorb(&mut self, other: Self) {
        self.failed += other.failed;
        if let Some(err) = other.dominant {
            self.consider(err);
        }
    }

    /// Keep `err` as the call's failure when it outranks what is held, by [`severity`]. A later
    /// failure of equal rank replaces the earlier one, so ties report the most recent.
    fn consider(&mut self, err: StorageError) {
        let outranks = self
            .dominant
            .as_ref()
            .is_none_or(|held| severity(&err) >= severity(held));
        if outranks {
            self.dominant = Some(err);
        }
    }

    /// Fold in every per-item task, turning a `JoinError` (task panic or cancellation) into an
    /// internal failure so no item's slot is lost.
    pub(crate) async fn drain(mut tasks: tokio::task::JoinSet<Result<(), StorageError>>) -> Self {
        let mut outcomes = Self::default();
        while let Some(joined) = tasks.join_next().await {
            outcomes.push(joined.unwrap_or_else(|err| {
                Err(StorageError::internal_with_context(
                    err,
                    "joining item task",
                ))
            }));
        }
        outcomes
    }

    /// The call-level result: the dominant failure, reported with its own code and message. How
    /// many items failed belongs to no single error, so it lands on the trace as context.
    #[lore_macro::test_pub]
    pub(crate) fn into_call_result(self, total: usize, op_name: &str) -> Result<(), StorageError> {
        let failed = self.failed;
        match self.dominant {
            None => Ok(()),
            Some(dominant) => Err(dominant).forward_with::<StorageError, _>(|| {
                format!("{failed}/{total} {op_name} items failed")
            }),
        }
    }
}
