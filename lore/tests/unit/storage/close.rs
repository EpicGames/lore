// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle as tree_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::storage::close::*;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::OpGuard;
use lore::storage::store::disk_backed_for_tests;
use lore::storage::store::in_memory_for_tests;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreEvent;

/// Load a revision tree against an already-registered storage handle.
async fn load_revision_tree(store_handle: LoreStore, repository: Partition) -> LoreRevisionTree {
    let loaded: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let sink = loaded.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::RevisionTreeLoaded(data) = event {
            *sink.lock().unwrap() = Some(data.handle_id);
        }
    }));
    let status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store: store_handle,
            repository,
            revision_hash: Hash::default(),
        },
        callback,
    )
    .await;
    assert_eq!(status, 0, "loading the revision tree fixture must succeed");
    let handle_id = loaded
        .lock()
        .unwrap()
        .expect("load must emit RevisionTreeLoaded");
    LoreRevisionTree { handle_id }
}

/// Close must block until the in-flight counter drains. An `OpGuard` held by the test keeps
/// the counter > 0; close's `mark_invalid_and_await` must not complete until the guard
/// drops.
#[allow(clippy::disallowed_methods)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_waits_for_in_flight_counter_to_drain() {
    let store = in_memory_for_tests("close-wait-test").await;
    let store_handle = handle::register(store.clone());
    let guard = OpGuard::enter(store_handle).expect("enter must succeed");

    let close_task = tokio::spawn(async move {
        close(
            LoreGlobalArgs::default(),
            LoreStorageCloseArgs {
                handle: store_handle,
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

/// The store outlives its storage handle for exactly as long as a revision tree still
/// references it. That reference is what makes reads work after a parent close, so it
/// has to be the last one dropped, not merely present.
#[tokio::test]
async fn the_store_tears_down_only_once_the_last_revision_handle_closes() {
    let store = in_memory_for_tests("close-refcount").await;
    let alive = Arc::downgrade(&store);
    let store_handle = handle::register(store);
    let tree = load_revision_tree(store_handle, Partition::from([0x1Au8; 16])).await;

    let status = close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
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
    let held = |name: &str| {
        lore_storage::local::store_lock::StoreLock::new(dir.path().join(name))
            .expect("a store directory")
            .is_held()
    };
    assert!(
        held("immutable") && held("mutable"),
        "an open handle claims both stores"
    );

    let claims = claims_for_flush(&store).await;
    assert_eq!(claims.len(), 2, "the flush joins a claim on each store");
    drop(store);
    assert!(
        held("immutable") && held("mutable"),
        "and those keep both stores once the handle's state is gone"
    );

    drop(claims);
    assert!(
        !held("immutable") && !held("mutable"),
        "until the flush lets them go"
    );
}

/// **A tree closed after its storage handle flushes what it wrote.** The handle's close
/// flushed what had been written by then; without a flush at the tree's own close, the
/// tree's later writes would keep the store's flock for as long as the process ran.
#[tokio::test]
async fn closing_a_tree_after_its_storage_handle_flushes_what_it_wrote() {
    let dir = lore_base::test_util::TempDir::new("close-tree-flush-");
    let store = disk_backed_for_tests("close-tree-flush", dir.path()).await;
    let mutable = store.mutable.clone();
    let repository = Partition::from([0x2Bu8; 16]);
    let store_handle = handle::register(store);
    let tree = load_revision_tree(store_handle, repository).await;
    let status = close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "closing the storage handle must succeed");

    // Written after the storage handle closed, as a commit through the tree would be.
    mutable
        .clone()
        .store(
            repository,
            Hash::from([0x5au8; 32]),
            Hash::from([0xa5u8; 32]),
            lore_base::types::KeyType::BranchMetadata,
        )
        .await
        .expect("stores");

    let status = lore::revision_tree::close::close(
        LoreGlobalArgs::default(),
        lore::revision_tree::close::LoreRevisionTreeCloseArgs {
            id: 1,
            handle: tree,
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "closing the tree must succeed");

    let held = || {
        lore_storage::local::store_lock::StoreLock::new(dir.path().join("mutable"))
            .expect("a store directory")
            .is_held()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while held() {
        assert!(
            Instant::now() < deadline,
            "the tree's close flushed what it wrote and let the store go"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
