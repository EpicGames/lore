// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore::shutdown` releases the handles a caller left open.
//!
//! One test in its own binary: shutdown drains the process-global registries and tears the
//! runtimes down for good, so it can share a process with nothing.

use std::sync::Arc;
use std::sync::Mutex;

use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::storage::handle::LoreStore;
use lore::storage::open::LoreStorageOpenArgs;
use lore::storage::open::open;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;

/// Capture the one handle id an open or a load reports.
fn handle_sink() -> (Arc<Mutex<Option<u64>>>, LoreEventCallback) {
    let handle_id: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let sink = handle_id.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
        LoreEvent::StorageOpened(data) => *sink.lock().unwrap() = Some(data.handle_id),
        LoreEvent::RevisionTreeLoaded(data) => *sink.lock().unwrap() = Some(data.handle_id),
        _ => {}
    }));
    (handle_id, callback)
}

/// Handles a caller never closed are closed for it, on both surfaces.
///
/// The order the two drain in is not asserted: once shutdown returns both registries are
/// empty and it is no longer recoverable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_outstanding_handles() {
    let (opened, callback) = handle_sink();
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
    let store = LoreStore {
        handle_id: opened.lock().unwrap().expect("open must report a handle"),
    };

    let (loaded, callback) = handle_sink();
    let status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store,
            repository: Partition::from([0x18u8; 16]),
            revision_hash: Hash::default(),
        },
        callback,
    )
    .await;
    assert_eq!(status, 0, "loading a revision tree must succeed");
    let tree = LoreRevisionTree {
        handle_id: loaded.lock().unwrap().expect("load must report a handle"),
    };

    assert!(lore::revision_tree::handle::is_registered_for_test(tree));
    assert!(lore::storage::handle::immutable_for_test(store).is_some());

    lore::shutdown();

    assert!(
        !lore::revision_tree::handle::is_registered_for_test(tree),
        "shutdown must close the revision tree handle the caller left open",
    );
    assert!(
        lore::storage::handle::immutable_for_test(store).is_none(),
        "shutdown must close the storage handle too",
    );
}
