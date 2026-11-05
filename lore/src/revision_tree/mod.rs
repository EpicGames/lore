// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Low-level memory-based revision control API.
//!
//! The `lore_revision_tree_*` namespace exposes a handle-based surface that
//! reads and constructs revisions directly in memory, keyed on opaque node
//! ids. The module groups one file per verb plus [`handle`] (POD type and
//! process-global registry) and [`call`] (the shared dispatcher, and the codes
//! a verb reports a failure of the State with).

pub mod add;
#[cfg(not(feature = "test-util"))]
pub(crate) mod call;
#[cfg(feature = "test-util")]
pub mod call;
pub mod close;
pub mod commit;
pub mod delete;
pub mod handle;
pub mod info;
pub mod list_children;
pub mod load;
pub mod metadata_clear;
pub mod metadata_get;
pub mod metadata_set;
pub mod modify;
pub mod move_node;
pub mod node_info;
pub mod node_path;
pub mod resolve_path;

use std::sync::Arc;
use std::time::Duration;

use crate::revision_tree::handle::RevisionTreeInternal;

/// Bound on a teardown's wait for in-flight ops. The budget shutdown allows per stage.
const TEARDOWN_DRAIN_WAIT: Duration = crate::SHUTDOWN_WAIT;

/// Close every registered revision tree handle: drain the registry, then mark each
/// invalid and await its in-flight counter. Returns once every handle is drained.
pub async fn close_all_handles() {
    drain_in_parallel(handle::drain_all()).await;
}

/// Close every revision tree handle loaded against `storage_handle_id`, waiting at most
/// [`TEARDOWN_DRAIN_WAIT`] for their in-flight ops.
///
/// Connection teardown reaches this through [`crate::storage::close_for_connection`], and
/// is the only path that closes a revision tree handle for its caller: elsewhere the
/// handle holds its own `Arc` to the store and stays usable after its parent closes.
///
/// Abandoning the wait is safe: the handles are unregistered before it starts, so an op
/// that outlives it completes into a tree nobody can reach.
pub(crate) async fn close_for_storage_handle(storage_handle_id: u64) {
    close_for_storage_handle_within(storage_handle_id, TEARDOWN_DRAIN_WAIT).await;
}

/// [`close_for_storage_handle`] with the bound supplied, so a test can reach the timeout
/// branch in milliseconds.
#[lore_macro::test_pub]
async fn close_for_storage_handle_within(storage_handle_id: u64, wait: Duration) {
    let entries = handle::drain_for_storage_handle(storage_handle_id);
    let count = entries.len();
    if count == 0 {
        return;
    }
    if tokio::time::timeout(wait, drain_in_parallel(entries))
        .await
        .is_err()
    {
        lore_base::lore_warn!(
            "Timed out draining {count} revision tree handle(s) on storage handle \
             {storage_handle_id} during connection teardown; they are unreachable but their \
             in-flight work is still running"
        );
    }
}

/// Mark each entry invalid and await its in-flight counter, concurrently, so the wall
/// time is the slowest drain rather than their sum.
///
/// No flush: the stores belong to the parent storage handle, whose own close flushes them.
#[lore_macro::test_pub]
pub(crate) async fn drain_in_parallel(entries: Vec<(u64, Arc<RevisionTreeInternal>)>) {
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    for (_, internal) in entries {
        lore_base::lore_spawn!(tasks, async move {
            internal.mark_invalid_and_await().await;
        });
    }
    while tasks.join_next().await.is_some() {}
}
