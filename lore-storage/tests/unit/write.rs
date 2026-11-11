// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::content::ContentSource;
use lore_storage::error::StorageError;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::immutable_store::StoreError;
use lore_storage::immutable_store::query_one;
use lore_storage::options::WriteOptions;
use lore_storage::store_types::StoreGetData;
use lore_storage::store_types::StoreMatch;
use lore_storage::store_types::StoreMatchResult;
use lore_storage::write_tracker::WriteContext;
use lore_storage::write_tracker::WriteTracker;
use lore_transport::StorageSession;
use tokio_util::sync::CancellationToken;

mod established_decisions;

use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_storage::FlushOptions;
use lore_storage::Partition;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::write::*;

/// A reader looking for a store while its guard drops finds the entry or sees the count
/// moved, never neither: neither would let it skip the probe that finds the store.
#[tokio::test]
async fn a_dropping_guard_is_never_both_gone_and_uncounted() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0xC7; 16]);
    let address = Address {
        hash: lore_storage::hash::hash_slice(b"a dropping guard"),
        context: Context::from([0xC7; 16]),
    };
    let before = stores_landed(address);
    let guard = try_acquire_in_flight(&store, partition, address).expect("fresh address");
    let seen = std::rc::Rc::new(std::cell::Cell::new(None));
    {
        let seen = seen.clone();
        lore_storage::test_util::set_guard_midpoint(move |dropped, still_in_flight| {
            if dropped == address {
                seen.set(Some(still_in_flight && stores_landed(address) != before));
            }
        });
    }
    drop(guard);
    lore_storage::test_util::clear_guard_midpoint();
    assert_eq!(
        seen.get(),
        Some(true),
        "midway through the drop the store was gone and its count unmoved"
    );
}

#[test]
fn remote_put_retry_accepts_arc_storage_session_and_is_send() {
    // Compile-only: asserts remote_put_retry's signature takes
    // Arc<StorageSession> and returns a Send + 'static future, which is
    // required to call it from inside tokio::spawn in a leader task.
    fn ensure_spawn_ok<F, Fut>(_f: F)
    where
        F: FnOnce(Arc<StorageSession>, Address, Fragment, Option<Bytes>) -> Fut,
        Fut: std::future::Future<Output = Result<(), StorageError>> + Send + 'static,
    {
    }
    ensure_spawn_ok(remote_put_retry);
}

/// `keep_local` keeps the payload in the writing store without recording anything in the fragment,
/// so the stores that fetch the fragment later do not cache it on its account, as they do for
/// `local_cache_priority`.
#[test]
fn keeping_a_payload_local_is_not_recorded_in_the_fragment() {
    let kept = WriteOptions::default().with_keep_local();
    assert!(kept.cache_local());
    assert_eq!(
        kept.as_u32() & FragmentFlags::PayloadLocalCachePriority.bits(),
        0
    );

    let prioritized = WriteOptions::default().with_local_cache_priority();
    assert!(prioritized.cache_local());
    assert_ne!(
        prioritized.as_u32() & FragmentFlags::PayloadLocalCachePriority.bits(),
        0
    );

    assert!(!WriteOptions::default().cache_local());
}

async fn make_test_store() -> (TempDir, Arc<dyn ImmutableStore>) {
    let dir = TempDir::new("lore-storage-follower-test-");
    let store = LocalImmutableStore::new(
        Some(PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("create test store");
    (dir, store)
}

fn make_address(seed: u8) -> (Partition, Address) {
    let payload = vec![seed; 64];
    let hash = lore_storage::hash::hash_slice(&payload);
    (
        Partition::from([seed; 16]),
        Address {
            hash,
            context: Context::from([seed; 16]),
        },
    )
}

#[tokio::test]
async fn follower_returns_ok_when_leader_wrote_terminal_entry() {
    let (_dir, store) = make_test_store().await;
    let (partition, address) = make_address(0xAA);
    let payload = vec![0xAA; 64];
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    store
        .clone()
        .put(
            partition,
            address,
            fragment,
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .expect("put terminal entry");

    let token = CancellationToken::new();
    token.cancel();
    follower_future(store, partition, address, token)
        .await
        .expect("follower should observe terminal entry stored locally");
}

#[tokio::test]
async fn follower_returns_err_when_no_entry_exists() {
    let (_dir, store) = make_test_store().await;
    let (partition, address) = make_address(0xBB);

    let token = CancellationToken::new();
    token.cancel();
    let err = follower_future(store, partition, address, token)
        .await
        .expect_err("follower should fail when no terminal entry");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("leader upload failed"),
        "expected leader-fail diagnostic, got: {msg}"
    );
}

#[tokio::test]
async fn follower_waits_for_token_before_querying() {
    let (_dir, store) = make_test_store().await;
    let (partition, address) = make_address(0xCC);
    let payload = vec![0xCC; 64];
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredDurable.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    let token = CancellationToken::new();
    let follower = lore_base::lore_spawn!(follower_future(
        store.clone(),
        partition,
        address,
        token.clone(),
    ));

    // Follower is waiting on the token. Write the entry AFTER spawn, THEN cancel.
    store
        .clone()
        .put(
            partition,
            address,
            fragment,
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .expect("put terminal entry");
    token.cancel();

    follower
        .await
        .expect("join follower")
        .expect("follower observed terminal entry stored durably");
}

/// Which resolutions name a source worth copying from. Everything here is about not naming one
/// that would cost a refused round trip and an upload afterwards.
mod copy_source_selection {
    use super::*;

    fn address() -> Address {
        Address {
            hash: lore_storage::hash::hash_slice(b"copy source selection"),
            context: Context::from([0x01u8; 16]),
        }
    }

    fn durable(match_made: StoreMatch) -> StoreMatchResult {
        StoreMatchResult {
            match_made,
            partition: Partition::from([0x02u8; 16]),
            context: Context::from([0x03u8; 16]),
            stored_local: true,
            stored_durable: true,
        }
    }

    #[test]
    fn a_partition_match_names_what_the_resolution_found() {
        let source = copy_source(&durable(StoreMatch::MatchPartition), address())
            .expect("a durable partition match names a source");
        assert_eq!(source.partition, Partition::from([0x02u8; 16]));
        assert_eq!(source.address.hash, address().hash);
        assert_eq!(source.address.context, Context::from([0x03u8; 16]));
    }

    #[test]
    fn a_hash_match_names_the_partition_it_was_found_in() {
        let source = copy_source(&durable(StoreMatch::MatchHash), address())
            .expect("a durable hash match names a source");
        assert_eq!(source.partition, Partition::from([0x02u8; 16]));
    }

    /// A resolution that named no context leaves the source naming none either, which is what
    /// the store reads as any association in the partition.
    #[test]
    fn an_unnamed_context_stays_unnamed() {
        let resolved = StoreMatchResult {
            context: Context::default(),
            ..durable(StoreMatch::MatchPartition)
        };
        let source = copy_source(&resolved, address()).expect("still names a source");
        assert!(source.address.context.is_zero());
    }

    #[test]
    fn a_full_match_names_nothing() {
        assert!(copy_source(&durable(StoreMatch::MatchFull), address()).is_none());
    }

    #[test]
    fn no_match_names_nothing() {
        assert!(copy_source(&durable(StoreMatch::MatchNone), address()).is_none());
    }

    /// The local store holding an association says nothing about the peer holding it, and a
    /// copy naming a source the peer never received is a round trip that can only fail.
    #[test]
    fn a_match_the_peer_never_received_names_nothing() {
        let resolved = StoreMatchResult {
            stored_durable: false,
            ..durable(StoreMatch::MatchPartition)
        };
        assert!(copy_source(&resolved, address()).is_none());
    }

    #[test]
    fn a_match_without_a_partition_names_nothing() {
        let resolved = StoreMatchResult {
            partition: Partition::default(),
            ..durable(StoreMatch::MatchPartition)
        };
        assert!(copy_source(&resolved, address()).is_none());
    }

    /// A destination context of zero is not a self-copy: the partition match is the statement
    /// that this tuple is not one of the associations the source names.
    #[test]
    fn a_zero_destination_context_still_names_a_source() {
        let destination = Address {
            hash: address().hash,
            context: Context::default(),
        };
        assert!(copy_source(&durable(StoreMatch::MatchPartition), destination).is_some());
    }
}

fn make_input(seed: u8) -> (Partition, Address, Fragment, Bytes) {
    let payload = vec![seed; 64];
    let hash = lore_storage::hash::hash_slice(&payload);
    let partition = Partition::from([seed; 16]);
    let address = Address {
        hash,
        context: Context::from([seed; 16]),
    };
    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    (partition, address, fragment, Bytes::from(payload))
}

/// [`make_input`] rehomed under `partition`.
fn make_input_in(partition: Partition, seed: u8) -> (Partition, Address, Fragment, Bytes) {
    let (_, address, fragment, buffer) = make_input(seed);
    (partition, address, fragment, buffer)
}

#[tokio::test]
async fn store_fragment_no_tracker_writes_synchronously() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x10);

    let result = store_fragment(
        store.clone(),
        partition,
        address,
        fragment,
        buffer,
        true,
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("synchronous store_fragment");

    assert_eq!(result.address, address);
    assert!(!result.deduplicated);

    // Entry should be present in the store after the call returns.
    let query = store
        .get_metadata(partition, address)
        .await
        .expect("query after sync write");
    assert_eq!(query.match_made, StoreMatch::MatchFull);
    assert_ne!(
        query.fragment.flags & FragmentFlags::PayloadStoredLocal.bits(),
        0,
        "sync write should leave PayloadStoredLocal set"
    );
}

#[tokio::test]
async fn store_fragment_already_durable_short_circuits() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, mut fragment, buffer) = make_input(0x20);
    // Pre-populate with a durable entry.
    fragment.flags = FragmentFlags::PayloadStoredDurable.bits();
    store
        .clone()
        .put(partition, address, fragment, Some(buffer.clone()), false)
        .await
        .expect("pre-populate durable entry");

    let tracker = Arc::new(WriteTracker::new());
    let fresh_fragment = Fragment {
        flags: 0,
        size_payload: buffer.len() as u32,
        size_content: buffer.len() as u64,
    };
    let result = store_fragment(
        store.clone(),
        partition,
        address,
        fresh_fragment,
        buffer.clone(),
        false,
        None,
        WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("store_fragment against already-durable entry");

    assert!(result.deduplicated, "should dedup on already-durable");
    assert!(
        result.stored_durable,
        "result should report the entry as durable"
    );
    // Tracker should have no outstanding work.
    assert!(tracker.await_all().await.is_ok());
}

#[tokio::test]
async fn store_fragment_follower_path_registers_in_tracker() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x30);

    // Manually hold a STORE_IN_FLIGHT guard to force the follower path.
    let held_guard =
        try_acquire_in_flight(&store, partition, address).expect("acquire in-flight guard");

    let tracker = Arc::new(WriteTracker::new());
    let result = store_fragment(
        store.clone(),
        partition,
        address,
        fragment,
        buffer,
        false,
        None,
        WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("store_fragment in follower path");
    assert!(result.deduplicated, "follower path should report dedup");

    // Drop the guard — this cancels the token. Follower queries the store
    // and sees no entry → returns an error.
    drop(held_guard);

    let await_result = tracker.await_all().await;
    let err = await_result.expect_err("follower sees no entry, errors");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("leader upload failed"),
        "expected follower's leader-fail error, got: {msg}"
    );
}

/// A write to a store while a write of the same address to another store is in flight stores the
/// content in its own store, rather than following the other store's write.
#[tokio::test]
async fn a_write_beside_another_stores_write_of_its_address_stores_its_own() {
    let (_held_dir, held_store) = make_test_store().await;
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x31);
    let _held =
        try_acquire_in_flight(&held_store, partition, address).expect("acquire in-flight guard");
    let session = Arc::new(StorageSession::pending(|| async {
        Err(lore_transport::ProtocolError::internal(
            "the stand-in remote takes no upload",
        ))
    }));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        store_fragment(
            store.clone(),
            partition,
            address,
            fragment,
            buffer,
            false,
            Some(session),
            WriteContext::counted(None),
            None,
        ),
    )
    .await
    .expect("the write waited on the other store's write")
    .expect("store_fragment");

    assert!(!result.deduplicated);
    assert!(result.stored_local);
    let resolved = query_one(&store, partition, address)
        .await
        .expect("query the store");
    assert_eq!(resolved.match_made, StoreMatch::MatchFull);
    assert!(resolved.stored_local);
}

/// [`a_write_beside_another_stores_write_of_its_address_stores_its_own`], for a write the tracker
/// dispatches.
#[tokio::test]
async fn a_dispatched_write_beside_another_stores_write_of_its_address_stores_its_own() {
    let (_held_dir, held_store) = make_test_store().await;
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x32);
    let _held =
        try_acquire_in_flight(&held_store, partition, address).expect("acquire in-flight guard");

    let tracker = Arc::new(WriteTracker::new());
    let result = store_fragment(
        store.clone(),
        partition,
        address,
        fragment,
        buffer,
        false,
        None,
        WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("store_fragment");
    tokio::time::timeout(std::time::Duration::from_secs(10), tracker.await_all())
        .await
        .expect("the write waited on the other store's write")
        .expect("the write stores its own content");

    assert!(!result.deduplicated);
    let resolved = query_one(&store, partition, address)
        .await
        .expect("query the store");
    assert_eq!(resolved.match_made, StoreMatch::MatchFull);
    assert!(resolved.stored_local);
}

#[tokio::test]
async fn store_fragment_leader_path_spawns_into_tracker_no_remote() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x40);

    let tracker = Arc::new(WriteTracker::new());
    let result = store_fragment(
        store.clone(),
        partition,
        address,
        fragment,
        buffer,
        true,
        None,
        WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("store_fragment leader spawn");
    assert!(!result.deduplicated);

    // Leader hasn't necessarily finished yet. Await tracker to drain.
    tracker.await_all().await.expect("tracker await_all");

    // After await_all, the entry should be in the store.
    let query = store
        .get_metadata(partition, address)
        .await
        .expect("query after leader completed");
    assert_eq!(query.match_made, StoreMatch::MatchFull);
    assert_ne!(
        query.fragment.flags & FragmentFlags::PayloadStoredLocal.bits(),
        0,
        "leader (no remote) should leave PayloadStoredLocal set"
    );
}

/// Wrapper that delegates to an inner `ImmutableStore` but forces `put`
/// to fail. Exercises the error-terminal lifecycle state: a leader
/// task whose terminal write fails surfaces the error through the
/// tracker's `await_all`.
struct FailingPutStore {
    inner: Arc<dyn ImmutableStore>,
}

#[async_trait::async_trait]
impl ImmutableStore for FailingPutStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::ImmutableStore::hold_for_command(self.inner.clone()).await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    fn is_local(&self) -> bool {
        self.inner.clone().is_local()
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
        _fragment: Fragment,
        _payload: Option<Bytes>,
        _force: bool,
    ) -> Result<(), StoreError> {
        Err(StoreError::internal("FailingPutStore: put disabled"))
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<lore_storage::store_types::StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

#[tokio::test]
async fn leader_error_surfaces_through_tracker_await_all() {
    let (_dir, inner) = make_test_store().await;
    let failing: Arc<dyn ImmutableStore> = Arc::new(FailingPutStore { inner });
    let (partition, address, fragment, buffer) = make_input(0x60);
    let tracker = Arc::new(WriteTracker::new());

    // Sync path returns before the leader has run. write_raw (which calls
    // put) is inside the leader task; its error surfaces via await_all.
    let result = store_fragment(
        failing.clone(),
        partition,
        address,
        fragment,
        buffer,
        true,
        None,
        WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("sync path returns Ok — work is deferred to the leader");
    assert!(!result.deduplicated);

    // Await the tracker. The leader's write_raw must fail and the error
    // must propagate through the tracker.
    let err = tracker
        .await_all()
        .await
        .expect_err("leader put fails; tracker surfaces error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("FailingPutStore") || msg.contains("put"),
        "expected diagnostic mentioning the failing put, got: {msg}"
    );

    // After await_all returns, no terminal entry exists — confirming the
    // leader's failure left the store in its original empty state.
    let resolved = query_one(&(failing as Arc<dyn ImmutableStore>), partition, address)
        .await
        .expect("resolve on empty store");
    assert_eq!(resolved.match_made, StoreMatch::MatchNone);
}

/// A write of content the local store holds and the remote does not reuses the stored payload as it
/// is: the local copy is trusted, so it is neither decoded nor hashed. The stand-in remote refuses
/// the upload that follows.
#[tokio::test]
async fn a_write_reuses_the_stored_payload_unhashed() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x70);
    store_fragment(
        store.clone(),
        partition,
        address,
        fragment,
        buffer.clone(),
        false,
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("store the content locally");
    let session = Arc::new(StorageSession::pending(|| async {
        Err(lore_transport::ProtocolError::internal(
            "the stand-in remote takes no upload",
        ))
    }));
    lore_storage::test_util::reset_counts();

    let _ = store_fragment(
        store,
        partition,
        address,
        fragment,
        buffer,
        false,
        Some(session),
        WriteContext::none(),
        None,
    )
    .await;

    let (_, decodes, hashes) = lore_storage::test_util::counts();
    assert_eq!((decodes, hashes), (0, 0));
}

/// A write of content the local store holds under sizes that disagree does not reuse that payload,
/// which is not the content, and prepares the payload from the content it was given. The stand-in
/// remote refuses the upload that follows.
#[tokio::test]
async fn a_write_does_not_reuse_a_stored_payload_whose_sizes_disagree() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x71);
    let claims_more = Fragment {
        size_payload: 1,
        ..fragment
    };
    lore_storage::test_util::accepting_unvalidated_puts(store.clone().put(
        partition,
        address,
        claims_more,
        Some(buffer.slice(..1)),
        false,
    ))
    .await
    .expect("store a payload claiming more content than it holds");
    let session = Arc::new(StorageSession::pending(|| async {
        Err(lore_transport::ProtocolError::internal(
            "the stand-in remote takes no upload",
        ))
    }));
    let stats = Arc::new(lore_storage::write_stats::FragmentWriteStats::default());

    let _ = store_fragment(
        store,
        partition,
        address,
        fragment,
        buffer,
        false,
        Some(session),
        WriteContext::counted(Some(stats.clone())),
        None,
    )
    .await;

    let prepared = stats.snapshot();
    assert_eq!(prepared.data_fragments, 1, "one payload is prepared");
    assert_ne!(
        prepared.data_payload_bytes, 1,
        "the stored payload was reused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_of_same_address_dedup_through_tracker() {
    // Two concurrent store_fragment calls for the same (partition, address)
    // should produce exactly one leader task and one follower — both calls
    // must succeed, and the store must end up with one terminal entry.
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x50);
    let tracker = Arc::new(WriteTracker::new());

    let call = |cache_local| {
        let store = store.clone();
        let buffer = buffer.clone();
        let tracker = tracker.clone();
        async move {
            store_fragment(
                store,
                partition,
                address,
                fragment,
                buffer,
                cache_local,
                None,
                WriteContext::tracked(Some(tracker), None),
                None,
            )
            .await
        }
    };

    let (r1, r2) = tokio::join!(call(true), call(true));
    let r1 = r1.expect("first writer");
    let r2 = r2.expect("second writer");

    // Exactly one of the two calls is the leader (deduplicated == false);
    // the other is a follower (deduplicated == true via the in-flight
    // short-circuit).
    let leader_count = usize::from(!r1.deduplicated) + usize::from(!r2.deduplicated);
    assert_eq!(
        leader_count, 1,
        "expected exactly one leader, got {leader_count} (r1.dedup={}, r2.dedup={})",
        r1.deduplicated, r2.deduplicated
    );

    // Both calls return the same address.
    assert_eq!(r1.address, address);
    assert_eq!(r2.address, address);

    // Drain the tracker so the leader task and follower future complete.
    tracker
        .await_all()
        .await
        .expect("tracker await_all succeeds");

    // Exactly one terminal entry exists in the store.
    let query = store
        .get_metadata(partition, address)
        .await
        .expect("query after concurrent writers");
    assert_eq!(query.match_made, StoreMatch::MatchFull);
    assert_ne!(
        query.fragment.flags & FragmentFlags::PayloadStoredLocal.bits(),
        0,
        "terminal entry should carry PayloadStoredLocal"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn many_concurrent_writers_all_succeed_with_single_upload() {
    // Generalisation of the 2-writer case: N concurrent writers on the
    // same address all succeed, exactly one becomes the leader.
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, buffer) = make_input(0x51);
    let tracker = Arc::new(WriteTracker::new());

    const N: usize = 64;
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let store = store.clone();
        let buffer = buffer.clone();
        let tracker = tracker.clone();
        handles.push(lore_base::lore_spawn!(async move {
            store_fragment(
                store,
                partition,
                address,
                fragment,
                buffer,
                true,
                None,
                WriteContext::tracked(Some(tracker), None),
                None,
            )
            .await
        }));
    }

    let mut leader_count = 0usize;
    for h in handles {
        let r = h.await.expect("join").expect("store_fragment success");
        if !r.deduplicated {
            leader_count += 1;
        }
    }
    assert_eq!(leader_count, 1, "expected 1 leader across {N} writers");
    tracker
        .await_all()
        .await
        .expect("tracker await_all succeeds");

    let query = store.get_metadata(partition, address).await.expect("query");
    assert_eq!(query.match_made, StoreMatch::MatchFull);
}

/// Wrapper that delegates to an inner `ImmutableStore`, holding `put` back
/// so a test can say when one finishes, and counting the ones that have.
///
/// `delay` sleeps inside `put`, simulating a slow backing store or, by
/// analogy, a high-RTT remote. `gate` instead parks `put` until the test
/// hands out a permit, which makes "no put has finished" a fact a counter
/// reports rather than a wall-clock comparison: on a loaded machine the
/// time a call takes says more about the machine than about the code.
struct DelayingPutStore {
    inner: Arc<dyn ImmutableStore>,
    delay: std::time::Duration,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    completed: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ImmutableStore for DelayingPutStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::ImmutableStore::hold_for_command(self.inner.clone()).await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    fn is_local(&self) -> bool {
        self.inner.clone().is_local()
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        tokio::time::sleep(self.delay).await;
        if let Some(gate) = self.gate.clone() {
            gate.acquire().await.expect("gate closed").forget();
        }
        let result = self
            .inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await;
        self.completed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        result
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<lore_storage::store_types::StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

/// The tracker's win is that `store_fragment` hands the write to a leader
/// task instead of awaiting it, so a caller's cost stops scaling with the
/// store's latency.
///
/// The inline half pays that latency and is measured: `put` really sleeps,
/// and a loaded machine only makes the wait longer, so the lower bound
/// holds however busy the machine is.
///
/// The deferred half is not measured. Every `put` parks on a gate holding
/// no permits, so the run asserts that all `N` calls returned while nothing
/// had been written — true whatever the machine does with the tasks in the
/// meantime — and then releases the gate and drains. Comparing the two
/// wall-clock times instead would assert a ratio between a path that waits
/// on timers and one that waits on the scheduler, which contention moves
/// by two orders of magnitude in opposite directions.
///
/// Both halves write under a partition of this test's own, so a gated
/// leader parked here can neither be joined by another test's write nor
/// stand in for one.
///
/// `GATE_GUARD` bounds the parked half so a regression that put inline
/// under a tracker fails rather than hanging. It is not a latency
/// assertion: the calls it covers await no timer, and the budget is orders
/// of magnitude above what they take.
#[tokio::test(flavor = "multi_thread")]
async fn tracker_parallelises_writes_vs_inline_serialisation() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    const N: usize = 100;
    const PUT_DELAY: Duration = Duration::from_millis(10);
    const GATE_GUARD: Duration = Duration::from_secs(120);

    let test_partition = Partition::from([0xB4u8; 16]);

    let (_dir, inner) = make_test_store().await;
    let inline_completed = Arc::new(AtomicUsize::new(0));
    let store: Arc<dyn ImmutableStore> = Arc::new(DelayingPutStore {
        inner,
        delay: PUT_DELAY,
        gate: None,
        completed: inline_completed.clone(),
    });

    let inline_start = tokio::time::Instant::now();
    for i in 0..N {
        let (partition, address, fragment, buffer) = make_input_in(test_partition, i as u8);
        store_fragment(
            store.clone(),
            partition,
            address,
            fragment,
            buffer,
            true,
            None,
            WriteContext::none(), // no tracker → inline path awaits the slow put.
            None,
        )
        .await
        .expect("inline store_fragment");
    }
    let inline_elapsed = inline_start.elapsed();

    assert_eq!(
        inline_completed.load(Ordering::SeqCst),
        N,
        "inline store_fragment must return with its put finished"
    );
    assert!(
        inline_elapsed >= PUT_DELAY * N as u32 / 2,
        "inline baseline too fast; got {inline_elapsed:?}, expected at least ~{:?}",
        PUT_DELAY * N as u32 / 2
    );

    let (_dir2, inner2) = make_test_store().await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let deferred_completed = Arc::new(AtomicUsize::new(0));
    let store2: Arc<dyn ImmutableStore> = Arc::new(DelayingPutStore {
        inner: inner2,
        delay: Duration::ZERO,
        gate: Some(gate.clone()),
        completed: deferred_completed.clone(),
    });
    let tracker = Arc::new(WriteTracker::new());

    let deferred_start = tokio::time::Instant::now();
    tokio::time::timeout(GATE_GUARD, async {
        for i in 0..N {
            let (partition, address, fragment, buffer) = make_input_in(test_partition, i as u8);
            store_fragment(
                store2.clone(),
                partition,
                address,
                fragment,
                buffer,
                true,
                None,
                WriteContext::tracked(Some(tracker.clone()), None),
                None,
            )
            .await
            .expect("deferred store_fragment sync return");
        }
    })
    .await
    .expect("deferred store_fragment blocked on a put that cannot finish");
    let sync_return_elapsed = deferred_start.elapsed();

    assert_eq!(
        deferred_completed.load(Ordering::SeqCst),
        0,
        "deferred store_fragment must return before the store has written anything"
    );

    gate.add_permits(N);
    tracker.await_all().await.expect("tracker await_all");
    let deferred_total_elapsed = deferred_start.elapsed();

    assert_eq!(
        deferred_completed.load(Ordering::SeqCst),
        N,
        "await_all must drain every leader the tracker took on"
    );

    eprintln!(
        "latency bench N={N} delay={PUT_DELAY:?}: inline={inline_elapsed:?} \
             deferred_sync_return={sync_return_elapsed:?} deferred_total={deferred_total_elapsed:?}"
    );
}

/// Wrapper that delegates to an inner `ImmutableStore` and tracks how many
/// `put` calls are in flight at any moment, plus the peak. Used by the
/// permit-stress test to verify the budget invariant: peak concurrent
/// buffers in the put pipeline never exceeds what the semaphore allows.
///
/// The sleep inside `put` ensures multiple leaders actually overlap so the
/// peak is observable — without it, puts can serialize fast enough that a
/// passing test wouldn't prove anything.
struct CountingPutStore {
    inner: Arc<dyn ImmutableStore>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
    put_delay: std::time::Duration,
}

#[async_trait::async_trait]
impl ImmutableStore for CountingPutStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::ImmutableStore::hold_for_command(self.inner.clone()).await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    fn is_local(&self) -> bool {
        self.inner.clone().is_local()
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.inner.clone().get(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        use std::sync::atomic::Ordering as AtomicOrdering;
        let current = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        self.peak.fetch_max(current, AtomicOrdering::SeqCst);
        tokio::time::sleep(self.put_delay).await;
        let result = self
            .inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await;
        self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
        result
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<lore_storage::store_types::StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_permit_stress_caps_concurrent_leaders_under_budget() {
    // REQ-F-2 (spec / plan task #15): configure the memory budget to a
    // small value, spawn leader tasks that would collectively need ~10x
    // the budget, and assert:
    //   (a) all spawned tasks reach a terminal state,
    //   (b) peak concurrent `put` calls ≤ budget / per-task permit cost,
    //   (c) every permit is released by the time await_all returns.
    //
    // A dedicated Arc<Semaphore> stands in for the global fragment
    // limiter (which is a process-wide OnceLock). `store_fragment`
    // accepts a pre-acquired OwnedSemaphorePermit, so callers can
    // transparently substitute any semaphore — the leader still owns
    // the permit for the duration of the buffer, which is what matters.
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use tokio::sync::Semaphore;

    const PER_TASK_COST: u32 = lore_storage::concurrency::FRAGMENT_MINIMUM_COST_KIB;
    const MAX_CONCURRENT: usize = 16;
    const BUDGET_PERMITS: usize = MAX_CONCURRENT * PER_TASK_COST as usize;
    const N: usize = MAX_CONCURRENT * 10;
    const PUT_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

    let semaphore = Arc::new(Semaphore::new(BUDGET_PERMITS));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let (_dir, inner) = make_test_store().await;
    let store: Arc<dyn ImmutableStore> = Arc::new(CountingPutStore {
        inner,
        in_flight: in_flight.clone(),
        peak: peak.clone(),
        put_delay: PUT_DELAY,
    });
    let tracker = Arc::new(WriteTracker::new());

    // Each task gets a unique `context`, which with the payload-derived hash gives N unique
    // addresses.
    let test_partition = Partition::from([0xA7u8; 16]);

    // Spawn N call-site coroutines. Each acquires its own permit from the
    // dedicated semaphore before calling store_fragment — mirroring the
    // production call pattern in write_content / write_fragmented.
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let semaphore = semaphore.clone();
        let store = store.clone();
        let tracker = tracker.clone();
        handles.push(lore_base::lore_spawn!(async move {
            // 64-byte buffers clamp to PER_TASK_COST permits via
            // fragment_permit_count. Distinct context per task keeps
            // addresses unique inside `test_partition`.
            let seed = i as u8;
            let payload = vec![seed; 64];
            let hash = lore_storage::hash::hash_slice(&payload);
            let address = Address {
                hash,
                context: Context::from([seed; 16]),
            };
            let fragment = Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            };
            let buffer = Bytes::from(payload);
            let permit = semaphore
                .acquire_many_owned(PER_TASK_COST)
                .await
                .expect("semaphore not closed");
            store_fragment(
                store,
                test_partition,
                address,
                fragment,
                buffer,
                true,
                None,
                WriteContext::tracked(Some(tracker), None),
                Some(permit),
            )
            .await
        }));
    }

    // All sync-path returns must succeed — the store hasn't errored, the
    // semaphore is large enough to eventually admit every task.
    for h in handles {
        h.await
            .expect("join spawner")
            .expect("store_fragment sync return");
    }

    // (a) All leaders reach a terminal state.
    tracker
        .await_all()
        .await
        .expect("await_all drains every leader without error");

    // (b) Peak concurrent `put` calls must not exceed the budget. This is
    // the safety property: more simultaneous buffers than the budget
    // allows would mean the permit stopped bounding memory.
    let observed_peak = peak.load(AtomicOrdering::SeqCst);
    assert!(
        observed_peak <= MAX_CONCURRENT,
        "peak concurrent put ({observed_peak}) exceeded budget ({MAX_CONCURRENT})"
    );
    // Sanity check: the test actually stressed the semaphore. If peak is
    // 1 the sleep/scheduling didn't produce overlap and the upper-bound
    // assertion above is vacuous.
    assert!(
        observed_peak > 1,
        "peak ({observed_peak}) too low to prove concurrency was exercised; \
             the test is not meaningfully validating the budget"
    );

    // (c) All permits are released. Every leader dropped its permit when
    // it dropped its buffer.
    assert_eq!(
        in_flight.load(AtomicOrdering::SeqCst),
        0,
        "puts still in flight after await_all"
    );
    assert_eq!(
        semaphore.available_permits(),
        BUDGET_PERMITS,
        "all permits must be released back to the semaphore"
    );
}

/// Deterministic and non-repeating, so a window read or sliced at the wrong offset
/// changes the hash of every chunk it touches instead of comparing equal by accident.
fn hash_test_content(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

/// The fragment list for `content` cut at `sizes`, hashed the way the compare path
/// hashes the file.
fn fragment_list_for(content: &[u8], sizes: &[usize]) -> Vec<FragmentReference> {
    let mut list = Vec::new();
    let mut offset = 0;
    for &size in sizes {
        list.push(FragmentReference {
            hash: Hash::hash_buffer(&content[offset..offset + size]),
            offset_content: offset as u64,
        });
        offset += size;
    }
    assert_eq!(
        offset,
        content.len(),
        "sizes must cover the content exactly"
    );
    list
}

/// Chunk sizes covering `length` in `size` steps plus whatever remains. A step that
/// does not divide the window is the interesting case: chunks then straddle window
/// boundaries, which is what the read-ahead has to stitch together.
fn chunk_sizes(length: usize, size: usize) -> Vec<usize> {
    let mut sizes = vec![size; length / size];
    if !length.is_multiple_of(size) {
        sizes.push(length % size);
    }
    sizes
}

async fn compare_file(content: &[u8], chunks: &[FragmentReference]) -> FileMatch {
    let (dir, store) = make_test_store().await;
    let path = PathBuf::from(dir.as_ref()).join("hash-compare.bin");
    std::fs::write(&path, content).expect("write test file");
    let (file, file_size) = lore_storage::content::ContentSource::file(&path)
        .open()
        .await
        .expect("open");
    let source = ContentSource::file(&path);

    compare_previous_chunks(
        SublistSource {
            store: &store,
            partition: Partition::from([7u8; 16]),
            context: Address::default().context,
            remote_session: &None,
            trust_remote: false,
        },
        &source,
        &file,
        file_size,
        chunks,
    )
    .await
    .expect("compare must not error on a readable file")
}

#[tokio::test]
async fn an_unchanged_file_matches_across_every_window() {
    // Five windows' worth, cut so no chunk boundary lands on a window boundary.
    let content = hash_test_content(5 * HASH_WINDOW_SIZE + 4_321);
    let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));
    assert!(chunks.len() > 20, "test wants many chunks per window");

    assert_eq!(
        compare_file(&content, &chunks).await,
        FileMatch::Match,
        "unchanged file must match its own fragment list"
    );
}

#[tokio::test]
async fn a_single_window_file_matches() {
    let content = hash_test_content(HASH_WINDOW_SIZE - 17);
    let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 200_000));

    assert_eq!(
        compare_file(&content, &chunks).await,
        FileMatch::Match,
        "file smaller than one window must match"
    );
}

/// The chunk is in the third window, so detecting it proves later windows are read at
/// the offset their chunks are hashed against — a window off by even one byte here
/// mismatches for a file that is in fact unchanged, and every `status` would
/// re-fragment it.
#[tokio::test]
async fn a_byte_changed_in_a_late_chunk_differs() {
    let content = hash_test_content(5 * HASH_WINDOW_SIZE + 4_321);
    let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));

    let mut changed = content.clone();
    let victim = 2 * HASH_WINDOW_SIZE + 11;
    changed[victim] ^= 0xff;

    assert_eq!(
        compare_file(&changed, &chunks).await,
        FileMatch::Differs,
        "a changed byte in the third window is a difference in content"
    );
}

/// The same list against a shorter file. The last chunk is measured to the end of the
/// file rather than to the offset the list records, so the missing bytes show up as the
/// content difference they are.
#[tokio::test]
async fn a_file_shorter_than_its_list_differs() {
    let content = hash_test_content(3 * HASH_WINDOW_SIZE);
    let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));

    assert_eq!(
        compare_file(&content[..content.len() - 1_000], &chunks).await,
        FileMatch::Differs,
    );
}

/// A list whose offsets do not ascend describes something other than this file, and the
/// walk reports that as a difference: the list is what defines the ranges being hashed,
/// so one that misdescribes the content is indistinguishable from content that changed.
/// The chunk size used to be an unchecked subtraction, which underflowed on it.
#[tokio::test]
async fn a_list_that_does_not_ascend_differs_without_underflowing() {
    let content = hash_test_content(3 * HASH_WINDOW_SIZE);
    let mut chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));
    chunks.swap(1, 2);

    assert_eq!(compare_file(&content, &chunks).await, FileMatch::Differs);
}

/// A chunk over the threshold is itself a fragment list, and the walk compares that
/// list's chunks instead. Those bytes are already in the resident window, which is why
/// the splice does not invalidate it.
#[tokio::test]
async fn a_recursively_fragmented_chunk_is_compared_through_its_sublist() {
    let content = hash_test_content(3 * HASH_WINDOW_SIZE + 4_321);
    let nested = 300 * 1024;
    let (chunks, store, partition, path, _dir) =
        recursive_case(&content, nested, &chunk_sizes(nested, 100 * 1024)).await;

    assert_eq!(
        compare_recursive(&store, partition, &path, &content, &chunks).await,
        FileMatch::Match,
        "unchanged file must match through the sublist"
    );
}

/// A nested entry pointing at a sublist nothing stored leaves the walk unable to read
/// the bytes it would have compared. Reporting that as a content difference would drop
/// the caller's content-comparison fallback for a file that may well be unchanged.
#[tokio::test]
async fn a_sublist_that_cannot_be_loaded_is_indeterminate() {
    let content = hash_test_content(3 * HASH_WINDOW_SIZE + 4_321);
    let nested = 300 * 1024;
    let (mut chunks, store, partition, path, _dir) =
        recursive_case(&content, nested, &chunk_sizes(nested, 100 * 1024)).await;
    chunks[0].hash = Hash::hash_buffer(b"a sublist that was never stored");

    assert_eq!(
        compare_recursive(&store, partition, &path, &content, &chunks).await,
        FileMatch::Indeterminate,
        "an unloadable sublist settles nothing about the content"
    );
}

/// Proves the sublist is genuinely compared rather than accepted because its parent
/// entry loaded: the changed byte is only covered by a sub-chunk hash.
#[tokio::test]
async fn a_byte_changed_inside_a_recursively_fragmented_chunk_differs() {
    let content = hash_test_content(3 * HASH_WINDOW_SIZE + 4_321);
    let nested = 300 * 1024;
    let (chunks, store, partition, path, _dir) =
        recursive_case(&content, nested, &chunk_sizes(nested, 100 * 1024)).await;

    let mut changed = content.clone();
    changed[250 * 1024] ^= 0xff;
    std::fs::write(&path, &changed).expect("rewrite test file");

    assert_eq!(
        compare_recursive(&store, partition, &path, &changed, &chunks).await,
        FileMatch::Differs,
        "a changed byte inside the nested range is a difference in content"
    );
}

/// Store `content` cut the way this build cuts, so a rehash can reproduce it.
async fn store_current_chunking(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    path: &Path,
) -> Address {
    let (file, file_size) = lore_storage::content::ContentSource::file(path)
        .open()
        .await
        .expect("open");
    lore_storage::fragment_engine::write_fragmented_from_file(
        Arc::clone(store),
        partition,
        Context::default(),
        file,
        file_size as usize,
        WriteOptions::default().no_remote_write(),
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("store the file")
    .0
}

/// Store `content` cut at boundaries below the minimum this build cuts at, which
/// no rehash of it can reproduce, under a list stating `size_content` bytes.
async fn store_foreign_chunking_sized(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    content: &[u8],
    size_content: u64,
) -> Address {
    use zerocopy::IntoBytes;

    let list = fragment_list_for(content, &chunk_sizes(content.len(), 17 * 1024));
    let payload = Bytes::copy_from_slice(list.as_slice().as_bytes());
    let address = Address {
        context: Address::default().context,
        hash: lore_storage::hash::hash_slice(&payload),
    };
    store_fragment(
        Arc::clone(store),
        partition,
        address,
        Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: payload.len() as u32,
            size_content,
        },
        payload,
        true,
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("store the fragment list");
    address
}

/// [`store_foreign_chunking_sized`] stating the size the content actually is.
async fn store_foreign_chunking(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    content: &[u8],
) -> Address {
    store_foreign_chunking_sized(store, partition, content, content.len() as u64).await
}

/// Store `content` as one fragment addressed by its own hash, which is the shape
/// the buffer-hash comparison is written for.
async fn store_single_fragment(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    content: &[u8],
) -> Address {
    let address = Address {
        context: Address::default().context,
        hash: Hash::hash_buffer(content),
    };
    store_fragment(
        Arc::clone(store),
        partition,
        address,
        Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        },
        Bytes::copy_from_slice(content),
        true,
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("store the fragment");
    address
}

/// A file of `size` bytes on disk, and a store to compare it against.
async fn compare_case(size: usize) -> (Vec<u8>, TempDir, Arc<dyn ImmutableStore>, PathBuf) {
    let content = hash_test_content(size);
    let (dir, store) = make_test_store().await;
    let path = PathBuf::from(dir.as_ref()).join("compare-case.bin");
    std::fs::write(&path, &content).expect("write test file");
    (content, dir, store, path)
}

/// Rewrite the file with one byte changed, which keeps its size.
fn edit_in_place(path: &Path, content: &[u8]) {
    let mut edited = content.to_vec();
    edited[content.len() / 2] ^= 0xff;
    std::fs::write(path, &edited).expect("rewrite test file");
}

async fn compare(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    path: &Path,
    address: Address,
    stored_size: usize,
) -> FileMatch {
    file_matches(
        store,
        partition,
        address,
        Some(stored_size),
        None,
        false,
        &ContentSource::file(path),
        &ContentHashes::default(),
    )
    .await
    .expect("comparing a readable file must not error")
}

/// Content whose bytes vary, so content-defined cutting finds boundaries in it.
fn pseudo_random_content(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

/// A resolved write publishes a mapping to stored content, so it refuses to address content
/// without storing it rather than publish a mapping to content nobody holds.
#[tokio::test]
async fn a_resolved_write_refuses_to_only_address() {
    let (dir, store) = make_test_store().await;
    let mutable = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        store.clone(),
    )
    .await
    .expect("create test mutable store");
    let partition = Partition::from([13u8; 16]);
    let key = lore_storage::hash::hash_slice(b"a key");
    let flags = WriteOptions::default().hash_only();

    assert!(
        write_resolved(
            store.clone(),
            mutable.clone(),
            partition,
            key,
            Context::default(),
            Bytes::from_static(b"content"),
            flags,
            None,
            WriteContext::none(),
        )
        .await
        .is_err()
    );

    let path = PathBuf::from(dir.as_ref()).join("resolved.bin");
    std::fs::write(&path, b"content").expect("write test file");
    assert!(
        write_resolved_from_file(
            store,
            mutable,
            partition,
            key,
            Context::default(),
            path.as_path(),
            flags,
            None,
            WriteContext::none(),
        )
        .await
        .is_err()
    );
}

/// Addressing content stores none of it, in either band the size rule cuts at.
#[tokio::test]
async fn addressing_content_stores_nothing() {
    let (dir, store) = make_test_store().await;
    let partition = Partition::from([12u8; 16]);

    for size in [1024, lore_storage::compress::FRAGMENT_SIZE_THRESHOLD * 3] {
        let path = PathBuf::from(dir.as_ref()).join(format!("unstored-{size}.bin"));
        std::fs::write(&path, pseudo_random_content(size)).expect("write test file");

        let hash = hash_file(store.clone(), partition, &ContentSource::file(&path), None)
            .await
            .expect("addressing the content");

        let described = store
            .clone()
            .get_metadata(partition, Address::zero_context_hash(hash))
            .await;
        assert!(
            !described.is_ok_and(|described| described.match_made != StoreMatch::MatchNone),
            "addressing {size} bytes stored them"
        );
    }
}

/// Addressing content without storing it answers what storing it would have addressed it by,
/// across every band the size rule cuts at: empty, one fragment, and a list.
///
/// The bands matter rather than the sizes. A file in the band below the fragmentation
/// threshold is stored as one fragment addressed by its own hash, where cutting it would
/// produce a list of several chunks and a different address.
#[tokio::test]
async fn addressing_content_agrees_with_storing_it() {
    let (dir, store) = make_test_store().await;
    let partition = Partition::from([11u8; 16]);

    for size in [
        0,
        1024,
        lore_storage::compress::FRAGMENT_SIZE_THRESHOLD - 1,
        lore_storage::compress::FRAGMENT_SIZE_THRESHOLD * 3,
    ] {
        let path = PathBuf::from(dir.as_ref()).join(format!("content-{size}.bin"));
        std::fs::write(&path, pseudo_random_content(size)).expect("write test file");

        let source = ContentSource::file(&path);
        let stored = write_from_file(
            store.clone(),
            partition,
            &source,
            Context::default(),
            WriteOptions::default().no_remote_write(),
            None,
            WriteContext::none(),
        )
        .await
        .expect("storing the content")
        .address
        .hash;

        let addressed = hash_file(store.clone(), partition, &source, None)
            .await
            .expect("addressing the content");

        assert_eq!(stored, addressed, "at {size} bytes");
    }
}

/// Content beyond reach is an answer, not a failure: a comparison that never happened is
/// distinct from one the machinery could not carry out, and only the second is an error.
#[tokio::test]
async fn content_that_cannot_be_read_answers_unreadable() {
    let (dir, store) = make_test_store().await;
    let path = PathBuf::from(dir.as_ref()).join("was-never-written.bin");

    let compared = file_matches(
        store,
        Partition::from([9u8; 16]),
        Address::zero_context_hash(lore_storage::hash::hash_slice(b"content")),
        Some(7),
        None,
        false,
        &ContentSource::file(&path),
        &ContentHashes::default(),
    )
    .await
    .expect("a path holding nothing is not a failure");

    assert_eq!(FileMatch::Unreadable, compared);
}

/// Virtual immutable content is settled by the address it names, reading neither it nor the store,
/// at any size: an address with its hash matches, under any context, and one with another hash
/// differs.
#[tokio::test]
async fn virtual_content_is_settled_by_the_address_it_names() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x5a; 16]);
    for size in [SINGLE_FRAGMENT_SIZE, LISTED_SIZE] {
        let address = Address {
            hash: lore_storage::hash::hash_slice(&size.to_le_bytes()),
            context: Context::from([0x5a; 16]),
        };
        let source = ContentSource::virtual_immutable(address, size as u64);
        for (previous, expected) in [
            (address, FileMatch::Match),
            (
                Address {
                    context: Context::from([0x5b; 16]),
                    ..address
                },
                FileMatch::Match,
            ),
            (
                Address {
                    hash: lore_storage::hash::hash_slice(b"other content"),
                    ..address
                },
                FileMatch::Differs,
            ),
        ] {
            let compared = file_matches(
                store.clone(),
                partition,
                previous,
                Some(size),
                None,
                false,
                &source,
                &ContentHashes::default(),
            )
            .await
            .expect("comparing virtual content must not error");

            assert_eq!(compared, expected, "at {size} bytes against {previous}");
        }
    }
}

/// Bigger than the minimum cut and smaller than the threshold, which is the band a
/// file is stored as a list in and its own hash answers nothing for.
const FRAGMENTED_SIZE: usize = 150 * 1024;

/// Smaller than the minimum cut, so it is one fragment and its own hash is its
/// address.
const SINGLE_FRAGMENT_SIZE: usize = 20 * 1024;

/// Larger than a fragment holds, so the content is always a list.
const LISTED_SIZE: usize = 300 * 1024;

/// Between the minimum cut and the threshold, where the content may be either and the
/// stored header is what says which.
const EITHER_SIZE: usize = 100 * 1024;

#[tokio::test]
async fn current_chunking_matches_the_unchanged_file() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address = store_current_chunking(&store, partition, &path).await;

    assert_ne!(
        address.hash,
        Hash::hash_buffer(&content),
        "The file has to be stored as a list for this to be the case under test"
    );
    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Match
    );
}

#[tokio::test]
async fn current_chunking_differs_from_an_edit_of_the_same_size() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address = store_current_chunking(&store, partition, &path).await;
    edit_in_place(&path, &content);

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Differs
    );
}

/// Without the list, rehashing the file under the chunking that stored it
/// reproduces the address, which settles it.
#[tokio::test]
async fn current_chunking_matches_the_unchanged_file_without_its_list() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address = store_current_chunking(&store, partition, &path).await;
    let (_empty_dir, empty_store) = make_test_store().await;

    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Match
    );
}

/// A rehash that does not reproduce the address says nothing: the content may have
/// changed, or the chunking may have.
#[tokio::test]
async fn current_chunking_without_its_list_is_indeterminate_on_an_edit() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address = store_current_chunking(&store, partition, &path).await;
    edit_in_place(&path, &content);
    let (_empty_dir, empty_store) = make_test_store().await;

    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Indeterminate
    );
}

/// Above the threshold the content is always a list, so its own hash is never tested and
/// the stored chunking is the only thing that answers.
/// One fragment in the band the header decides: its own hash is the address, so the
/// content answers without the payload being read.
#[tokio::test]
async fn one_fragment_in_the_header_decided_band_matches_by_its_own_hash() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(EITHER_SIZE).await;
    let address = store_single_fragment(&store, partition, &content).await;

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Match
    );
}

#[tokio::test]
async fn one_fragment_in_the_header_decided_band_differs_from_an_edit() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(EITHER_SIZE).await;
    let address = store_single_fragment(&store, partition, &content).await;
    edit_in_place(&path, &content);

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Differs
    );
}

/// With nothing to describe the object, the content's own hash is still what answers for
/// one fragment.
#[tokio::test]
async fn one_fragment_in_the_header_decided_band_matches_without_its_store() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(EITHER_SIZE).await;
    let address = store_single_fragment(&store, partition, &content).await;
    let (_empty_dir, empty_store) = make_test_store().await;

    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Match
    );
}

#[tokio::test]
async fn a_file_above_the_threshold_matches_through_its_list() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(LISTED_SIZE).await;
    let address = store_current_chunking(&store, partition, &path).await;

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Match
    );
}

/// The list of a file above the threshold, fetched from the remote, is checked on receipt unless
/// the reader trusts the remote, which hashes it once less.
#[tokio::test]
async fn a_fetched_list_is_checked_unless_the_remote_is_trusted() {
    let partition = Partition::from([0x3d; 16]);
    let (content, _dir, written, path) = compare_case(LISTED_SIZE).await;
    let address = store_current_chunking(&written, partition, &path).await;
    let (fragment, list) = lore_storage::read::read_raw(written, partition, address)
        .await
        .expect("the stored list")
        .raw;
    lore_storage::test_util::serve_remote(address, fragment, list);
    let (_empty_dir, empty) = make_test_store().await;
    let session = Arc::new(StorageSession::pending(|| async {
        Err(lore_transport::ProtocolError::internal(
            "the stand-in remote needs no session",
        ))
    }));

    let mut hashes = Vec::new();
    for trust_remote in [false, true] {
        lore_storage::test_util::reset_counts();
        let compared = file_matches(
            empty.clone(),
            partition,
            address,
            Some(content.len()),
            Some(session.clone()),
            trust_remote,
            &ContentSource::file(&path),
            &ContentHashes::default(),
        )
        .await
        .expect("comparing against a fetched list");
        assert_eq!(compared, FileMatch::Match);
        hashes.push(lore_storage::test_util::counts().2);
    }
    assert_eq!(
        hashes[0],
        hashes[1] + 1,
        "the list is hashed on receipt only where the remote is not trusted"
    );
}

#[tokio::test]
async fn a_file_above_the_threshold_matches_by_rehashing_without_its_list() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(LISTED_SIZE).await;
    let address = store_current_chunking(&store, partition, &path).await;
    let (_empty_dir, empty_store) = make_test_store().await;

    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Match
    );
}

#[tokio::test]
async fn a_file_above_the_threshold_is_indeterminate_on_an_edit_without_its_list() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(LISTED_SIZE).await;
    let address = store_current_chunking(&store, partition, &path).await;
    edit_in_place(&path, &content);
    let (_empty_dir, empty_store) = make_test_store().await;

    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Indeterminate
    );
}

#[tokio::test]
async fn foreign_chunking_matches_the_unchanged_file() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address = store_foreign_chunking(&store, partition, &content).await;

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Match,
        "The stored chunking answers for the file whatever cut it"
    );
}

#[tokio::test]
async fn foreign_chunking_differs_from_an_edit_of_the_same_size() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address = store_foreign_chunking(&store, partition, &content).await;
    edit_in_place(&path, &content);

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Differs
    );
}

/// A foreign chunking is not reproducible by rehashing, so without the list there is
/// nothing left to answer with and an unchanged file reads as indeterminate. Only
/// fetching the list settles it.
#[tokio::test]
async fn foreign_chunking_without_its_list_is_indeterminate() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address = store_foreign_chunking(&store, partition, &content).await;
    let (_empty_dir, empty_store) = make_test_store().await;

    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Indeterminate
    );
}

/// The size the caller states settles it before the store is touched, which is why an
/// empty store answers it.
#[tokio::test]
async fn a_file_of_another_size_differs_before_the_store_is_touched() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, _store, path) = compare_case(FRAGMENTED_SIZE).await;
    let (_empty_dir, empty_store) = make_test_store().await;
    std::fs::write(&path, &content[..content.len() - 1_000]).expect("truncate test file");

    assert_eq!(
        compare(
            empty_store,
            partition,
            &path,
            Address {
                context: Address::default().context,
                hash: Hash::hash_buffer(b"nothing stored"),
            },
            content.len()
        )
        .await,
        FileMatch::Differs
    );
}

/// A stored list describing more content than the file holds describes something else,
/// which the caller's own size cannot catch.
#[tokio::test]
async fn a_list_stating_another_size_differs() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(FRAGMENTED_SIZE).await;
    let address =
        store_foreign_chunking_sized(&store, partition, &content, content.len() as u64 + 1).await;

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Differs
    );
}

#[tokio::test]
async fn a_single_fragment_matches_the_unchanged_file_from_its_own_hash() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(SINGLE_FRAGMENT_SIZE).await;
    let address = store_single_fragment(&store, partition, &content).await;
    let (_empty_dir, empty_store) = make_test_store().await;
    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Match
    );
    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Match,
        "Its own hash needs no store to answer"
    );
}

#[tokio::test]
async fn a_single_fragment_differs_from_an_edit_of_the_same_size() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(SINGLE_FRAGMENT_SIZE).await;
    let address = store_single_fragment(&store, partition, &content).await;
    edit_in_place(&path, &content);

    assert_eq!(
        compare(store, partition, &path, address, content.len()).await,
        FileMatch::Differs
    );
}

/// Below the minimum cut the content is never chunked, so its own hash settles the edit
/// without the store describing anything.
#[tokio::test]
async fn a_single_fragment_differs_from_an_edit_without_its_store() {
    let partition = Partition::from([7u8; 16]);
    let (content, _dir, store, path) = compare_case(SINGLE_FRAGMENT_SIZE).await;
    let address = store_single_fragment(&store, partition, &content).await;
    edit_in_place(&path, &content);
    let (_empty_dir, empty_store) = make_test_store().await;

    assert_eq!(
        compare(empty_store, partition, &path, address, content.len()).await,
        FileMatch::Differs
    );
}

/// A file whose first `nested` bytes are one fragmented chunk, cut into `sub_sizes`,
/// with that sublist stored so the walk can load it. Sublist offsets are absolute in
/// the whole content, which is what lets the splice produce a flat list.
async fn recursive_case(
    content: &[u8],
    nested: usize,
    sub_sizes: &[usize],
) -> (
    Vec<FragmentReference>,
    Arc<dyn ImmutableStore>,
    Partition,
    PathBuf,
    TempDir,
) {
    use zerocopy::IntoBytes;

    let (dir, store) = make_test_store().await;
    let path = PathBuf::from(dir.as_ref()).join("hash-compare-nested.bin");
    std::fs::write(&path, content).expect("write test file");
    let partition = Partition::from([7u8; 16]);

    let sublist = fragment_list_for(&content[..nested], sub_sizes);
    let payload = Bytes::copy_from_slice(sublist.as_slice().as_bytes());
    let sublist_hash = lore_storage::hash::hash_slice(&payload);
    store_fragment(
        Arc::clone(&store),
        partition,
        Address {
            context: Address::default().context,
            hash: sublist_hash,
        },
        Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: payload.len() as u32,
            size_content: nested as u64,
        },
        payload,
        true,
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("store sublist");

    // The nested chunk stands in for its whole range, followed by ordinary chunks.
    let mut chunks = vec![FragmentReference {
        hash: sublist_hash,
        offset_content: 0,
    }];
    let mut offset = nested;
    for size in chunk_sizes(content.len() - nested, 100_003) {
        chunks.push(FragmentReference {
            hash: Hash::hash_buffer(&content[offset..offset + size]),
            offset_content: offset as u64,
        });
        offset += size;
    }

    (chunks, store, partition, path, dir)
}

async fn compare_recursive(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    path: &Path,
    content: &[u8],
    chunks: &[FragmentReference],
) -> FileMatch {
    let (file, file_size) = lore_storage::content::ContentSource::file(path)
        .open()
        .await
        .expect("open");
    assert_eq!(file_size, content.len() as u64);
    let source = ContentSource::file(path);

    compare_previous_chunks(
        SublistSource {
            store,
            partition,
            context: Address::default().context,
            remote_session: &None,
            trust_remote: false,
        },
        &source,
        &file,
        file_size,
        chunks,
    )
    .await
    .expect("compare must not error on a readable file")
}

#[test]
fn the_read_ahead_starts_at_the_first_chunk_the_window_does_not_hold() {
    let content = hash_test_content(3 * HASH_WINDOW_SIZE);
    let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));
    let file_size = content.len() as u64;

    let offset = next_window_offset(&chunks, 0, HASH_WINDOW_SIZE as u64, file_size)
        .expect("a chunk must straddle the first window boundary");
    let straddling = chunks
        .iter()
        .position(|chunk| chunk.offset_content == offset)
        .expect("offset must be a chunk boundary");
    assert!(
        offset < HASH_WINDOW_SIZE as u64,
        "the chunk starts inside the window it is not held by"
    );
    assert!(
        chunk_end(&chunks, straddling, file_size).expect("ascending") > HASH_WINDOW_SIZE as u64,
        "and ends past it"
    );
}

#[test]
fn there_is_nothing_to_read_ahead_at_the_end_of_the_list() {
    let content = hash_test_content(HASH_WINDOW_SIZE);
    let chunks = fragment_list_for(&content, &chunk_sizes(content.len(), 100_003));

    assert_eq!(
        next_window_offset(&chunks, 0, content.len() as u64, content.len() as u64),
        None,
        "a window reaching the end of the file has no successor"
    );
}

/// A chunk over the threshold is fragmented further, so the walk loads its sublist
/// rather than reading those bytes. Reading ahead there would read a window nothing
/// asks for.
#[test]
fn there_is_nothing_to_read_ahead_before_a_recursively_fragmented_chunk() {
    let content = hash_test_content(2 * HASH_WINDOW_SIZE);
    let sizes = vec![
        lore_storage::compress::FRAGMENT_SIZE_THRESHOLD,
        content.len() - lore_storage::compress::FRAGMENT_SIZE_THRESHOLD,
    ];
    let chunks = fragment_list_for(&content, &sizes);

    assert_eq!(
        next_window_offset(
            &chunks,
            0,
            lore_storage::compress::FRAGMENT_SIZE_THRESHOLD as u64,
            content.len() as u64
        ),
        None,
        "the next chunk is over the threshold and is not read as bytes"
    );
}
/// A file that fits one fragment is written as one, so the file write does not hold the
/// fragmented write that content of any size may take.
#[tokio::test]
async fn a_file_write_does_not_hold_the_fragmented_write() {
    let (_dir, store) = make_test_store().await;
    let (partition, address) = make_address(1);
    let path = PathBuf::from("content");
    let source = ContentSource::file(&path);

    let file = write_from_file(
        store.clone(),
        partition,
        &source,
        address.context,
        WriteOptions::default(),
        None,
        WriteContext::none(),
    );
    let content = write_content(
        store,
        partition,
        address.context,
        Bytes::new(),
        WriteOptions::default(),
        None,
        WriteContext::none(),
        None,
    );

    assert!(
        size_of_val(&file) < size_of_val(&content),
        "the file write holds {} bytes, the content write {}",
        size_of_val(&file),
        size_of_val(&content)
    );
}

/// High-entropy content, which compression cannot shrink, so a write stores it as it came.
fn incompressible_content(length: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        })
        .collect()
}

/// The entries a lent write and a shared write of `content` leave, each in a store of its own.
async fn lent_and_shared_entries(content: &[u8]) -> ((Fragment, Bytes), (Fragment, Bytes)) {
    let (partition, address) = make_address(0x40);
    let (_lent_dir, lent_store) = make_test_store().await;
    let (_shared_dir, shared_store) = make_test_store().await;
    let lent = write_content_borrowed(
        lent_store.clone(),
        partition,
        address.context,
        content,
        WriteOptions::default(),
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("lent write");
    let shared = write_content(
        shared_store.clone(),
        partition,
        address.context,
        Bytes::copy_from_slice(content),
        WriteOptions::default(),
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("shared write");
    assert_eq!(lent.address, shared.address);
    let lent_entry = lent_store
        .get(partition, lent.address)
        .await
        .and_then(StoreGetData::into_payload)
        .expect("lent entry");
    let shared_entry = shared_store
        .get(partition, shared.address)
        .await
        .and_then(StoreGetData::into_payload)
        .expect("shared entry");
    (lent_entry, shared_entry)
}

/// The upload and the local write of lent content keep one copy of it.
#[test]
fn lent_content_is_copied_once() {
    let content = incompressible_content(64);
    let mut payload = Payload::Lent(&content);
    let uploaded = payload.share();
    let stored = payload.into_shared();
    assert_eq!(uploaded, content);
    assert_eq!(uploaded.as_ptr(), stored.as_ptr());
}

#[tokio::test]
async fn lent_content_that_compresses_is_stored_compressed() {
    let content = vec![7; 4096];
    let (lent, shared) = lent_and_shared_entries(&content).await;
    assert_eq!(lent, shared);
    assert!(lent.1.len() < content.len());
}

#[tokio::test]
async fn lent_content_that_does_not_compress_is_stored_as_a_copy() {
    let content = incompressible_content(4096);
    let (lent, shared) = lent_and_shared_entries(&content).await;
    assert_eq!(lent, shared);
    assert_eq!(lent.1, content);
}

#[tokio::test]
async fn lent_content_over_one_fragment_is_stored_fragmented() {
    let content = hash_test_content(lore_storage::compress::FRAGMENT_SIZE_THRESHOLD + 4096);
    let (lent, shared) = lent_and_shared_entries(&content).await;
    assert_eq!(lent, shared);
}

/// A write with a durable association in the local store as its copy source. The peer answers copy
/// requests with `SlowDown`, a refusal, or success.
mod copy_association {
    use lore_storage::write_stats::FragmentWriteCounts;
    use lore_storage::write_stats::FragmentWriteStats;
    use lore_transport::ProtocolError;
    use lore_transport::test_util::CopyingPeer;

    use super::*;

    /// What the peer was asked for while the write under test ran, and how that write ended.
    struct Written {
        result: StoreResult,
        copies: usize,
        puts: usize,
        counts: FragmentWriteCounts,
        stored: StoreMatchResult,
    }

    /// Stores content durably under one context of `partition`, then writes the same content under
    /// another context of `partition`. The second write uses the first association as its copy
    /// source.
    async fn write_beside_a_durable_association(
        partition: Partition,
        seed: u8,
        slow_downs: usize,
        refusal: Option<ProtocolError>,
    ) -> Written {
        let (_dir, store) = make_test_store().await;
        let peer = CopyingPeer::new(slow_downs, refusal);
        let session = Arc::new(peer.session(partition));
        let (partition, held, fragment, buffer) = make_input_in(partition, seed);
        store_fragment(
            store.clone(),
            partition,
            held,
            fragment,
            buffer.clone(),
            false,
            Some(session.clone()),
            WriteContext::none(),
            None,
        )
        .await
        .expect("store the association to copy from");
        let wanted = Address {
            hash: held.hash,
            context: Context::from([seed.wrapping_add(1); 16]),
        };
        let copies_before = peer.copies();
        let puts_before = peer.puts();
        let stats = Arc::new(FragmentWriteStats::default());

        let result = store_fragment(
            store.clone(),
            partition,
            wanted,
            fragment,
            buffer,
            false,
            Some(session),
            WriteContext::counted(Some(stats.clone())),
            None,
        )
        .await
        .expect("a throttled or refused copy does not fail the write");

        let stored = query_one(&store, partition, wanted)
            .await
            .expect("query the written association");
        Written {
            result,
            copies: peer.copies() - copies_before,
            puts: peer.puts() - puts_before,
            counts: stats.snapshot(),
            stored,
        }
    }

    /// A copy the peer throttles and then accepts registers the association without its payload.
    #[tokio::test(start_paused = true)]
    async fn a_throttled_copy_that_is_accepted_uploads_nothing() {
        let written =
            write_beside_a_durable_association(Partition::from([0xC1; 16]), 0xC1, 2, None).await;

        assert_eq!(written.copies, 3);
        assert_eq!(written.puts, 0);
        assert_eq!(written.counts.remote_copy_writes, 1);
        assert!(written.result.stored_durable);
        assert_eq!(written.stored.match_made, StoreMatch::MatchFull);
        assert!(written.stored.stored_durable);
    }

    /// If the peer throttles a copy until retries run out, no payload is uploaded. The payload stays
    /// local without being marked durable, as when an upload runs out of retries.
    #[tokio::test(start_paused = true)]
    async fn a_copy_throttled_through_every_retry_keeps_the_payload_local() {
        let written =
            write_beside_a_durable_association(Partition::from([0xC3; 16]), 0xC3, usize::MAX, None)
                .await;

        assert_eq!(written.copies, lore_storage::store_retry().limit() + 1);
        assert_eq!(written.puts, 0);
        assert_eq!(written.counts.remote_copy_writes, 0);
        assert_eq!(written.counts.remote_upload_failed, 1);
        assert!(!written.result.stored_durable);
        assert!(written.result.stored_local);
        assert_eq!(written.stored.match_made, StoreMatch::MatchFull);
        assert!(!written.stored.stored_durable);
    }

    /// A copy refused for a reason other than throttling is not retried, and the payload is uploaded.
    #[tokio::test(start_paused = true)]
    async fn a_refused_copy_falls_back_to_uploading() {
        let written = write_beside_a_durable_association(
            Partition::from([0xC5; 16]),
            0xC5,
            0,
            Some(ProtocolError::from(lore_base::error::NotFound)),
        )
        .await;

        assert_eq!(written.copies, 1);
        assert_eq!(written.puts, 1);
        assert_eq!(written.counts.remote_copy_writes, 0);
        assert_eq!(written.counts.remote_put_writes, 1);
        assert!(written.result.stored_durable);
        assert!(written.stored.stored_durable);
    }

    /// A refusal that follows throttling still falls back to uploading the payload.
    #[tokio::test(start_paused = true)]
    async fn a_throttled_copy_that_is_then_refused_falls_back_to_uploading() {
        let written = write_beside_a_durable_association(
            Partition::from([0xC7; 16]),
            0xC7,
            2,
            Some(ProtocolError::from(lore_base::error::NotFound)),
        )
        .await;

        assert_eq!(written.copies, 3);
        assert_eq!(written.puts, 1);
        assert_eq!(written.counts.remote_copy_writes, 0);
        assert_eq!(written.counts.remote_put_writes, 1);
        assert!(written.result.stored_durable);
        assert!(written.stored.stored_durable);
    }
}

/// A copy the session's peer is asked for directly. The peer answers copy requests with
/// `SlowDown`, a refusal, or success.
mod remote_copy_retry {
    use lore_base::error::NotFound;
    use lore_storage::CopyOutcome;
    use lore_storage::remote_copy_retry;
    use lore_transport::ProtocolError;
    use lore_transport::test_util::CopyingPeer;

    use super::*;

    fn own() -> Partition {
        Partition::from([0xE1; 16])
    }

    fn source() -> Address {
        Address {
            hash: Hash::from([0xE2; 32]),
            context: Context::from([0xE3; 16]),
        }
    }

    /// Copies from `partition` into a fresh context on a peer whose responses are set by
    /// `slow_downs` and `refusal`. Returns the outcome and the copy requests the peer received.
    async fn copy_from(
        partition: Partition,
        slow_downs: usize,
        refusal: Option<ProtocolError>,
    ) -> (CopyOutcome, usize) {
        let peer = CopyingPeer::new(slow_downs, refusal);
        let session = peer.session(own());
        let outcome =
            remote_copy_retry(&session, partition, source(), Context::from([0xE4; 16])).await;
        (outcome, peer.copies())
    }

    #[tokio::test(start_paused = true)]
    async fn a_throttled_copy_is_retried_until_accepted() {
        assert_eq!(copy_from(own(), 2, None).await, (CopyOutcome::Copied, 3));
    }

    #[tokio::test(start_paused = true)]
    async fn a_copy_throttled_through_every_retry_is_reported_throttled() {
        assert_eq!(
            copy_from(own(), usize::MAX, None).await,
            (
                CopyOutcome::Throttled,
                lore_storage::store_retry().limit() + 1
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_not_retried() {
        assert_eq!(
            copy_from(own(), 0, Some(ProtocolError::from(NotFound))).await,
            (CopyOutcome::Refused, 1)
        );
    }

    /// A source partition the connection holds no claim to is refused without asking the peer.
    #[tokio::test(start_paused = true)]
    async fn a_source_partition_without_a_claim_is_refused_unasked() {
        assert_eq!(
            copy_from(Partition::from([0xE5; 16]), 0, None).await,
            (CopyOutcome::Refused, 0)
        );
    }
}
