// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore::shutdown` releases the handles a caller left open, and flushes what they wrote.
//!
//! One test in its own binary: shutdown drains the process-global registries and tears the
//! runtimes down for good, so it can share a process with nothing.

use std::sync::Arc;
use std::sync::Mutex;

use lore::revision_tree::load::load_empty_for_tests;
use lore::storage::close::LoreStorageCloseArgs;
use lore::storage::close::close;
use lore::storage::handle::LoreStore;
use lore::storage::open::LoreStorageOpenArgs;
use lore::storage::open::open;
use lore::storage::store::disk_backed_for_tests;
use lore_base::test_util::TempDir;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_storage::local::store_lock::StoreLock;

/// Open an in-memory store through `lore_storage_open`, returning its handle.
async fn open_in_memory() -> LoreStore {
    let opened: Arc<Mutex<Option<u64>>> = Arc::default();
    let sink = opened.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::StorageOpened(data) = event {
            *sink.lock().unwrap() = Some(data.handle_id);
        }
    }));
    let status = open(
        LoreGlobalArgs::default(),
        LoreStorageOpenArgs {
            in_memory: 1,
            ..Default::default()
        },
        callback,
    )
    .await;
    assert_eq!(status, 0, "opening an in-memory store must succeed");
    let reported = *opened.lock().unwrap();
    LoreStore {
        handle_id: reported.expect("open must report a handle"),
    }
}

/// Handles a caller never closed are closed for it, on both surfaces, and what a tree wrote after
/// its storage handle closed is flushed before shutdown returns: the write no longer keeps the
/// store's lock, and a store opened on the directory afterwards finds it.
///
/// Shutdown runs on a thread outside any runtime, as a call of `lore_shutdown` from C does. The
/// order the two surfaces drain in is not asserted: once shutdown returns both registries are
/// empty and it is no longer recoverable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_outstanding_handles_and_flushes_what_they_wrote() {
    let store = open_in_memory().await;
    let tree = load_empty_for_tests(store, Partition::from([0x18u8; 16])).await;

    let dir = TempDir::new("shutdown-tree-flush-");
    let repository = Partition::from([0x19u8; 16]);
    let key = Hash::from([0x1Du8; 32]);
    let disk = disk_backed_for_tests("shutdown-tree-flush", dir.path()).await;
    let (immutable, mutable) = (disk.immutable.clone(), disk.mutable.clone());
    let closed = lore::storage::handle::register(disk);
    let written = load_empty_for_tests(closed, repository).await;
    let status = close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: closed,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(
        status, 0,
        "closing the disk-backed storage handle must succeed"
    );
    mutable
        .clone()
        .store(
            repository,
            key,
            Hash::from([0xD1u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .expect("the tree's value is stored");

    assert!(lore::revision_tree::handle::is_registered_for_test(tree));
    assert!(lore::storage::handle::immutable_for_test(store).is_some());

    let shut_down = std::thread::spawn(lore::shutdown)
        .join()
        .expect("shutdown returns");
    assert!(shut_down, "this call shuts the library down");

    assert!(
        !lore::revision_tree::handle::is_registered_for_test(tree),
        "shutdown must close the revision tree handle the caller left open",
    );
    assert!(
        lore::storage::handle::immutable_for_test(store).is_none(),
        "shutdown must close the storage handle too",
    );
    assert!(
        !lore::revision_tree::handle::is_registered_for_test(written),
        "shutdown must close the tree whose storage handle had closed",
    );
    assert!(
        !StoreLock::new(dir.path().join("mutable"), false)
            .expect("a store directory")
            .is_held(),
        "shutdown waits for the flush of what that tree wrote, which lets the store go",
    );

    drop((immutable, mutable));
    let reopened = disk_backed_for_tests("shutdown-tree-flush-reopened", dir.path()).await;
    reopened
        .mutable
        .clone()
        .load(repository, key, KeyType::BranchMetadata)
        .await
        .expect("the value the tree wrote is on disk");
}
