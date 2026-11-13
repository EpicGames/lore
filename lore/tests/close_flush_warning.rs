// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A failed flush that no caller waits for is logged as a warning.
//!
//! In its own binary: it installs the process-wide log callback. Each test reads the warnings
//! logged on its own thread, where its runtime runs the flush.

use std::cell::Cell;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle as tree_handle;
use lore::revision_tree::load::load_empty_for_tests;
use lore::storage::close::LoreStorageCloseArgs;
use lore::storage::close::close;
use lore::storage::handle;
use lore::storage::store::disk_backed_for_tests;
use lore_base::log::LoreLogLevel;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_storage::Fragment;
use lore_storage::test_util::PausePoint;
use lore_storage::test_util::fail_packfile_syncs;
use lore_storage::test_util::pause_at;

thread_local! {
    static WARNED: Cell<bool> = const { Cell::new(false) };
}

/// Notes a warning from the module that flushes a closed handle's stores.
fn capture(level: LoreLogLevel, location: &str, _message: &str) {
    if level == LoreLogLevel::Warn && location == "lore::storage::close" {
        WARNED.set(true);
    }
}

/// Installs [`capture`] and clears this thread's note of a warning.
fn capture_warnings() {
    lore_base::log::set_log_level(LoreLogLevel::Warn);
    lore_base::log::set_log_callback(Some(capture));
    WARNED.set(false);
}

/// Waits for a warning on this thread, failing with `what` after five seconds.
async fn wait_for_warning(what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !WARNED.get() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// **A close dropped during a flush that then fails leaves the failure logged.** The flush runs
/// to its end in a task of its own, and with no close left to return its error, logs it.
#[tokio::test]
async fn a_failed_flush_no_close_waits_for_is_logged_as_a_warning() {
    capture_warnings();

    let dir = TempDir::new("close-flush-warning-");
    let store = disk_backed_for_tests("close-flush-warning", dir.path()).await;
    let payload = b"a payload whose sync fails";
    let address = Address {
        hash: lore_storage::hash_slice(payload),
        context: Context::default(),
    };
    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    store
        .immutable
        .clone()
        .put(
            Partition::from([0x2Fu8; 16]),
            address,
            fragment,
            Some(Bytes::from_static(payload)),
            false,
        )
        .await
        .expect("the payload is stored");
    let store_handle = handle::register(store);

    let scanning = pause_at(PausePoint::Scanning);
    fail_packfile_syncs(1);
    let close_task = lore_base::lore_spawn!(close(
        LoreGlobalArgs {
            sync_data: 1,
            ..Default::default()
        },
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

    wait_for_warning("the failed flush is logged as a warning").await;
}

/// **A drained tree whose flush fails leaves the failure logged.** Shutdown and connection
/// teardown flush the stores of a tree whose storage handle closed with no caller to return the
/// flush's error to.
#[tokio::test]
async fn a_failed_flush_of_a_drained_tree_is_logged_as_a_warning() {
    capture_warnings();

    let dir = TempDir::new("drain-flush-warning-");
    let repository = Partition::from([0x3Au8; 16]);
    let store = disk_backed_for_tests("drain-flush-warning", dir.path()).await;
    let mutable = store.mutable.clone();
    let store_handle = handle::register(store);
    let tree = load_empty_for_tests(store_handle, repository).await;
    let status = close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "closing the storage handle succeeds");
    mutable
        .store(
            repository,
            Hash::from([0x3Bu8; 32]),
            Hash::from([0xB3u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .expect("the tree's value is stored");
    block_index(&dir.path().join("mutable"));

    let internal = tree_handle::unregister(tree).expect("the tree is registered");
    lore::revision_tree::drain_in_parallel(vec![(tree.handle_id, internal)]).await;

    wait_for_warning("the drained tree's failed flush is logged as a warning").await;
}

/// Puts a file where the store directory `store` keeps its index, so a flush cannot write it.
fn block_index(store: &Path) {
    let index = store.join("index");
    if index.is_dir() {
        std::fs::remove_dir_all(&index).expect("the index directory is removed");
    }
    std::fs::write(&index, b"").expect("a file is written in the index directory's place");
}
