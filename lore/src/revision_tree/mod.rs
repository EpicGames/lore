// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Low-level memory-based revision control API.
//!
//! The `lore_revision_tree_*` namespace exposes a handle-based surface that
//! reads and constructs revisions directly in memory, keyed on opaque node
//! ids. The module groups one file per verb plus [`handle`] (POD type and
//! process-global registry) and [`call`] (the shared dispatcher).

pub mod add;
pub(crate) mod call;
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
pub(crate) async fn drain_in_parallel(entries: Vec<(u64, Arc<RevisionTreeInternal>)>) {
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    for (_, internal) in entries {
        lore_base::lore_spawn!(tasks, async move {
            internal.mark_invalid_and_await().await;
        });
    }
    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    /// Round-trip a `RevisionTreeInternal` through the registry: a fresh
    /// registration produces a non-zero handle; `lookup` returns the same
    /// `Arc`; `unregister` removes the entry so subsequent `lookup`
    /// returns `None`.
    #[tokio::test]
    async fn registry_register_lookup_unregister_round_trip() {
        use std::sync::Arc;

        use super::handle;
        use super::handle::test_support;

        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        assert_ne!(handle_value.handle_id, 0);
        let looked_up = handle::lookup(handle_value).expect("registered handle must look up");
        assert!(Arc::ptr_eq(&looked_up, &internal));
        let removed =
            handle::unregister(handle_value).expect("first unregister returns the held Arc");
        assert!(Arc::ptr_eq(&removed, &internal));
        assert!(handle::lookup(handle_value).is_none());
    }

    /// Unregistering an already-removed handle returns `None`. The second
    /// close call from the C side must see a defined miss, not a panic or
    /// a stale double-drop.
    #[tokio::test]
    async fn registry_double_unregister_returns_none() {
        use super::handle;
        use super::handle::test_support;

        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal);
        assert!(handle::unregister(handle_value).is_some());
        assert!(handle::unregister(handle_value).is_none());
    }

    /// The `INVALID` sentinel must never match a real registry entry.
    /// Lookup and unregister on it return `None` unconditionally.
    #[test]
    fn registry_invalid_sentinel_misses() {
        use super::handle;
        use super::handle::LoreRevisionTree;

        assert!(handle::lookup(LoreRevisionTree::INVALID).is_none());
        assert!(handle::unregister(LoreRevisionTree::INVALID).is_none());
    }

    /// Each call to `register` produces a distinct `handle_id`.
    /// Two concurrent registrations against the same `Arc` must not
    /// collide.
    #[tokio::test]
    async fn registry_two_registrations_produce_distinct_ids() {
        use super::handle;
        use super::handle::test_support;

        let a_internal = test_support::new_for_testing().await;
        let b_internal = test_support::new_for_testing().await;
        let a = handle::register(a_internal);
        let b = handle::register(b_internal);
        assert_ne!(a.handle_id, b.handle_id);
        handle::unregister(a);
        handle::unregister(b);
    }

    /// `RevisionTreeGuard::enter` increments the in-flight counter while
    /// the guard is live; dropping it decrements. Concurrent enters
    /// observe the counter at or above the number of live guards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn guard_increments_and_drops_decrement_in_flight_counter() {
        use std::sync::Arc;
        use std::sync::Barrier;
        use std::sync::atomic::Ordering;
        use std::thread;

        use super::handle;
        use super::handle::RevisionTreeGuard;
        use super::handle::test_support;

        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);

        const THREADS: usize = 8;
        let start = Arc::new(Barrier::new(THREADS + 1));
        let observed = Arc::new(Barrier::new(THREADS + 1));
        let release = Arc::new(Barrier::new(THREADS + 1));
        let mut joins = Vec::new();
        for _ in 0..THREADS {
            let start = start.clone();
            let observed = observed.clone();
            let release = release.clone();
            joins.push(thread::spawn(move || {
                start.wait();
                let guard = RevisionTreeGuard::enter(handle_value)
                    .expect("enter must succeed on a registered, non-invalid handle");
                observed.wait();
                release.wait();
                drop(guard);
            }));
        }
        start.wait();
        observed.wait();
        assert_eq!(internal.in_flight.load(Ordering::Acquire), THREADS as u64);
        release.wait();
        for j in joins {
            j.join().unwrap();
        }
        assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);
        handle::unregister(handle_value);
    }

    /// `RevisionTreeGuard::enter` returns `None` when the handle has
    /// already been marked invalid. The increment-then-check ordering
    /// ensures the counter is balanced even on the rejection path.
    #[tokio::test]
    async fn guard_enter_after_mark_invalid_returns_none() {
        use std::sync::atomic::Ordering;

        use super::handle;
        use super::handle::RevisionTreeGuard;
        use super::handle::test_support;

        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        internal.invalid.store(true, Ordering::Release);
        assert!(RevisionTreeGuard::enter(handle_value).is_none());
        assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);
        handle::unregister(handle_value);
    }

    /// `RevisionTreeGuard::enter` returns `None` when the handle is
    /// unknown (never registered or already unregistered).
    #[tokio::test]
    async fn guard_enter_unregistered_handle_returns_none() {
        use super::handle;
        use super::handle::RevisionTreeGuard;
        use super::handle::test_support;

        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal);
        handle::unregister(handle_value);
        assert!(RevisionTreeGuard::enter(handle_value).is_none());
    }

    /// The cascade takes the handles loaded against one storage handle and no others.
    /// Keyed on ids no other test can be using, since it sweeps the live registry.
    #[tokio::test]
    async fn drain_for_storage_handle_takes_only_the_handles_loaded_on_it() {
        use super::handle;
        use super::handle::test_support;

        const OWNER: u64 = 0xC105_E001;
        const SIBLING: u64 = 0xC105_E002;

        let first = handle::register(test_support::new_for_testing_on_storage_handle(OWNER).await);
        let second = handle::register(test_support::new_for_testing_on_storage_handle(OWNER).await);
        let other =
            handle::register(test_support::new_for_testing_on_storage_handle(SIBLING).await);

        let drained: Vec<u64> = handle::drain_for_storage_handle(OWNER)
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert_eq!(
            drained.len(),
            2,
            "both handles on the closing storage handle must drain, got {drained:?}"
        );
        assert!(drained.contains(&first.handle_id));
        assert!(drained.contains(&second.handle_id));
        assert!(handle::lookup(first).is_none());
        assert!(handle::lookup(second).is_none());
        assert!(
            handle::lookup(other).is_some(),
            "a handle loaded against another storage handle must survive",
        );

        handle::unregister(other);
    }

    /// The worker both close paths funnel through. Driven with an explicit entry list, so
    /// it cannot close handles other tests own.
    #[tokio::test]
    async fn drain_in_parallel_marks_each_handle_invalid() {
        use std::sync::atomic::Ordering;

        use super::drain_in_parallel;
        use super::handle::test_support;

        let first = test_support::new_for_testing().await;
        let second = test_support::new_for_testing().await;

        drain_in_parallel(vec![(1, first.clone()), (2, second.clone())]).await;

        for internal in [first, second] {
            assert!(
                internal.invalid.load(Ordering::Acquire),
                "every drained handle must be marked invalid",
            );
        }
    }

    /// An op that will not finish — a commit uploading over the connection that just
    /// dropped — must not hold the teardown open.
    #[tokio::test]
    async fn a_teardown_cascade_gives_up_on_an_op_that_will_not_finish() {
        use std::time::Duration;

        use super::close_for_storage_handle_within;
        use super::handle;
        use super::handle::RevisionTreeGuard;
        use super::handle::test_support;

        const OWNER: u64 = 0xC105_E005;

        let stuck = handle::register(test_support::new_for_testing_on_storage_handle(OWNER).await);
        let guard = RevisionTreeGuard::enter(stuck).expect("guard enter must succeed");

        close_for_storage_handle_within(OWNER, Duration::from_millis(20)).await;

        assert!(
            handle::lookup(stuck).is_none(),
            "the handle is unregistered before the wait, so the timeout still reclaims it",
        );
        drop(guard);
    }

    /// Shutdown must not tear down a handle out from under a call still running on
    /// it: the drain parks until the in-flight counter reaches zero.
    #[allow(clippy::disallowed_methods)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_in_parallel_waits_for_an_in_flight_op() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        use std::time::Instant;

        use super::drain_in_parallel;
        use super::handle;
        use super::handle::RevisionTreeGuard;
        use super::handle::test_support;

        let internal = test_support::new_for_testing().await;
        let handle_value = handle::register(internal.clone());
        let guard = RevisionTreeGuard::enter(handle_value).expect("guard enter must succeed");

        let entries = vec![(handle_value.handle_id, internal.clone())];
        let drain = tokio::spawn(async move { drain_in_parallel(entries).await });

        // A task that was never polled is also not finished, so wait for the drain to be
        // observably inside the await before asserting it is parked.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !internal.invalid.load(Ordering::Acquire) {
            if Instant::now() > deadline {
                panic!("the drain never reached the handle");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            !drain.is_finished(),
            "the drain must park while an op is in flight",
        );

        drop(guard);
        drain.await.expect("drain task join");

        handle::unregister(handle_value);
    }
}
