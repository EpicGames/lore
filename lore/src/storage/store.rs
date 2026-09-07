// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shared runtime state for each open content-addressed storage handle.
//!
//! A [`StoreInternal`] is kept alive by the handle registry entry and by
//! every in-flight op holding an `Arc` clone; the store is torn down when
//! the last `Arc` drops. The `identity` string gives tests and logs a
//! human-readable reference for a handle.
//!
//! Concurrency protocol: every op increments `in_flight`, then checks
//! `invalid`; a true `invalid` flag triggers immediate decrement and
//! rejection. Close sets `invalid`, then awaits `in_flight -> 0` before
//! proceeding to teardown. The [`OpGuard`] RAII wrapper enforces this
//! pairing for callers.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::error::InvalidArguments;
use lore_base::types::Partition;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::options::ReadOptions;
use lore_transport::StorageSession;
use tokio::sync::Notify;

use crate::interface::LoreGlobalArgs;
use crate::storage::handle;
use crate::storage::handle::LoreStore;
use crate::storage::remote::RemoteEndpoint;

/// Bound mode set at `lore_storage_open` from `globals.{offline,local,remote}`.
///
/// `offline` and `local` both forbid the handle from reaching a remote; the storage API
/// treats them as a single "no remote" intent. `remote` flips reads to bypass the local
/// cache and forbids the upload-fallback tier in copy.
///
/// Open rejects `local && remote` — the two are contradictory in the bound state.
#[lore_macro::test_pub]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BoundFlags {
    pub offline: bool,
    pub local: bool,
    pub remote: bool,
}

impl BoundFlags {
    /// Read the bound flags from `globals` and validate the `local && remote`
    /// invariant. Returns `InvalidArguments` describing the violation when both bits are set.
    pub(crate) fn try_from_globals(globals: &LoreGlobalArgs) -> Result<Self, InvalidArguments> {
        let local = globals.local != 0;
        let remote = globals.remote != 0;
        if local && remote {
            return Err(InvalidArguments {
                reason: "open with `globals.local=1` and `globals.remote=1` is not allowed; \
                         the bound state cannot be both local-only and remote-only"
                    .into(),
            });
        }
        Ok(Self {
            offline: globals.offline != 0,
            local,
            remote,
        })
    }
}

/// Per-call snapshot of the three flag bits an op combines with the handle's `BoundFlags`.
/// Reading these three booleans up front avoids cloning the full `LoreGlobalArgs` (which
/// holds three heap-allocated `LoreString` fields) per op invocation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct PerCallFlags {
    pub offline: bool,
    pub local: bool,
    pub remote: bool,
}

impl PerCallFlags {
    pub(crate) fn from_globals(globals: &LoreGlobalArgs) -> Self {
        Self {
            offline: globals.offline != 0,
            local: globals.local != 0,
            remote: globals.remote != 0,
        }
    }
}

/// Combined bound + per-call flags for one op invocation. Computed via
/// [`StoreInternal::effective_flags`].
///
/// Invariants:
/// - `no_remote` = (bound or per-call) `offline` or `local` is set; remote calls are suppressed.
/// - `no_local` = (bound or per-call) `remote` is set; the local probe is bypassed.
/// - `no_remote && no_local` is impossible — the per-call combine rejects that case before
///   constructing this value.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveFlags {
    pub no_remote: bool,
    pub no_local: bool,
    /// Skip re-hashing a loaded payload against the address it was read from. Taken from the
    /// handle's `skip_verify` at open; no per-call flag moves it.
    pub skip_verify: bool,
}

impl EffectiveFlags {
    /// Build the base `ReadOptions` for a get-style op given whether a remote session is
    /// available. The per-item `local_cache=1` opt-in is layered by the caller on top of the
    /// returned options.
    pub(crate) fn read_options(&self, has_remote_session: bool) -> ReadOptions {
        let opts = ReadOptions::default();
        let opts = if self.skip_verify {
            opts.no_verify()
        } else {
            opts
        };
        if self.no_remote || !has_remote_session {
            return opts.no_remote();
        }
        if self.no_local {
            return opts.no_local();
        }
        opts
    }
}

/// Runtime state for one open storage handle.
#[lore_macro::test_pub]
pub(crate) struct StoreInternal {
    // Auth identity for remote-path ops; unused by the local-only path.
    #[allow(dead_code)]
    pub identity: String,
    pub immutable: Arc<dyn ImmutableStore>,
    pub mutable: Arc<dyn MutableStore>,
    /// Remote endpoint bound at open time when `has_remote_config != 0`. `None` for local-only
    /// handles. Ops that need a remote consult this; absence flips them to local-only behavior.
    pub remote: Option<Arc<RemoteEndpoint>>,
    /// Bound `globals.{offline,local,remote}` from the open call. Each op combines these with
    /// its per-call globals via [`Self::effective_flags`] to decide whether to consult the
    /// remote or bypass the local cache.
    pub bound_flags: BoundFlags,
    /// Whether reads on this handle skip re-hashing what they loaded. From `skip_verify` at open.
    pub skip_verify: bool,
    /// Optional service-mode connection that owns this handle. Populated by the IPC dispatcher
    /// during a server-mode open so that connection teardown can drive
    /// [`crate::storage::handle::close_all_for_connection`] and reclaim handles whose owning
    /// connection has dropped without an explicit close. `None` for client-mode opens — those
    /// remain on explicit-close.
    pub connection_id: Option<u64>,
    pub in_flight: AtomicU64,
    pub invalid: AtomicBool,
    pub drained: Notify,
    /// This process's claim on the stores, held for as long as the handle is open.
    ///
    /// An open store is a store this process is using, and the flock is what says so
    /// to other processes. Taking the claim here rather than per operation is what
    /// makes it mean that: the alternative counts operations, which says nothing
    /// about whether anyone has the store, and pays a lock acquisition and an epoch
    /// read on every call.
    ///
    /// Dropped with the last reference to the handle's state: at close, or when the
    /// last revision tree loaded against the handle closes, if that is later. Close
    /// joins it first for the flush it starts, so the stores stay claimed until that
    /// flush is done. The keep-alive cache may still hold the store objects afterwards —
    /// that is retained memory, not an open store, and the epoch is what decides whether
    /// it is still usable when one is opened again.
    _hold: lore_storage::local::store_lock::StoreHold,
}

impl StoreInternal {
    #[lore_macro::test_pub]
    pub(crate) fn new(
        identity: impl Into<String>,
        immutable: Arc<dyn ImmutableStore>,
        mutable: Arc<dyn MutableStore>,
        remote: Option<Arc<RemoteEndpoint>>,
        bound_flags: BoundFlags,
        skip_verify: bool,
        hold: lore_storage::local::store_lock::StoreHold,
    ) -> Self {
        Self {
            identity: identity.into(),
            immutable,
            mutable,
            remote,
            bound_flags,
            skip_verify,
            connection_id: None,
            in_flight: AtomicU64::new(0),
            invalid: AtomicBool::new(false),
            drained: Notify::new(),
            _hold: hold,
        }
    }

    /// Builder variant: tag the handle with the IPC connection it was opened on. The IPC
    /// dispatcher invokes this in server mode so connection teardown can find and close every
    /// handle the connection owned. Client-mode opens never set this; their handles survive
    /// until an explicit close or `lore::shutdown`.
    ///
    /// Currently only tests exercise this — the IPC dispatcher hookup is the next piece of
    /// infrastructure to land. The `test-util` gate keeps the helper out of the public surface
    /// until then; remove the gate once a production caller exists.
    #[cfg(feature = "test-util")]
    pub fn with_connection_id(mut self, id: u64) -> Self {
        self.connection_id = Some(id);
        self
    }

    /// Combine the handle's bound flags with `per_call` to produce the effective flags for
    /// one op invocation. Per-call flags can tighten but not loosen the bound state; per-call
    /// `local && remote` is rejected; a bound `remote` paired with a per-call `local` (or
    /// vice-versa) is also rejected — the request is contradictory after combine.
    pub(crate) fn effective_flags(
        &self,
        per_call: PerCallFlags,
    ) -> Result<EffectiveFlags, InvalidArguments> {
        if per_call.local && per_call.remote {
            return Err(InvalidArguments {
                reason: "per-call `globals.local=1` with `globals.remote=1` is not allowed; \
                         the call cannot be both local-only and remote-only"
                    .into(),
            });
        }
        let any_local = self.bound_flags.offline
            || self.bound_flags.local
            || per_call.offline
            || per_call.local;
        let any_remote = self.bound_flags.remote || per_call.remote;
        if any_local && any_remote {
            return Err(InvalidArguments {
                reason: "combined bound + per-call flags require both local-only and \
                         remote-only behavior; tighten the per-call flags so they do not \
                         contradict the handle's bound state"
                    .into(),
            });
        }
        Ok(EffectiveFlags {
            no_remote: any_local,
            no_local: any_remote,
            skip_verify: self.skip_verify,
        })
    }

    /// Ensure the server's `authorized_repos` for the underlying connection contains
    /// `partition`, so subsequent admin / copy / cross-session calls referencing it
    /// pass the connection-scoped authz check. Fast-paths via the connector's
    /// `authorized_partitions` cache — partitions that have already been registered on
    /// this connection skip the wire round-trip entirely. No-op on local-only handles.
    pub(crate) async fn ensure_partition_authorized(&self, partition: Partition) {
        if let Some(remote) = self.remote.clone() {
            let _ = remote.ensure_authorized(partition).await;
        }
    }

    /// Build a lazily-resolved [`StorageSession`] bound to this handle's remote endpoint and
    /// the supplied partition. Returns `None` when the handle has no `remote_config`. The
    /// returned session does not open a connection until first use — passing it to
    /// `write_content` makes the upload happen, while ops that ultimately bypass the remote
    /// (e.g. address dedup hits) leave the connection idle.
    pub(crate) fn remote_session_for(&self, partition: Partition) -> Option<Arc<StorageSession>> {
        let remote = self.remote.clone()?;
        let session = StorageSession::pending(move || {
            let remote = remote.clone();
            async move { remote.session(partition).await }
        });
        Some(Arc::new(session))
    }

    /// Close sequence: mark the store invalid so no new ops enter, then
    /// block until every in-flight op has paired its decrement. Ops that
    /// race in between increment-and-check self-abort because they see
    /// `invalid=true` before proceeding.
    #[lore_macro::test_pub]
    pub(crate) async fn mark_invalid_and_await(&self) {
        self.invalid.store(true, Ordering::Release);
        loop {
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            let mut notified = std::pin::pin!(self.drained.notified());
            // Register before the re-check — `notified()` alone is unregistered until
            // first poll, which would miss a decrement that fires between the check and the
            // await.
            notified.as_mut().enable();
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Reuses one remote [`StorageSession`] across a batched call's items. Batched storage ops
/// almost always target a single partition (a batch of keys/addresses in one repository), so a
/// single `SessionReuse` threaded through the item loop resolves the session once for the whole
/// call and reuses it for every following item with the same partition, resolving a fresh one
/// only when the partition changes. It is scoped to one call so the lazily-resolved session binds
/// to that call's correlation id, and it holds no heap state beyond the one remembered session.
#[derive(Default)]
pub(crate) struct SessionReuse {
    last: Option<(Partition, Option<Arc<StorageSession>>)>,
}

impl SessionReuse {
    /// The remote session for `partition`, reusing the previously resolved one when `partition`
    /// matches the last call. Returns `None` without resolving when `want_remote` is false (the
    /// local path) or the handle has no remote endpoint.
    pub(crate) fn session_for(
        &mut self,
        store: &StoreInternal,
        partition: Partition,
        want_remote: bool,
    ) -> Option<Arc<StorageSession>> {
        if !want_remote {
            return None;
        }
        if let Some((last_partition, session)) = &self.last
            && *last_partition == partition
        {
            return session.clone();
        }
        let session = store.remote_session_for(partition);
        self.last = Some((partition, session.clone()));
        session
    }
}

/// RAII guard protecting an in-flight op. Obtained via [`OpGuard::enter`];
/// dropping it pairs the in-flight increment with the matching decrement
/// and, when the count reaches zero, wakes any [`mark_invalid_and_await`]
/// waiter.
#[lore_macro::test_pub]
pub(crate) struct OpGuard {
    store: Arc<StoreInternal>,
}

impl OpGuard {
    /// Enter an op on the store behind `store_handle`. Returns `None` when
    /// the handle is unknown or the store has been marked invalid.
    #[lore_macro::test_pub]
    pub(crate) fn enter(store_handle: LoreStore) -> Option<Self> {
        let store = handle::lookup(store_handle)?;
        store.in_flight.fetch_add(1, Ordering::AcqRel);
        if store.invalid.load(Ordering::Acquire) {
            Self::release(&store);
            return None;
        }
        Some(Self { store })
    }

    #[allow(dead_code)] // Ops that spawn parallel work use `store_clone`; this is the deref path for ops that don't need an owned Arc.
    pub(crate) fn store(&self) -> &StoreInternal {
        &self.store
    }

    /// Clone the underlying `Arc<StoreInternal>` for handing to a spawned task. The caller is
    /// responsible for making sure the spawned work completes before this guard drops; cloning
    /// the Arc only extends the store's teardown past the guard, not the op's in-flight
    /// counter.
    pub(crate) fn store_clone(&self) -> Arc<StoreInternal> {
        self.store.clone()
    }

    fn release(store: &StoreInternal) {
        // `fetch_sub` returns the previous value; previous == 1 means we just brought it to
        // zero — wake the closer.
        if store.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            store.drained.notify_waiters();
        }
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        Self::release(&self.store);
    }
}

// Path-keyed sharing of backend Arcs between opens of the same path is provided by
// `lore_revision::repository`'s IMMUTABLE_STORE_CACHE / MUTABLE_STORE_CACHE (and in-memory
// counterparts). Each `StoreInternal` is per-handle and wraps those shared Arcs; open-time
// construction goes through `create_client_immutable_store` / `create_client_mutable_store` so
// two handles targeting the same path receive the same underlying backend Arcs while retaining
// their own counters, invalid flags, and bound global flags.

/// Construct a `StoreInternal` backed by in-memory `LocalImmutableStore` / `LocalMutableStore`.
/// Used by tests that need a real `StoreInternal` to exercise the in-flight counter protocol,
/// the dispatch helper, etc., without having to drive a full `open` op.
#[cfg(feature = "test-util")]
pub async fn in_memory_for_tests(identity: impl Into<String>) -> Arc<StoreInternal> {
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::immutable_store::create as create_immutable;
    use lore_storage::local::mutable_store::LocalMutableStore;
    use lore_storage::local::mutable_store::MutableStoreSettings;

    let immutable = create_immutable(
        Option::<std::path::PathBuf>::None,
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store init");
    let mutable: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Option::<&std::path::Path>::None,
            MutableStoreSettings::default(),
            immutable.clone(),
        )
        .await
        .expect("in-memory mutable store init"),
    );
    // In-memory stores have no directory and so no flock to claim; an empty hold is
    // what `hold_for_command` returns for them anyway.
    Arc::new(StoreInternal::new(
        identity,
        immutable,
        mutable,
        None,
        BoundFlags::default(),
        false,
        lore_storage::local::store_lock::StoreHold::default(),
    ))
}

/// Construct a `StoreInternal` backed by disk stores under `root`, holding the claims an open
/// handle holds. For tests of what those claims do, which in-memory stores have none of.
#[cfg(feature = "test-util")]
pub async fn disk_backed_for_tests(
    identity: impl Into<String>,
    root: &std::path::Path,
) -> Arc<StoreInternal> {
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::immutable_store::create as create_immutable;
    use lore_storage::local::mutable_store::LocalMutableStore;
    use lore_storage::local::mutable_store::MutableStoreSettings;

    let immutable = create_immutable(
        Some(root),
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("disk-backed immutable store init");
    let mutable: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(root),
            MutableStoreSettings::default(),
            immutable.clone(),
        )
        .await
        .expect("disk-backed mutable store init"),
    );
    let mut guards = Vec::new();
    guards.extend(
        immutable
            .clone()
            .hold_for_command()
            .await
            .expect("claims the immutable store"),
    );
    guards.extend(
        mutable
            .clone()
            .hold_for_command()
            .await
            .expect("claims the mutable store"),
    );
    Arc::new(StoreInternal::new(
        identity,
        immutable,
        mutable,
        None,
        BoundFlags::default(),
        false,
        lore_storage::local::store_lock::StoreHold::new(guards),
    ))
}
