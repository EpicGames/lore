// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Handle registry for the content-addressed storage API.
//!
//! Handles are opaque POD values handed to FFI callers. Each is a `u64`
//! drawn from a monotonic counter and indexed into a process-global
//! [`DashMap`] keyed by that id. The map's value is an
//! `Arc<StoreInternal>` — the underlying store is shared between the
//! registry entry and any in-flight ops that have already looked up the
//! handle and are holding an `Arc` clone.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dashmap::DashMap;

use crate::storage::store::StoreInternal;

/// Opaque handle to an open content-addressed storage instance.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LoreStore {
    /// Registry key; `0` is the reserved invalid/unregistered sentinel (zero-init = null handle)
    pub handle_id: u64,
}

impl LoreStore {
    pub const INVALID: Self = Self { handle_id: 0 };
}

static REGISTRY: LazyLock<DashMap<u64, Arc<StoreInternal>>> = LazyLock::new(DashMap::new);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Register a store and receive a fresh [`LoreStore`] handle.
///
/// The returned `handle_id` is guaranteed non-zero so it never collides
/// with [`LoreStore::INVALID`] — the counter skips the sentinel on wrap.
pub(crate) fn register(store: Arc<StoreInternal>) -> LoreStore {
    let handle_id = loop {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if id != LoreStore::INVALID.handle_id {
            break id;
        }
        // Counter wrapped to the sentinel (only reachable after 2^64 registrations); skip it.
    };
    REGISTRY.insert(handle_id, store);
    LoreStore { handle_id }
}

/// Look up the store behind a handle. Returns `None` for unknown or
/// already-unregistered handles.
pub(crate) fn lookup(handle: LoreStore) -> Option<Arc<StoreInternal>> {
    if handle.handle_id == LoreStore::INVALID.handle_id {
        return None;
    }
    REGISTRY.get(&handle.handle_id).map(|entry| entry.clone())
}

/// Test-only helper: return the underlying `Arc<dyn ImmutableStore>` for a registered handle.
/// Integration tests use this to introspect fragment counts and exercise the evictor/
/// compactor wiring. `#[doc(hidden)]` keeps it out of the public surface.
#[doc(hidden)]
pub fn immutable_for_test(handle: LoreStore) -> Option<Arc<dyn lore_storage::ImmutableStore>> {
    lookup(handle).map(|store| store.immutable.clone())
}

/// Test-only helper: return the underlying `Arc<dyn MutableStore>` for a registered handle.
/// Integration tests use this to assert mutable-store state directly. `#[doc(hidden)]` keeps it
/// out of the public surface.
#[doc(hidden)]
pub fn mutable_for_test(handle: LoreStore) -> Option<Arc<dyn lore_storage::MutableStore>> {
    lookup(handle).map(|store| store.mutable.clone())
}

/// Drain every entry in the registry, returning each `(handle_id, Arc<StoreInternal>)` pair
/// the registry held. After this call the registry is empty. Used by the library-level
/// shutdown path to walk + close every outstanding handle in one pass without racing against
/// a concurrent op that re-registers.
pub(crate) fn drain_all() -> Vec<(u64, Arc<StoreInternal>)> {
    let mut drained = Vec::new();
    REGISTRY.retain(|&id, store| {
        drained.push((id, store.clone()));
        false
    });
    drained
}

/// Drain every registry entry whose `StoreInternal::connection_id` matches `connection_id`.
/// Used by the IPC dispatcher on connection teardown to reclaim handles whose owning
/// connection dropped without an explicit close. Client-mode handles (with `connection_id =
/// None`) are unaffected.
pub(crate) fn drain_for_connection(connection_id: u64) -> Vec<(u64, Arc<StoreInternal>)> {
    let mut drained = Vec::new();
    REGISTRY.retain(|&id, store| {
        if store.connection_id == Some(connection_id) {
            drained.push((id, store.clone()));
            false
        } else {
            true
        }
    });
    drained
}

/// Remove the handle's entry from the registry, returning the `Arc` the
/// entry held (for the caller to drive close).
pub(crate) fn unregister(handle: LoreStore) -> Option<Arc<StoreInternal>> {
    if handle.handle_id == LoreStore::INVALID.handle_id {
        return None;
    }
    REGISTRY.remove(&handle.handle_id).map(|(_, store)| store)
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::storage::store::in_memory_for_tests;

    async fn make_store() -> Arc<StoreInternal> {
        in_memory_for_tests("handle-test").await
    }

    #[tokio::test]
    async fn register_then_lookup_round_trip() {
        let store_handle = register(make_store().await);
        assert_ne!(store_handle.handle_id, 0);
        let found = lookup(store_handle).expect("registered handle must look up");
        // Registry entry + the local clone = at least 2.
        assert!(Arc::strong_count(&found) >= 2);
        unregister(store_handle);
    }

    #[tokio::test]
    async fn stale_id_lookup_returns_none() {
        let store_handle = register(make_store().await);
        unregister(store_handle);
        assert!(lookup(store_handle).is_none());
    }

    #[test]
    fn invalid_handle_lookup_returns_none() {
        assert!(lookup(LoreStore::INVALID).is_none());
        assert!(unregister(LoreStore::INVALID).is_none());
    }

    #[tokio::test]
    async fn two_registrations_produce_distinct_ids() {
        let a = register(make_store().await);
        let b = register(make_store().await);
        assert_ne!(a.handle_id, b.handle_id);
        unregister(a);
        unregister(b);
    }

    #[tokio::test]
    async fn concurrent_registration_yields_unique_ids() {
        const THREADS: usize = 16;
        const PER_THREAD: usize = 128;
        // The store identity is irrelevant — share one Arc across threads to keep memory flat.
        let store = make_store().await;
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut joins = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let b = barrier.clone();
            let store = store.clone();
            joins.push(thread::spawn(move || {
                b.wait();
                let mut ids = Vec::with_capacity(PER_THREAD);
                for _ in 0..PER_THREAD {
                    ids.push(register(store.clone()));
                }
                ids
            }));
        }
        let mut all = Vec::with_capacity(THREADS * PER_THREAD);
        for j in joins {
            all.extend(j.join().unwrap());
        }
        let mut ids: Vec<u64> = all.iter().map(|h| h.handle_id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "ids must be unique across threads");
        for h in all {
            unregister(h);
        }
    }

    #[tokio::test]
    async fn unregister_returns_last_strong_ref() {
        let store = make_store().await;
        let store_handle = register(store.clone());
        // Drop the local Arc so only the registry holds a strong ref before unregister.
        drop(store);
        let returned = unregister(store_handle).expect("unregister returns the held Arc");
        assert_eq!(Arc::strong_count(&returned), 1);
    }
}
