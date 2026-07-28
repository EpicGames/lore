// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Opaque handle and process-global registry for the low-level
//! memory-based revision control API.
//!
//! Handles are opaque POD values handed to FFI callers. Each is a `u64`
//! drawn from a monotonic counter and indexed into a process-global
//! [`DashMap`] keyed by that id. The map's value is an
//! `Arc<RevisionTreeInternal>` — the underlying state is shared between
//! the registry entry and any in-flight ops that have already looked up
//! the handle and are holding an `Arc` clone.
//!
//! The internal state carries an `Arc<StoreInternal>` clone of the parent
//! storage handle so the revision tree outlives a `lore_storage_close` on
//! the parent. The registry, lookup, and unregister helpers mirror
//! `lore::storage::handle` byte-for-byte; the [`RevisionTreeGuard`] RAII
//! wrapper enforces the in-flight counter protocol the same way
//! [`crate::storage::store::OpGuard`] does.

// Several items here are unread until their owning verbs land. A file-level
// `expect` keeps the lint quiet now and fires once every item is wired up —
// at which point this attribute should be removed.
#![expect(dead_code)]

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use lore_base::error::NoRemote;
use lore_base::types::Partition;
use lore_revision::filter::Filter;
use lore_revision::instance::InstanceId;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryFormat;
use lore_revision::state::State;
use lore_transport::ProtocolError;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Notify;

use crate::storage::store::StoreInternal;

/// Opaque handle to an open memory-based revision tree instance.
///
/// Treat this as an opaque value; never cast it directly to or from raw
/// pointers.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct LoreRevisionTree {
    /// Registry key; `0` is the reserved invalid/unregistered sentinel (zero-init = null handle)
    pub handle_id: u64,
}

impl LoreRevisionTree {
    pub const INVALID: Self = Self { handle_id: 0 };
}

lore_base::carries_no_text!(LoreRevisionTree);

/// Runtime state for one open revision tree handle.
///
/// Holds an `Arc<StoreInternal>` clone of the parent storage handle so
/// `lore_storage_close` on the parent does not tear down the underlying
/// store while a revision tree handle still references it. The
/// `parent_storage_handle_id` is the registry key of the parent storage
/// handle at load time and is the matching key used by the IPC dispatcher
/// to cascade closes on connection teardown.
pub(crate) struct RevisionTreeInternal {
    /// Shared store reference cloned from the parent storage handle.
    pub(crate) store_internal: Arc<StoreInternal>,
    /// Registry key of the parent storage handle this revision tree was
    /// loaded against. Used by the storage-close warning path and the IPC
    /// connection-teardown cascade to match revision tree handles to
    /// their parent storage handle.
    pub(crate) parent_storage_handle_id: u64,
    /// Repository identity (a `Partition`) the loaded revision targets.
    pub(crate) repository: Partition,
    /// Synthesized repository context covering the underlying immutable,
    /// mutable, and (optional) remote stores. Built in the bridge helper
    /// at load time and reused across every verb on this handle.
    pub(crate) repository_context: Arc<RepositoryContext>,
    /// Loaded revision's in-memory `State`. Internally mutable via the
    /// `parking_lot` locks `State` holds; no outer lock is required.
    pub(crate) state: Arc<State>,
    /// Accumulator for `metadata_set` edits. Commit clones the buffer,
    /// serializes the clone, and on success replaces this with a fresh
    /// default.
    pub(crate) pending_metadata: parking_lot::RwLock<Metadata>,
    /// In-flight op counter; paired increment/decrement via
    /// [`RevisionTreeGuard`].
    pub(crate) in_flight: AtomicU64,
    /// Set by close (or any commit failure) to reject subsequent ops.
    pub(crate) invalid: AtomicBool,
    /// Wakes [`Self::mark_invalid_and_await`] when `in_flight` reaches
    /// zero.
    pub(crate) drained: Notify,
}

impl RevisionTreeInternal {
    /// Close sequence: mark the handle invalid so no new ops enter, then
    /// block until every in-flight op has paired its decrement. Ops that
    /// race in between increment-and-check self-abort because they see
    /// `invalid=true` before proceeding.
    pub(crate) async fn mark_invalid_and_await(&self) {
        self.invalid.store(true, Ordering::Release);
        loop {
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            let mut notified = std::pin::pin!(self.drained.notified());
            // Register before the re-check — `notified()` alone is unregistered until
            // first poll, which would miss a decrement that fires between the check and
            // the await.
            notified.as_mut().enable();
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) static REGISTRY: LazyLock<DashMap<u64, Arc<RevisionTreeInternal>>> =
    LazyLock::new(DashMap::new);
pub(crate) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Register a revision tree and receive a fresh [`LoreRevisionTree`]
/// handle.
///
/// The returned `handle_id` is guaranteed non-zero so it never collides
/// with [`LoreRevisionTree::INVALID`] — the counter skips the sentinel on
/// wrap.
pub(crate) fn register(internal: Arc<RevisionTreeInternal>) -> LoreRevisionTree {
    let handle_id = loop {
        let id = NEXT_ID.fetch_add(1, Ordering::AcqRel);
        if id != LoreRevisionTree::INVALID.handle_id {
            break id;
        }
        // Counter wrapped to the sentinel (only reachable after 2^64 registrations); skip it.
    };
    REGISTRY.insert(handle_id, internal);
    LoreRevisionTree { handle_id }
}

/// Look up the revision tree behind a handle. Returns `None` for unknown
/// or already-unregistered handles.
pub(crate) fn lookup(handle: LoreRevisionTree) -> Option<Arc<RevisionTreeInternal>> {
    if handle.handle_id == LoreRevisionTree::INVALID.handle_id {
        return None;
    }
    REGISTRY.get(&handle.handle_id).map(|entry| entry.clone())
}

/// Remove the handle's entry from the registry, returning the `Arc` the
/// entry held (for the caller to drive close).
pub(crate) fn unregister(handle: LoreRevisionTree) -> Option<Arc<RevisionTreeInternal>> {
    if handle.handle_id == LoreRevisionTree::INVALID.handle_id {
        return None;
    }
    REGISTRY
        .remove(&handle.handle_id)
        .map(|(_, internal)| internal)
}

/// Build an [`Arc<RepositoryContext>`] backed by the immutable, mutable,
/// and (optional) remote stores carried on `store`, targeting `repository`.
///
/// The synthesized context has no working-tree path — every op against the
/// memory-based revision tree API operates only on the underlying stores.
/// When `store` has a remote endpoint, the helper resolves the
/// per-partition `Arc<Connection>` so the context lands in the `Connected`
/// remote state on success; a connection failure propagates as `Failed`.
/// Absent a remote endpoint the context resolves directly to the `Offline`
/// terminal state.
pub(crate) async fn synth_repository_context(
    store: &StoreInternal,
    repository: Partition,
) -> Arc<RepositoryContext> {
    let remote_result = match store.remote.as_ref() {
        Some(endpoint) => endpoint.session_connection(repository).await,
        None => Err(ProtocolError::from(NoRemote)),
    };
    Arc::new(RepositoryContext::new(
        None,
        store.immutable.clone(),
        store.mutable.clone(),
        repository,
        InstanceId::default(),
        remote_result,
        Arc::new(Filter::default()),
        RepositoryFormat::Lore,
    ))
}

/// RAII guard protecting an in-flight op. Obtained via
/// [`RevisionTreeGuard::enter`]; dropping it pairs the in-flight
/// increment with the matching decrement and, when the count reaches
/// zero, wakes any [`RevisionTreeInternal::mark_invalid_and_await`]
/// waiter.
pub(crate) struct RevisionTreeGuard {
    internal: Arc<RevisionTreeInternal>,
}

impl RevisionTreeGuard {
    /// Enter an op on the revision tree behind `handle`. Returns `None`
    /// when the handle is unknown or the tree has been marked invalid.
    pub(crate) fn enter(handle: LoreRevisionTree) -> Option<Self> {
        let internal = lookup(handle)?;
        internal.in_flight.fetch_add(1, Ordering::AcqRel);
        if internal.invalid.load(Ordering::Acquire) {
            Self::release(&internal);
            return None;
        }
        Some(Self { internal })
    }

    /// Clone the underlying `Arc<RevisionTreeInternal>` for handing to a
    /// spawned task. The caller is responsible for making sure the
    /// spawned work completes before this guard drops; cloning the Arc
    /// only extends the tree's teardown past the guard, not the op's
    /// in-flight counter.
    pub(crate) fn internal_clone(&self) -> Arc<RevisionTreeInternal> {
        self.internal.clone()
    }

    fn release(internal: &RevisionTreeInternal) {
        // `fetch_sub` returns the previous value; previous == 1 means we just brought it to
        // zero — wake the closer.
        if internal.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            internal.drained.notify_waiters();
        }
    }
}

impl Drop for RevisionTreeGuard {
    fn drop(&mut self) {
        Self::release(&self.internal);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test-only fixture builder for [`RevisionTreeInternal`]. The
    //! production constructor lives with the load verb; this fixture
    //! lets the registry / guard unit tests run against a minimally-
    //! populated value without depending on `load`.
    //!
    //! The fixture builds a real `Arc<StoreInternal>` via the storage
    //! crate's `in_memory_for_tests` helper and a real `Arc<State>` /
    //! `Arc<RepositoryContext>` via the `lore-revision` in-memory test
    //! plumbing, so the registry tests run against the same type shape
    //! the production load verb produces.
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;

    use lore_base::error::NoRemote;
    use lore_base::types::Partition;
    use lore_revision::filter::Filter;
    use lore_revision::instance::InstanceId;
    use lore_revision::metadata::Metadata;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::repository::create_client_memory_stores;
    use lore_revision::state::State;
    use lore_transport::ProtocolError;
    use tokio::sync::Notify;

    use super::RevisionTreeInternal;
    use crate::storage::store::StoreInternal;
    use crate::storage::store::in_memory_for_tests;

    /// Build a `RevisionTreeInternal` for tests. Uses in-memory stores so
    /// no filesystem touch happens and no cleanup is required.
    pub(crate) async fn new_for_testing() -> Arc<RevisionTreeInternal> {
        let store_internal: Arc<StoreInternal> = in_memory_for_tests("revision-tree-test").await;
        let (immutable, mutable) = create_client_memory_stores()
            .await
            .expect("create_client_memory_stores");
        let repository = Partition::default();
        let repository_context = Arc::new(RepositoryContext::new(
            None,
            immutable,
            mutable,
            repository,
            InstanceId::default(),
            Err(ProtocolError::from(NoRemote)),
            Arc::new(Filter::default()),
            RepositoryFormat::Lore,
        ));
        let state = Arc::new(State::new());
        Arc::new(RevisionTreeInternal {
            store_internal,
            parent_storage_handle_id: 0,
            repository,
            repository_context,
            state,
            pending_metadata: parking_lot::RwLock::new(Metadata::default()),
            in_flight: AtomicU64::new(0),
            invalid: AtomicBool::new(false),
            drained: Notify::new(),
        })
    }
}

#[cfg(test)]
mod synth_repository_context_tests {
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::repository::RemoteStatus;
    use lore_revision::state::State;

    use super::synth_repository_context;
    use crate::storage::store::in_memory_for_tests;

    #[tokio::test]
    async fn synth_repository_context_round_trips_empty_state_via_zero_hash_deserialize() {
        let store = in_memory_for_tests("synth-context-test").await;
        let partition = Partition::from([0x77u8; 16]);

        let repo_context = synth_repository_context(&store, partition).await;

        State::deserialize(repo_context.clone(), Hash::default())
            .await
            .expect("zero hash must deserialize to an empty state");

        assert!(
            repo_context.path.is_none(),
            "synthesized context must have no working-tree path"
        );
        assert_eq!(
            repo_context.id, partition,
            "synthesized context must carry the supplied partition"
        );
        assert!(
            matches!(repo_context.remote_status().await, RemoteStatus::Offline),
            "in-memory store has no remote, so the context must be Offline"
        );
    }
}
