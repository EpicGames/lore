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
use lore_revision::repository::InMemoryContext;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryContextCreationArgs;
use lore_revision::repository::RepositoryFormat;
use lore_revision::repository::RepositoryWriteToken;
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
    /// Registry key of the parent storage handle this revision tree was loaded
    /// against, and what the connection-teardown cascade matches on. The id
    /// rather than the store, so two handles sharing one cached backend stay
    /// distinct.
    pub(crate) parent_storage_handle_id: u64,
    /// Repository identity (a `Partition`) the loaded revision targets.
    pub(crate) repository: Partition,
    /// Synthesized repository context covering the underlying immutable,
    /// mutable, and (optional) remote stores. Built in the bridge helper
    /// at load time and reused across every verb on this handle.
    pub(crate) repository_context: Arc<RepositoryContext>,
    /// Loaded revision's in-memory `State`.
    ///
    /// Private, and reachable only through [`Self::access_shared`] or
    /// [`Self::access_exclusive`], so a call cannot touch the tree without saying
    /// whether it can share the handle. `State` is internally
    /// mutable, so an editing verb mutates it while holding a *shared* guard; `commit`
    /// takes the exclusive one. That is what makes a commit atomic against the handle —
    /// no edit lands inside the freeze, and nothing is reading the old state when a
    /// failed commit puts the restored one back through the same guard.
    ///
    /// Async-aware because commit holds it across the freeze's awaits, where a
    /// `parking_lot` guard is neither `Send` nor safe to hold.
    state: tokio::sync::RwLock<Arc<State>>,
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

/// A held claim on the handle's tree, shared with other calls.
///
/// Every verb but `commit` works under one of these. `State` is internally mutable, so
/// an editing verb mutates the tree through a shared claim; what the claim excludes is
/// a commit, not another edit.
///
/// Keep it alive for as long as the state is in use: dropping it and holding on to the
/// `Arc<State>` leaves a verb mutating a tree a commit believes it has to itself.
pub(crate) struct SharedAccess<'handle>(tokio::sync::RwLockReadGuard<'handle, Arc<State>>);

impl SharedAccess<'_> {
    /// The tree this call may work on.
    pub(crate) fn state(&self) -> Arc<State> {
        (*self.0).clone()
    }
}

/// A held claim on the handle's tree, to the exclusion of every other call.
///
/// Only `commit` takes one, because it rewrites the tree in place and replaces it
/// outright if that fails. A caller whose event callback re-enters the API on the same
/// handle deadlocks against this rather than corrupting a commit — which the callback
/// contract already forbids.
///
/// Keep it alive for as long as the state is in use: dropping it and holding on to the
/// `Arc<State>` leaves a commit rewriting a tree other calls can reach.
pub(crate) struct ExclusiveAccess<'handle>(tokio::sync::RwLockWriteGuard<'handle, Arc<State>>);

impl ExclusiveAccess<'_> {
    /// The tree this call may work on.
    pub(crate) fn state(&self) -> Arc<State> {
        (*self.0).clone()
    }

    /// Replace the tree the handle serves — a commit restoring its pre-commit snapshot.
    ///
    /// Offered on the exclusive claim alone because that is what makes it sound: no
    /// concurrent call is reading the tree being swapped out.
    pub(crate) fn replace(&mut self, state: Arc<State>) {
        *self.0 = state;
    }
}

impl RevisionTreeInternal {
    /// Build a handle's internals around a freshly loaded tree. A constructor rather
    /// than a struct literal because [`Self::state`] is private, which is what keeps
    /// every reader on the two access methods.
    pub(crate) fn new(
        store_internal: Arc<StoreInternal>,
        parent_storage_handle_id: u64,
        repository: Partition,
        repository_context: Arc<RepositoryContext>,
        state: Arc<State>,
    ) -> Self {
        Self {
            store_internal,
            parent_storage_handle_id,
            repository,
            repository_context,
            state: tokio::sync::RwLock::new(state),
            pending_metadata: parking_lot::RwLock::new(Metadata::default()),
            in_flight: AtomicU64::new(0),
            invalid: AtomicBool::new(false),
            drained: Notify::new(),
        }
    }

    /// Claim the handle's tree for a call that can share it — see [`SharedAccess`].
    pub(crate) async fn access_shared(&self) -> SharedAccess<'_> {
        SharedAccess(self.state.read().await)
    }

    /// Claim the handle's tree for a call that cannot — see [`ExclusiveAccess`].
    pub(crate) async fn access_exclusive(&self) -> ExclusiveAccess<'_> {
        ExclusiveAccess(self.state.write().await)
    }

    /// The handle's tree, for a fixture that builds or inspects it directly.
    ///
    /// Deliberately non-blocking and non-async: it stays callable from a synchronous
    /// helper inside an async test, and a fixture that has in fact raced a verb fails
    /// here instead of quietly waiting for it.
    #[cfg(test)]
    pub(crate) fn state_for_tests(&self) -> Arc<State> {
        self.state
            .try_read()
            .expect("a fixture must not race a verb for the handle's tree")
            .clone()
    }

    /// Close sequence: mark the handle invalid so no new ops enter, then
    /// block until every in-flight op has paired its decrement. Ops that
    /// race in between increment-and-check self-abort because they see
    /// `invalid=true` before proceeding.
    ///
    /// The waiter is registered before the re-check, because `notified()` alone
    /// stays unregistered until first poll and would miss a decrement firing
    /// between the check and the await.
    pub(crate) async fn mark_invalid_and_await(&self) {
        self.invalid.store(true, Ordering::Release);
        loop {
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            let mut notified = std::pin::pin!(self.drained.notified());
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

/// Test-only helper: whether `handle` is still registered. `#[doc(hidden)]` keeps it
/// out of the public surface, mirroring `storage::handle::immutable_for_test`.
///
/// `lore/tests/shutdown.rs` reads the registry directly because no verb can report on
/// it once `lore::shutdown` has taken the runtimes down.
#[doc(hidden)]
pub fn is_registered_for_test(handle: LoreRevisionTree) -> bool {
    lookup(handle).is_some()
}

/// Drain the registry, returning every `(handle_id, Arc<RevisionTreeInternal>)` pair it
/// held. Atomic against a concurrent load, which is why shutdown takes the whole set in
/// one pass rather than iterating.
pub(crate) fn drain_all() -> Vec<(u64, Arc<RevisionTreeInternal>)> {
    let mut drained = Vec::new();
    REGISTRY.retain(|&id, internal| {
        drained.push((id, internal.clone()));
        false
    });
    drained
}

/// Drain every registry entry loaded against the storage handle `storage_handle_id`, for
/// the connection-teardown cascade: a tree outlives its parent by design, so a connection
/// that drops without closing it would hold the store for the life of the process.
pub(crate) fn drain_for_storage_handle(
    storage_handle_id: u64,
) -> Vec<(u64, Arc<RevisionTreeInternal>)> {
    let mut drained = Vec::new();
    REGISTRY.retain(|&id, internal| {
        if internal.parent_storage_handle_id == storage_handle_id {
            drained.push((id, internal.clone()));
            false
        } else {
            true
        }
    });
    drained
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
    Arc::new(
        RepositoryContext::new(RepositoryContextCreationArgs {
            path: None,
            immutable_store: store.immutable.clone(),
            mutable_store: store.mutable.clone(),
            id: repository,
            instance_id: InstanceId::default(),
            remote: remote_result,
            filter: Arc::new(Filter::default()),
            format: RepositoryFormat::Lore,
            filesystem_provider: None,
        })
        .with_write_token(RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER)),
    )
}

/// Gates [`RepositoryWriteToken::in_memory`] for this crate.
///
/// The handle exists to build revisions, so its context is write-capable from
/// load. Being path-less it has no per-path write mutex to take; what serializes
/// publication is the branch tip compare-and-swap in the commit.
pub(crate) struct InMemoryMarker;
impl InMemoryContext for InMemoryMarker {}
pub(crate) const IN_MEMORY_MARKER: InMemoryMarker = InMemoryMarker;

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

    use lore_base::types::Partition;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryContextCreationArgs;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::repository::create_client_memory_stores;
    use lore_revision::state::State;
    use lore_transport::ProtocolError;

    use super::RevisionTreeInternal;
    use crate::storage::store::StoreInternal;
    use crate::storage::store::in_memory_for_tests;

    /// Build a `RevisionTreeInternal` for tests. Uses in-memory stores so
    /// no filesystem touch happens and no cleanup is required.
    pub(crate) async fn new_for_testing() -> Arc<RevisionTreeInternal> {
        new_for_testing_on_storage_handle(0).await
    }

    /// The same fixture, claiming to have been loaded against a given storage
    /// handle — what the connection-teardown close cascade matches on.
    pub(crate) async fn new_for_testing_on_storage_handle(
        parent_storage_handle_id: u64,
    ) -> Arc<RevisionTreeInternal> {
        let store_internal: Arc<StoreInternal> = in_memory_for_tests("revision-tree-test").await;
        let (immutable, mutable) = create_client_memory_stores()
            .await
            .expect("create_client_memory_stores");
        let repository = Partition::default();
        let repository_context = Arc::new(RepositoryContext::new(RepositoryContextCreationArgs {
            path: None,
            immutable_store: immutable,
            mutable_store: mutable,
            id: repository,
            instance_id: Default::default(),
            remote: Err(ProtocolError::from(lore_base::error::NoRemote)),
            filter: Arc::default(),
            format: RepositoryFormat::Lore,
            filesystem_provider: None,
        }));
        let state = Arc::new(State::new());
        Arc::new(RevisionTreeInternal::new(
            store_internal,
            parent_storage_handle_id,
            repository,
            repository_context,
            state,
        ))
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
