// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! This process's access to the external root: one entry per global data directory, shared or
//! exclusive, over the OS lock on `external.lock`. A lock another holder takes through an
//! `FSLock` of its own contends as another process's would.
// Fixtures change a global data directory of their own, outside any repository.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;

use lore_base::fs::lock::FSLock;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::access::release_exclusive;
use lore_revision::global::external_dir::access::require_shared;
use lore_revision::global::external_dir::access::resolve_shared;
use lore_revision::global::external_dir::access::try_exclusive;
use lore_revision::global::external_dir::resolve::test_util::reads_in;

use crate::helper::execution_in;

/// The path whose lock file, `external.lock`, guards the external root of `global`.
fn guarded(global: &Path) -> PathBuf {
    global.join("external")
}

/// Whether another holder could take the lock of `global`'s external root in each mode, as
/// (shared, exclusive).
fn others_could_take(global: &Path) -> (bool, bool) {
    let shared = FSLock::try_acquire_shared_file_lock(guarded(global))
        .expect("attempt")
        .is_some();
    let exclusive = FSLock::try_acquire_file_lock(guarded(global))
        .expect("attempt")
        .is_some();
    (shared, exclusive)
}

/// A call takes shared access before it resolves the root: while another holder has exclusive
/// access, it fails as internal without reading any of the root's files, and once that holder
/// goes, it holds shared access and resolves.
#[tokio::test]
async fn shared_access_is_taken_before_the_root_is_resolved() {
    let global = TempDir::new("lore-external-access-before-");
    let data = global.path().join("data");
    std::fs::create_dir(&data).expect("make the global data directory");
    let holder = FSLock::try_acquire_file_lock(guarded(&data))
        .expect("attempt")
        .expect("the lock is free");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let before = reads_in(&data);
            let refused = resolve_shared().expect_err("a move holds exclusive access");
            assert!(refused.is_internal());
            assert_eq!(reads_in(&data), before, "nothing is resolved");

            drop(holder);
            resolve_shared().expect("shared access is taken");
            assert_eq!(reads_in(&data).setting, before.setting + 1);
        })
        .await;
    assert_eq!(
        others_could_take(&data),
        (true, false),
        "this process holds shared access"
    );
}

/// A refused shared try fails as internal and records nothing, so the next call tries again.
#[test]
fn a_refused_shared_try_fails_as_internal() {
    let global = TempDir::new("lore-external-access-refused-");
    let holder = FSLock::try_acquire_file_lock(guarded(global.path()))
        .expect("attempt")
        .expect("the lock is free");

    let refused = require_shared(global.path()).expect_err("another holder is exclusive");
    assert!(refused.is_internal());

    drop(holder);
    require_shared(global.path()).expect("the lock is free again");
}

/// While the entry holds exclusive access for a pending recovery, a call is refused by the entry
/// alone: with the lock file gone, a call that tried the OS lock would take a new one.
#[test]
fn a_pending_recovery_refuses_shared_access_without_touching_the_lock() {
    let global = TempDir::new("lore-external-access-pending-");
    assert!(try_exclusive(global.path()).expect("attempt"));
    std::fs::remove_file(global.path().join("external.lock")).expect("remove the lock file");

    assert!(
        require_shared(global.path())
            .expect_err("recovery is pending")
            .is_internal()
    );
    assert!(
        !global.path().join("external.lock").exists(),
        "no lock file is opened"
    );
}

/// Two spellings of one global data directory share one entry.
#[test]
fn two_spellings_of_a_global_data_directory_share_an_entry() {
    let global = TempDir::new("lore-external-access-spellings-");
    let other = global.path().join("sub");
    std::fs::create_dir(&other).expect("make a subdirectory");
    let spelled = other.join("..");
    assert!(try_exclusive(global.path()).expect("attempt"));
    std::fs::remove_file(global.path().join("external.lock")).expect("remove the lock file");

    assert!(require_shared(&spelled).is_err(), "the entry is exclusive");
    assert!(
        try_exclusive(&spelled).expect("attempt"),
        "and already held"
    );
}

/// Exclusive access released goes back to shared access, which excludes another exclusive holder
/// and admits another shared one.
#[test]
fn exclusive_access_is_released_into_shared_access() {
    let global = TempDir::new("lore-external-access-release-");
    assert!(try_exclusive(global.path()).expect("attempt"));
    assert_eq!(others_could_take(global.path()), (false, false));

    release_exclusive(global.path()).expect("shared access is taken");
    assert_eq!(others_could_take(global.path()), (true, false));
    require_shared(global.path()).expect("calls are served");
}

/// This process's own shared access never keeps its recovery from exclusive access, while another
/// holder's shared access does, and leaves this process with none.
#[test]
fn only_another_holders_shared_access_keeps_recovery_waiting() {
    let global = TempDir::new("lore-external-access-own-");
    require_shared(global.path()).expect("shared access is taken");
    assert!(try_exclusive(global.path()).expect("attempt"));
    release_exclusive(global.path()).expect("shared access is taken again");

    let other = FSLock::try_acquire_shared_file_lock(guarded(global.path()))
        .expect("attempt")
        .expect("shared beside shared");
    assert!(!try_exclusive(global.path()).expect("attempt"));
    drop(other);
    assert_eq!(
        others_could_take(global.path()),
        (true, true),
        "this process holds no access"
    );
}
