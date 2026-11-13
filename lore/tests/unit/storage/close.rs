// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle as tree_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::handle::RevisionTreeGuard;
use lore::storage::close::*;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::OpGuard;
use lore::storage::store::disk_backed_for_tests;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::InvalidArguments;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_error_set::FfiError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_storage::FragmentFlags;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StorageError;
use lore_storage::StoreMatch;
use lore_storage::local::store_lock::StoreLock;
use lore_storage::test_util::PausePoint;
use lore_storage::test_util::fail_packfile_syncs;
use lore_storage::test_util::pause_at;

use super::put_payload;
use crate::call::test_support::registered_store;
use crate::revision_tree::support::load_tree;

/// Whether this process holds the lock of the store directory `name` under `root`.
fn held(root: &Path, name: &str) -> bool {
    StoreLock::new(root.join(name), false)
        .expect("a store directory")
        .is_held()
}

/// Waits until this process no longer holds the lock of the store directory `name` under `root`,
/// failing with `what` after five seconds.
async fn wait_until_released(root: &Path, name: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while held(root, name) {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Close must block until the in-flight counter drains. An `OpGuard` held by the test keeps
/// the counter > 0; close's `mark_invalid_and_await` must not complete until the guard
/// drops.
#[allow(clippy::disallowed_methods)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_waits_for_in_flight_counter_to_drain() {
    let (_store, store_handle) = registered_store("close-wait-test").await;
    let guard = OpGuard::enter(store_handle).expect("enter must succeed");

    let close_task = tokio::spawn(async move {
        close(
            LoreGlobalArgs::default(),
            LoreStorageCloseArgs {
                handle: store_handle,
                ..Default::default()
            },
            None,
        )
        .await
    });

    let deadline = Instant::now() + Duration::from_secs(1);
    while handle::lookup(store_handle).is_some() {
        if Instant::now() > deadline {
            panic!("close never unregistered the handle");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // The guard pins the in-flight counter at 1, so close must still be parked inside
    // `mark_invalid_and_await`.
    assert!(
        !close_task.is_finished(),
        "close must block while the in-flight counter is non-zero",
    );

    drop(guard);

    let status = close_task.await.expect("close task join");
    assert_eq!(
        status, 0,
        "close should report success after the counter drains"
    );
}

/// **Close completes once its flush has run.** A flush held at its first group keeps the close
/// from completing, and a store opened on the directory after the close finds what the handle
/// wrote.
#[tokio::test]
async fn close_completes_once_the_flush_of_what_the_handle_wrote_has_run() {
    let dir = TempDir::new("close-awaits-flush-");
    let store = disk_backed_for_tests("close-awaits-flush", dir.path()).await;
    let partition = Partition::from([0x3Cu8; 16]);
    let address = put_payload(&store.immutable, partition, b"what the handle wrote", 0).await;
    let store_handle = handle::register(store);

    let scanning = pause_at(PausePoint::Scanning);
    let close_task = lore_base::lore_spawn!(close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    ));
    tokio::time::timeout(Duration::from_secs(5), scanning.reached.notified())
        .await
        .expect("close's flush reaches its first group");
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !close_task.is_finished(),
        "close must not complete while its flush is held"
    );

    scanning.resume.notify_one();
    let status = close_task.await.expect("close task join");
    assert_eq!(status, 0, "close reports the flush's success");
    assert!(
        !held(dir.path(), "immutable") && !held(dir.path(), "mutable"),
        "and its flush has let the stores go"
    );

    let reopened = disk_backed_for_tests("close-awaits-flush-reopened", dir.path()).await;
    let found = reopened
        .immutable
        .clone()
        .get_metadata(partition, address)
        .await
        .expect("reads the reopened store");
    assert_eq!(
        found.match_made,
        StoreMatch::MatchFull,
        "the entry is on disk"
    );
}

/// **A close dropped during its flush leaves the flush to run to its end.** The flush runs in a
/// task of its own, so the store opened on the directory afterwards finds what the handle wrote.
#[tokio::test]
async fn a_close_dropped_during_its_flush_leaves_the_flush_to_run_to_its_end() {
    let dir = TempDir::new("close-dropped-");
    let store = disk_backed_for_tests("close-dropped", dir.path()).await;
    let partition = Partition::from([0x5Eu8; 16]);
    let address = put_payload(
        &store.immutable,
        partition,
        b"what the dropped close wrote",
        0,
    )
    .await;
    let store_handle = handle::register(store);

    let scanning = pause_at(PausePoint::Scanning);
    let close_task = lore_base::lore_spawn!(close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    ));
    tokio::time::timeout(Duration::from_secs(5), scanning.reached.notified())
        .await
        .expect("close's flush reaches its first group");
    close_task.abort();
    assert!(
        close_task.await.is_err_and(|err| err.is_cancelled()),
        "the close is dropped"
    );
    scanning.resume.notify_one();

    wait_until_released(
        dir.path(),
        "immutable",
        "the flush runs to its end and lets the store go",
    )
    .await;
    let reopened = disk_backed_for_tests("close-dropped-reopened", dir.path()).await;
    let found = reopened
        .immutable
        .clone()
        .get_metadata(partition, address)
        .await
        .expect("reads the reopened store");
    assert_eq!(
        found.match_made,
        StoreMatch::MatchFull,
        "the entry is on disk"
    );
}

/// **A close whose flush fails releases the handle and returns the error.** The handle is gone
/// whatever the flush did, so a second close finds no handle, and the mutable store is not
/// flushed once the immutable store's flush has failed.
#[tokio::test]
async fn a_close_whose_flush_fails_releases_the_handle_and_returns_the_error() {
    let dir = TempDir::new("close-flush-fails-");
    let store = disk_backed_for_tests("close-flush-fails", dir.path()).await;
    let partition = Partition::from([0x4Du8; 16]);
    put_payload(
        &store.immutable,
        partition,
        b"a payload whose sync fails",
        0,
    )
    .await;
    let key = Hash::from([0x6Eu8; 32]);
    store
        .mutable
        .clone()
        .store(
            partition,
            key,
            Hash::from([0xE6u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .expect("stores");
    let store_handle = handle::register(store);
    let args = LoreStorageCloseArgs {
        handle: store_handle,
        ..Default::default()
    };

    fail_packfile_syncs(1);
    let status = close(
        LoreGlobalArgs {
            sync_data: 1,
            ..Default::default()
        },
        args.clone(),
        None,
    )
    .await;
    assert_eq!(
        status,
        StorageError::internal("flush").ffi_code(),
        "close returns the flush's error"
    );
    assert!(
        handle::lookup(store_handle).is_none(),
        "and releases the handle"
    );

    let status = close(LoreGlobalArgs::default(), args, None).await;
    let no_handle = StorageError::from(InvalidArguments {
        reason: String::new(),
    });
    assert_eq!(
        status,
        no_handle.ffi_code(),
        "a second close finds no handle"
    );

    let reopened = disk_backed_for_tests("close-flush-fails-reopened", dir.path()).await;
    let loaded = reopened
        .mutable
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await;
    assert!(
        loaded.is_err_and(|err| err.is_address_not_found()),
        "the mutable store is not flushed once the immutable store's flush failed"
    );
}

/// The store outlives its storage handle for exactly as long as a revision tree still
/// references it. That reference is what makes reads work after a parent close, so it
/// has to be the last one dropped, not merely present.
#[tokio::test]
async fn the_store_tears_down_only_once_the_last_revision_handle_closes() {
    let store = in_memory_for_tests("close-refcount").await;
    let alive = Arc::downgrade(&store);
    let store_handle = handle::register(store);
    let tree = load_tree(store_handle, Partition::from([0x1Au8; 16])).await;

    let status = close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "closing the storage handle must succeed");
    assert!(
        alive.upgrade().is_some(),
        "the revision tree's reference must hold the store up",
    );

    let internal = tree_handle::unregister(tree).expect("the tree must still be registered");
    drop(internal);
    assert!(
        alive.upgrade().is_none(),
        "and dropping it must be what finally tears the store down",
    );
}

/// **A closing handle's flush holds the stores until it is done.** The handle's own
/// claims end with its state, which close does not keep for the flush, so the flush
/// runs under claims joined while the handle's were still held.
#[tokio::test]
async fn the_claims_joined_for_a_flush_outlive_the_handle() {
    let dir = lore_base::test_util::TempDir::new("close-flush-claims-");
    let store = disk_backed_for_tests("close-flush-claims", dir.path()).await;
    assert!(
        held(dir.path(), "immutable") && held(dir.path(), "mutable"),
        "an open handle claims both stores"
    );

    let claims = claims_for_flush(&store);
    assert_eq!(claims.len(), 2, "the flush joins a claim on each store");
    drop(store);
    assert!(
        held(dir.path(), "immutable") && held(dir.path(), "mutable"),
        "and those keep both stores once the handle's state is gone"
    );

    drop(claims);
    assert!(
        !held(dir.path(), "immutable") && !held(dir.path(), "mutable"),
        "until the flush lets them go"
    );
}

/// **A close that skips the flush leaves what the handle wrote unflushed.** A flush held at its
/// first group would keep the close from completing, and a store opened on the directory after
/// the close finds nothing.
#[tokio::test]
async fn a_close_that_skips_the_flush_leaves_what_the_handle_wrote_unflushed() {
    let dir = TempDir::new("close-skips-flush-");
    let store = disk_backed_for_tests("close-skips-flush", dir.path()).await;
    let partition = Partition::from([0x7Au8; 16]);
    let address = put_payload(&store.immutable, partition, b"what no flush writes", 0).await;
    let store_handle = handle::register(store);

    let _scanning = pause_at(PausePoint::Scanning);
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        close(
            LoreGlobalArgs::default(),
            LoreStorageCloseArgs {
                handle: store_handle,
                skip_flush: 1,
            },
            None,
        ),
    )
    .await
    .expect("close completes without a flush");
    assert_eq!(status, 0, "close succeeds");
    assert!(
        handle::lookup(store_handle).is_none(),
        "and releases the handle"
    );

    let reopened = disk_backed_for_tests("close-skips-flush-reopened", dir.path()).await;
    let found = reopened
        .immutable
        .clone()
        .get_metadata(partition, address)
        .await
        .expect("reads the reopened store");
    assert_eq!(
        found.match_made,
        StoreMatch::MatchNone,
        "the entry is not on disk"
    );
}

/// **Close waits for a garbage collection pass in flight to give up before its flush.** A
/// compaction pass held in its size eviction keeps the close from completing, and once resumed it
/// sees the close's stop request and gives up before it evicts, so the store opened on the
/// directory afterwards still holds the payload.
#[tokio::test]
async fn close_waits_for_a_gc_pass_in_flight_to_give_up() {
    let dir = TempDir::new("close-waits-for-gc-");
    let store = disk_backed_for_tests("close-waits-for-gc", dir.path()).await;
    let immutable = store.immutable.clone();
    let partition = Partition::from([0x6Cu8; 16]);
    let address = put_evictable_in_the_first_group(&immutable, partition).await;
    let store_handle = handle::register(store);

    let evicting = pause_at(PausePoint::Evicting);
    let compaction = lore_base::lore_spawn!(immutable.compact(1, None, false, None));
    tokio::time::timeout(Duration::from_secs(5), evicting.reached.notified())
        .await
        .expect("the compaction reaches its size eviction");
    let close_task = lore_base::lore_spawn!(close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle::lookup(store_handle).is_some() {
        assert!(Instant::now() < deadline, "close unregisters the handle");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !close_task.is_finished(),
        "close must not complete while a garbage collection pass is in flight"
    );

    evicting.resume.notify_one();
    compaction
        .await
        .expect("compaction task join")
        .expect("the compaction gives up without failing");
    assert_eq!(
        close_task.await.expect("close task join"),
        0,
        "close completes once the pass has given up"
    );

    let reopened = disk_backed_for_tests("close-waits-for-gc-reopened", dir.path()).await;
    assert!(
        reopened
            .immutable
            .clone()
            .get(partition, address)
            .await
            .is_ok(),
        "the pass gave up before it evicted the payload"
    );
}

/// Store a payload marked stored upstream, which eviction may drop, in the first group, where a
/// compaction pass from the start of the store begins, returning its address.
async fn put_evictable_in_the_first_group(
    immutable: &Arc<dyn ImmutableStore>,
    partition: Partition,
) -> Address {
    let payload = (0u32..)
        .map(u32::to_le_bytes)
        .find(|payload| lore_storage::hash_slice(payload).data()[0] == 0)
        .expect("a payload hashes into the first group");
    put_payload(
        immutable,
        partition,
        &payload,
        FragmentFlags::PayloadStoredDurable.bits(),
    )
    .await
}

/// A storage handle on a disk-backed store under `root`, opened by the IPC connection
/// `connection_id` where one is given, a revision tree loaded on it, and the handle's stores.
async fn tree_on_storage_handle(
    root: &Path,
    repository: Partition,
    connection_id: Option<u64>,
) -> (
    LoreStore,
    LoreRevisionTree,
    Arc<dyn ImmutableStore>,
    Arc<dyn MutableStore>,
) {
    let mut store = disk_backed_for_tests("tree-on-storage-handle", root).await;
    if let Some(connection_id) = connection_id {
        let opened = Arc::into_inner(store).expect("a store just built has one owner");
        store = Arc::new(opened.with_connection_id(connection_id));
    }
    let stores = (store.immutable.clone(), store.mutable.clone());
    let store_handle = handle::register(store);
    let tree = load_tree(store_handle, repository).await;
    (store_handle, tree, stores.0, stores.1)
}

/// Close `store_handle`, flushing its stores.
async fn close_storage_handle(store_handle: LoreStore) {
    let status = close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "closing the storage handle must succeed");
}

/// A revision tree loaded on a disk-backed store under `root`, and the tree's stores, once the
/// storage handle the tree was loaded against has closed.
async fn tree_after_storage_close(
    root: &Path,
    repository: Partition,
) -> (
    LoreRevisionTree,
    Arc<dyn ImmutableStore>,
    Arc<dyn MutableStore>,
) {
    let (store_handle, tree, immutable, mutable) =
        tree_on_storage_handle(root, repository, None).await;
    close_storage_handle(store_handle).await;
    (tree, immutable, mutable)
}

/// Store a branch metadata value under `key` in `mutable`, as a commit through a tree would.
async fn store_value(mutable: &Arc<dyn MutableStore>, repository: Partition, key: Hash) {
    mutable
        .clone()
        .store(
            repository,
            key,
            Hash::from([0xa5u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .expect("stores");
}

/// Close `tree` as `globals` and `skip_flush` ask, returning the status and the code its
/// `RevisionTreeCloseComplete` event carries.
async fn close_tree(
    tree: LoreRevisionTree,
    globals: LoreGlobalArgs,
    skip_flush: u8,
) -> (i32, Option<LoreErrorCode>) {
    let reported: Arc<Mutex<Option<LoreErrorCode>>> = Arc::new(Mutex::new(None));
    let sink = reported.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::RevisionTreeCloseComplete(data) = event {
            *sink.lock().unwrap() = Some(data.error_code);
        }
    }));
    let status = lore::revision_tree::close::close(
        globals,
        lore::revision_tree::close::LoreRevisionTreeCloseArgs {
            id: 1,
            handle: tree,
            skip_flush,
        },
        callback,
    )
    .await;
    let code = *reported.lock().unwrap();
    (status, code)
}

/// **A tree closed after its storage handle completes once what it wrote is flushed.** The
/// handle's close flushed what had been written by then; the tree's later writes keep the store's
/// flock until a flush writes them, so the close lets the store go before it completes.
#[tokio::test]
async fn a_tree_closed_after_its_storage_handle_completes_once_what_it_wrote_is_flushed() {
    let dir = TempDir::new("close-tree-flush-");
    let repository = Partition::from([0x2Bu8; 16]);
    let (tree, _immutable, mutable) = tree_after_storage_close(dir.path(), repository).await;
    store_value(&mutable, repository, Hash::from([0x5au8; 32])).await;

    let (status, code) = close_tree(tree, LoreGlobalArgs::default(), 0).await;
    assert_eq!(status, 0, "closing the tree must succeed");
    assert_eq!(code, Some(LoreErrorCode::None), "and reports so");
    assert!(
        !held(dir.path(), "mutable"),
        "the tree's close flushed what it wrote and let the store go"
    );
}

/// **A tree close whose flush fails releases the tree and returns the error.**
#[tokio::test]
async fn a_tree_close_whose_flush_fails_releases_the_tree_and_returns_the_error() {
    let dir = TempDir::new("close-tree-flush-fails-");
    let repository = Partition::from([0x3Du8; 16]);
    let (tree, immutable, _mutable) = tree_after_storage_close(dir.path(), repository).await;
    put_payload(&immutable, repository, b"a payload the tree wrote", 0).await;

    fail_packfile_syncs(1);
    let (status, code) = close_tree(
        tree,
        LoreGlobalArgs {
            sync_data: 1,
            ..Default::default()
        },
        0,
    )
    .await;
    assert_eq!(
        status,
        StorageError::internal("flush").ffi_code(),
        "the tree's close returns the flush's error"
    );
    assert_eq!(code, Some(LoreErrorCode::Internal), "and reports it");
    assert!(tree_handle::lookup(tree).is_none(), "and releases the tree");
}

/// **A tree close that skips the flush leaves what the tree wrote unflushed.** A store opened on
/// the directory once nothing holds the tree's stores finds nothing.
#[tokio::test]
async fn a_tree_close_that_skips_the_flush_leaves_what_the_tree_wrote_unflushed() {
    let dir = TempDir::new("close-tree-skips-flush-");
    let repository = Partition::from([0x4Eu8; 16]);
    let key = Hash::from([0x6Bu8; 32]);
    let (tree, immutable, mutable) = tree_after_storage_close(dir.path(), repository).await;
    store_value(&mutable, repository, key).await;
    drop((immutable, mutable));

    let (status, code) = close_tree(tree, LoreGlobalArgs::default(), 1).await;
    assert_eq!(status, 0, "closing the tree must succeed");
    assert_eq!(code, Some(LoreErrorCode::None), "and reports so");

    let reopened = disk_backed_for_tests("close-tree-skips-flush-reopened", dir.path()).await;
    let loaded = reopened
        .mutable
        .clone()
        .load(repository, key, KeyType::BranchMetadata)
        .await;
    assert!(
        loaded.is_err_and(|err| err.is_address_not_found()),
        "the value is not on disk"
    );
}

/// **Shutdown's drain of a tree flushes what the tree wrote after its storage handle closed.**
/// The tree's writes keep the store's flock until a flush writes them, so the drain's flush lets
/// the store go.
#[tokio::test]
async fn the_shutdown_drain_flushes_what_a_tree_wrote_after_its_storage_handle_closed() {
    let dir = TempDir::new("drain-tree-flush-");
    let repository = Partition::from([0x5Cu8; 16]);
    let (tree, _immutable, mutable) = tree_after_storage_close(dir.path(), repository).await;
    store_value(&mutable, repository, Hash::from([0x7Cu8; 32])).await;

    let internal = tree_handle::unregister(tree).expect("the tree must still be registered");
    lore::revision_tree::drain_in_parallel(vec![(tree.handle_id, internal)]).await;
    wait_until_released(
        dir.path(),
        "mutable",
        "the drain flushes what the tree wrote and lets the store go",
    )
    .await;
}

/// **Connection teardown flushes what the connection's trees wrote, whether the connection closed
/// their storage handle or left it open.** The tree on the closed handle flushes the stores
/// itself; the tree on the open handle leaves the flush to that handle's drain. Each tree's
/// writes keep its store's flock until a flush writes them, so teardown lets both stores go, and
/// a store opened on each directory afterwards finds the value.
#[tokio::test]
async fn teardown_flushes_what_the_trees_wrote_whether_their_storage_handle_closed_or_not() {
    const CONNECTION: u64 = 0x7EA2_D0F1;
    let dir = TempDir::new("teardown-tree-flush-");
    let closed_root = dir.path().join("closed");
    let open_root = dir.path().join("open");
    let repository = Partition::from([0x6Du8; 16]);
    let key = Hash::from([0x8Du8; 32]);

    let (closed_handle, on_closed, closed_immutable, closed_mutable) =
        tree_on_storage_handle(&closed_root, repository, Some(CONNECTION)).await;
    close_storage_handle(closed_handle).await;
    store_value(&closed_mutable, repository, key).await;
    let (open_handle, on_open, open_immutable, open_mutable) =
        tree_on_storage_handle(&open_root, repository, Some(CONNECTION)).await;
    store_value(&open_mutable, repository, key).await;

    lore::storage::close_for_connection(CONNECTION).await;
    assert!(
        tree_handle::lookup(on_closed).is_none() && tree_handle::lookup(on_open).is_none(),
        "teardown closes both trees"
    );
    assert!(
        handle::lookup(open_handle).is_none(),
        "and the storage handle the connection left open"
    );
    for root in [&closed_root, &open_root] {
        wait_until_released(
            root,
            "mutable",
            "teardown flushes what the tree wrote and lets the store go",
        )
        .await;
    }

    drop((
        closed_immutable,
        closed_mutable,
        open_immutable,
        open_mutable,
    ));
    for root in [&closed_root, &open_root] {
        let reopened = disk_backed_for_tests("teardown-tree-flush-reopened", root).await;
        reopened
            .mutable
            .clone()
            .load(repository, key, KeyType::BranchMetadata)
            .await
            .expect("the value the tree wrote is on disk");
    }
}

/// **A tree whose call outlasts the teardown's wait is flushed once the call ends.** The drain
/// stops waiting for the call, but detaches rather than aborts the tree's task, which starts the
/// flush once the call has ended.
#[tokio::test]
async fn teardown_flushes_a_tree_whose_call_outlasts_its_wait_once_the_call_ends() {
    const CONNECTION: u64 = 0x7EA2_D0F2;
    let dir = TempDir::new("teardown-late-flush-");
    let repository = Partition::from([0x6Eu8; 16]);
    let (store_handle, tree, _immutable, mutable) =
        tree_on_storage_handle(dir.path(), repository, Some(CONNECTION)).await;
    close_storage_handle(store_handle).await;
    let call = RevisionTreeGuard::enter(tree).expect("a call enters the tree");
    store_value(&mutable, repository, Hash::from([0x8Eu8; 32])).await;

    lore::revision_tree::close_for_connection_within(CONNECTION, Duration::from_millis(20)).await;
    assert!(
        held(dir.path(), "mutable"),
        "what the tree wrote waits for its call to end"
    );

    drop(call);
    wait_until_released(
        dir.path(),
        "mutable",
        "the drain flushes what the tree wrote once the call ends",
    )
    .await;
}
