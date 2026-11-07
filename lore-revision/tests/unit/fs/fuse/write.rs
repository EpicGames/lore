// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures plant host files under `write/` directly, as a mount would have left them.
#![allow(clippy::disallowed_methods)]

use std::fs::File;
use std::sync::Arc;

use futures::FutureExt;
use lore_base::test_util::TempDir;
use lore_fuse::reply::Errno;
use lore_revision::fs::fuse::write::Arrival;
use lore_revision::fs::fuse::write::HostPath;
use lore_revision::fs::fuse::write::LocalFiles;
use lore_revision::fs::fuse::write::PlaceClaims;
use lore_revision::fs::fuse::write::Settling;
use lore_revision::fs::fuse::write::with_ancestors;
use lore_revision::node::NodeID;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

/// A wait dropped before a release counts its waiter down, so releases stop telling.
#[test]
fn a_cancelled_wait_leaves_no_waiter_counted() {
    let claims = PlaceClaims::new();
    let name: (NodeID, u64) = (7, hash_string("k"));
    let held = claims.claim(name);
    let arrival = claims.arrival(name).expect("the name is claimed");
    let waker = futures::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    let mut wait = Box::pin(claims.released(arrival));

    assert!(wait.as_mut().poll(&mut context).is_pending());
    assert_eq!(claims.waiters(), 1);
    drop(wait);
    assert_eq!(claims.waiters(), 0);
    drop(held);
}

/// A wait on a name's claim ends only on the release of the claim it found: another name's
/// claim released from a slot the name then retakes, once `arrival` has read that slot,
/// leaves the wait pending, and so does the retaken claim's release, until the found claim's.
#[test]
fn a_release_of_another_names_claim_from_a_retaken_slot_does_not_end_the_wait() {
    let claims = PlaceClaims::new();
    let name: (NodeID, u64) = (7, hash_string("k"));
    let other: (NodeID, u64) = (7, hash_string("other"));
    let mut first_other = Some(claims.claim(other));
    let held = claims.claim(name);
    let mut retaken = None;
    let arrival = claims
        .arrival_scanning(name, |index| {
            if index == 0
                && let Some(released) = first_other.take()
            {
                drop(released);
                retaken = Some(claims.claim(name));
            }
        })
        .expect("the name is claimed");

    assert!(
        claims.released(arrival).now_or_never().is_none(),
        "{arrival:?} ended on the other name's release"
    );
    drop(retaken);
    assert!(
        claims.released(arrival).now_or_never().is_none(),
        "{arrival:?} ended on the retaken claim's release"
    );
    drop(held);
    assert!(
        claims.released(arrival).now_or_never().is_some(),
        "{arrival:?} still waits after the found claim's release"
    );
}

/// A wait ends on the release of the claim it found even where the same name takes the same
/// slot again at once: the word's generation tells the new claim from the found one.
#[test]
fn a_wait_ends_on_its_claims_release_though_the_name_retakes_the_slot() {
    let claims = PlaceClaims::new();
    let name: (NodeID, u64) = (7, hash_string("k"));
    let found = claims.claim(name);
    let arrival = claims.arrival(name).expect("the name is claimed");
    drop(found);
    let again = claims.claim(name);
    assert!(
        matches!((arrival, again.slot), (Arrival::Slot(was, _), Some(now)) if was == now),
        "the same slot: {arrival:?}"
    );

    assert!(
        claims.released(arrival).now_or_never().is_some(),
        "{arrival:?} still waits after the found claim's release"
    );
    drop(again);
}

/// A host file taken aside and dropped before it is settled, as by a panic, goes back.
#[test]
fn a_host_file_taken_aside_and_dropped_unsettled_goes_back() {
    let dir = TempDir::new("lore-fuse-taken-aside-");
    let write = dir.path().join("write");
    let local = LocalFiles::open(&write).expect("open");
    std::fs::create_dir(write.join("d")).expect("mkdir");
    std::fs::write(write.join("d/f.txt"), b"local").expect("write");
    let path = RelativePath::new_from_initial_path("d/f.txt").expect("path");

    let aside = local.take_aside(&path).expect("taken aside");
    assert!(!write.join("d/f.txt").exists(), "the host file is aside");
    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _aside = aside;
        panic!("a settle that fails part-way");
    }));
    assert!(dropped.is_err());
    assert_eq!(
        std::fs::read(write.join("d/f.txt")).expect("the host file is back"),
        b"local"
    );
}

/// A settle dropped part-way, as by a panic, releases its claim on the name and wakes the
/// requests waiting on it.
#[test]
fn a_settle_dropped_by_a_panic_releases_its_name_and_wakes_its_waiters() {
    let dir = TempDir::new("lore-fuse-settling-");
    let local = LocalFiles::open(&dir.path().join("write")).expect("open");
    let place: (NodeID, u64) = (7, hash_string("f.txt"));
    let waker = futures::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    let mut woken = Box::pin(local.settled.notified());
    woken.as_mut().enable();
    let settling = Settling {
        claim: Some(local.settling.claim(place)),
        settled: &local.settled,
    };
    assert!(local.settling.holds(place));

    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _settling = settling;
        panic!("a flip that fails part-way");
    }));
    assert!(dropped.is_err());
    assert!(!local.settling.holds(place), "the claim is released");
    assert!(
        woken.as_mut().poll(&mut context).is_ready(),
        "the waiting request is woken"
    );
}

/// A copy-up gate costs its own allocation and nothing more, whichever shard of the gates
/// its inode lands in, so a RENAME over a file stays within its budget.
#[test]
fn a_copy_up_gate_allocates_only_itself() {
    let dir = TempDir::new("lore-fuse-copy-up-gates-");
    let local = LocalFiles::open(&dir.path().join("write")).expect("open");
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    lore_base::allocator::set_allocation_counting(true);
    let _restore = Restore;
    for ino in 2..258 {
        let before = lore_base::allocator::thread_allocation_count();
        let gate = local.copy_up_gate(ino);
        let allocated = lore_base::allocator::thread_allocation_count() - before;
        local.release_copy_up_gate(ino);
        drop(gate);
        assert!(
            allocated <= 1,
            "inode {ino}'s gate made {allocated} allocations"
        );
    }
}

#[test]
fn caching_a_file_keeps_a_pinned_one() {
    let dir = TempDir::new("lore-fuse-local-files-");
    let local = LocalFiles::open(&dir.path().join("write")).expect("open");
    let open = |name: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, name).expect("write");
        Arc::new(File::open(&path).expect("open"))
    };
    let pinned = open("pinned");
    local.pin_file(7, pinned.clone());
    local.cache_file(7, open("other"));
    assert!(
        Arc::ptr_eq(&local.file(7).expect("cached"), &pinned),
        "the pinned file stays"
    );
    local.unpin(7);
    let reopened = open("reopened");
    local.cache_file(7, reopened.clone());
    assert!(Arc::ptr_eq(&local.file(7).expect("cached"), &reopened));
}

#[test]
fn host_path_builds_back_to_front_and_names_write_dot() {
    let mut path = HostPath::new();
    assert!(path.is_root());
    assert_eq!(path.as_cstr(), Ok(c"."));
    assert_eq!(path.as_str(), "");
    path.prepend(b"obj.o").expect("fits");
    path.prepend(b"Intermediate").expect("fits");
    assert_eq!(path.as_str(), "Intermediate/obj.o");
    assert_eq!(path.as_cstr(), Ok(c"Intermediate/obj.o"));

    let mut long = HostPath::new();
    let name = [b'a'; 255];
    let mut result = Ok(());
    for _ in 0..20 {
        result = long.prepend(&name);
        if result.is_err() {
            break;
        }
    }
    assert_eq!(result, Err(Errno::ENAMETOOLONG));
}

/// The settle's paths: each directory handed and each of its ancestors up to the root, once,
/// deepest first, with its depth, whatever the order and repeats handed in. Names that sort
/// between a directory and the paths below it byte for byte (`b-x` between `b` and `b/c`) take
/// no ancestor twice.
#[test]
fn with_ancestors_takes_each_ancestor_once_deepest_first() {
    use std::collections::BTreeSet;

    let cases: &[&[&str]] = &[
        &[],
        &[""],
        &["a"],
        &["a/b/c", "a/b", "a"],
        &["a/b-x/y", "a/b/c", "a/b", "a/b-x", "a/b/c", "a/b0/z"],
        &[
            "files/d1",
            "files/d10",
            "files/d2",
            "files/d1",
            "files",
            "files/d10/e",
        ],
        &["z/y/x/w", "a/b", "z/y", "", "m/n-/o", "m/n/o", "m/n.o/p"],
    ];
    for dirs in cases {
        let handed: Vec<RelativePath> = dirs
            .iter()
            .map(|dir| RelativePath::new_from_initial_path(dir).expect("a path"))
            .collect();
        let mut wanted = BTreeSet::new();
        for dir in *dirs {
            let mut path = *dir;
            loop {
                wanted.insert(path.to_string());
                match path.rsplit_once('/') {
                    Some((parent, _)) => path = parent,
                    None if !path.is_empty() => path = "",
                    None => break,
                }
            }
        }
        let depth = |path: &str| path.split('/').filter(|name| !name.is_empty()).count();
        let mut wanted: Vec<(usize, String)> = wanted
            .into_iter()
            .map(|path| (depth(&path), path))
            .collect();
        wanted.sort_by(|(a_depth, a), (b_depth, b)| b_depth.cmp(a_depth).then_with(|| a.cmp(b)));
        let got: Vec<(usize, String)> = with_ancestors(handed)
            .into_iter()
            .map(|(depth, path)| (depth, path.as_str().to_string()))
            .collect();
        assert_eq!(got, wanted, "handed {dirs:?}");
    }
}
