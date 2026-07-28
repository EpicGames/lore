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
pub mod metadata_get;
pub mod metadata_set;
pub mod modify;
pub mod move_node;
pub mod node_info;
pub mod node_path;
pub mod resolve_path;

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
}
