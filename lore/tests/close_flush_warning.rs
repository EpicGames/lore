// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A failed flush that no close waits for is logged as a warning.
//!
//! In its own binary: it installs the process-wide log callback.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use lore::interface::LoreGlobalArgs;
use lore::storage::close::LoreStorageCloseArgs;
use lore::storage::close::close;
use lore::storage::handle;
use lore::storage::store::disk_backed_for_tests;
use lore_base::log::LoreLogLevel;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Partition;
use lore_storage::Fragment;
use lore_storage::test_util::PausePoint;
use lore_storage::test_util::fail_packfile_syncs;
use lore_storage::test_util::pause_at;

static WARNED: AtomicBool = AtomicBool::new(false);

/// Notes a warning from the module that flushes a closed handle's stores.
fn capture(level: LoreLogLevel, location: &str, _message: &str) {
    if level == LoreLogLevel::Warn && location == "lore::storage::close" {
        WARNED.store(true, Ordering::Release);
    }
}

/// **A close dropped during a flush that then fails leaves the failure logged.** The flush runs
/// to its end in a task of its own, and with no close left to return its error, logs it.
#[tokio::test]
async fn a_failed_flush_no_close_waits_for_is_logged_as_a_warning() {
    lore_base::log::set_log_level(LoreLogLevel::Warn);
    lore_base::log::set_log_callback(Some(capture));

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

    let deadline = Instant::now() + Duration::from_secs(5);
    while !WARNED.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "the failed flush is logged as a warning"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    lore_base::log::set_log_callback(None);
}
