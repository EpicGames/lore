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
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, bitcode::Encode, bitcode::Decode)]
pub struct LoreStore {
    /// Registry key; `0` is the reserved invalid/unregistered sentinel (zero-init = null handle)
    pub handle_id: u64,
}

impl LoreStore {
    pub const INVALID: Self = Self { handle_id: 0 };
}

lore_base::carries_no_text!(LoreStore);

static REGISTRY: LazyLock<DashMap<u64, Arc<StoreInternal>>> = LazyLock::new(DashMap::new);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Register a store and receive a fresh [`LoreStore`] handle.
///
/// The returned `handle_id` is guaranteed non-zero so it never collides
/// with [`LoreStore::INVALID`] — the counter skips the sentinel on wrap.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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

/// Test-only helper: return the storage session a partition's ops would use, or `None` when the
/// handle has no remote. Integration tests use it to drive `lore_storage` directly against the
/// same server an op would reach, for results the event surface does not carry.
/// `#[doc(hidden)]` keeps it out of the public surface.
#[doc(hidden)]
pub fn session_for_test(
    handle: LoreStore,
    partition: lore_base::types::Partition,
) -> Option<Arc<lore_transport::StorageSession>> {
    lookup(handle).and_then(|store| store.remote_session_for(partition))
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
pub(crate) fn unregister(handle: LoreStore) -> Option<Arc<StoreInternal>> {
    if handle.handle_id == LoreStore::INVALID.handle_id {
        return None;
    }
    REGISTRY.remove(&handle.handle_id).map(|(_, store)| store)
}
