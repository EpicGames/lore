// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use lore_base::test_util::TempDir;
use rbe_lore::digest;
use rbe_proto::reapi::Command;
use rbe_worker::*;

fn scratch(name: &str) -> TempDir {
    TempDir::new(&format!("rbe-worker-test-{name}-"))
}

/// The executable bit is part of the inode, so content needed both ways cannot share one
/// staged file. If these collided, the second action to want it would silently get the first
/// one's mode.
#[test]
fn staging_separates_the_executable_variant() {
    let staging = Staging::new(PathBuf::from("/s"));
    assert_ne!(
        staging.path_for("abc", true),
        staging.path_for("abc", false)
    );
    assert_eq!(staging.path_for("abc", false), PathBuf::from("/s/abc"));
}

/// Two slots wanting the same content must produce one claim and one waiter, or they both
/// reassemble it -- the amplification this coordination exists to remove.
#[test]
fn only_one_slot_claims_a_given_key() {
    let staging = Staging::new(PathBuf::from("/s"));
    let wanted = || {
        BTreeMap::from([(
            ("abc".to_string(), 1i64, false),
            staging.path_for("abc", false),
        )])
    };
    let (first, waiting) = staging.claim(wanted());
    assert_eq!(first.mine.len(), 1);
    assert!(waiting.is_empty());

    let (second, waiting) = staging.claim(wanted());
    assert!(second.mine.is_empty(), "the second slot must not re-claim");
    assert_eq!(waiting.len(), 1);

    // Dropping the claim has to release it, or a failed stage strands every later slot.
    drop(first);
    drop(second);
    let (third, waiting) = staging.claim(wanted());
    assert_eq!(
        third.mine.len(),
        1,
        "a released key must be claimable again"
    );
    assert!(waiting.is_empty());
}

#[test]
fn temps_are_removed_when_dropped() {
    let dir = scratch("temps");
    let path = dir.join("leftover");
    std::fs::write(&path, b"partial").unwrap();
    drop(Temps(vec![path.clone()]));
    assert!(
        !path.exists(),
        "a temporary that never landed must not survive"
    );
}

#[test]
fn eviction_leaves_staged_inputs_that_are_still_linked() {
    let dir = scratch("evict");
    let idle = dir.join("idle");
    let in_use = dir.join("in-use");
    std::fs::write(&idle, vec![0u8; 4096]).unwrap();
    std::fs::write(&in_use, vec![0u8; 4096]).unwrap();
    // Standing in for a live input root holding a link to the staged file.
    let root_link = dir.join("linked-into-an-input-root");
    std::fs::hard_link(&in_use, &root_link).unwrap();

    Staging::new(dir.to_path_buf()).evict(0).unwrap();

    assert!(
        !idle.exists(),
        "an unreferenced staged input should be evicted"
    );
    assert!(
        in_use.exists(),
        "evicting a file an action has linked would pull its input out from under it"
    );
}

#[test]
fn eviction_does_nothing_under_the_cap() {
    let dir = scratch("evict-under-cap");
    let path = dir.join("kept");
    std::fs::write(&path, vec![0u8; 16]).unwrap();
    Staging::new(dir.to_path_buf()).evict(1024).unwrap();
    assert!(path.exists());
}

#[test]
fn eviction_tolerates_a_staging_directory_that_does_not_exist_yet() {
    let dir = scratch("evict-missing");
    assert!(Staging::new(dir.join("never-created")).evict(0).is_ok());
}

/// Between asking for an input and linking it, an input root's only protection is its pin:
/// the staged file has no other link yet.
#[test]
fn eviction_leaves_pinned_inputs_until_they_are_unpinned() {
    let dir = scratch("evict-pinned");
    let path = dir.join("wanted");
    std::fs::write(&path, vec![0u8; 16]).unwrap();
    let staging = Staging::new(dir.to_path_buf());

    let pins = staging.pin(vec![path.clone()]);
    staging.evict(0).unwrap();
    assert!(path.exists(), "a pinned input must survive the sweep");

    drop(pins);
    staging.evict(0).unwrap();
    assert!(!path.exists(), "an unpinned, unlinked input is fair game");
}

/// Ordered by when an input root last wanted it, not by when it was staged. The toolchain is
/// staged first and wanted by every action, so staging order would evict it first.
#[test]
fn eviction_takes_the_least_recently_wanted_first() {
    let dir = scratch("evict-lru");
    let toolchain = dir.join("toolchain");
    let one_off = dir.join("one-off");
    std::fs::write(&toolchain, vec![0u8; 16]).unwrap();
    std::fs::write(&one_off, vec![0u8; 16]).unwrap();
    let staging = Staging::new(dir.to_path_buf());

    drop(staging.pin(vec![one_off.clone()]));
    std::thread::sleep(Duration::from_millis(2));
    drop(staging.pin(vec![toolchain.clone()]));

    staging.evict(16).unwrap();
    assert!(
        toolchain.exists(),
        "the most recently wanted input was evicted"
    );
    assert!(!one_off.exists());
}

/// A temporary is a reassembly in progress, and removing it fails the action reassembling
/// it. It counts towards the cap, because it occupies the disk, but is never evicted.
#[test]
fn eviction_leaves_temporaries_alone() {
    let dir = scratch("evict-temporary");
    let temporary = dir.join(".0123.tmp");
    std::fs::write(&temporary, vec![0u8; 16]).unwrap();
    Staging::new(dir.to_path_buf()).evict(0).unwrap();
    assert!(temporary.exists());
}

/// An input missing from the CAS fails the slot that claimed it. Everything else it fetched
/// still has to land, or every slot waiting on one of those fails too.
#[tokio::test]
async fn landing_stages_what_arrived_before_reporting_what_did_not() {
    let dir = scratch("land");
    let key = |hash: &str| (hash.to_string(), 1i64, false);
    let mine = vec![
        (key("a"), dir.join("a")),
        (key("b"), dir.join("b")),
        (key("c"), dir.join("c")),
    ];
    let temps = vec![dir.join(".a.tmp"), dir.join(".b.tmp"), dir.join(".c.tmp")];
    std::fs::write(&temps[0], b"a").unwrap();
    std::fs::write(&temps[2], b"c").unwrap();

    let err = land(&mine, &temps, &[true, false, true]).await.unwrap_err();

    assert!(err.to_string().contains("not in the CAS"), "{err:#}");
    assert!(dir.join("a").exists());
    assert!(!dir.join("b").exists());
    assert!(
        dir.join("c").exists(),
        "an input after the missing one must still land: another slot may be waiting on it"
    );
}

/// A slot that gave up on a claim must not fail the slots waiting on it: they get the
/// content back to stage themselves.
#[tokio::test]
async fn a_waiter_takes_back_what_another_slot_abandoned() {
    let dir = scratch("abandon");
    let staging = Staging::new(dir.to_path_buf());
    let key = ("abc".to_string(), 1i64, false);
    let path = staging.path_for("abc", false);
    let wanted = || BTreeMap::from([(key.clone(), path.clone())]);

    let (claim, _) = staging.claim(wanted());
    let (_, waiting) = staging.claim(wanted());
    let (abandoned, ()) = tokio::join!(staging.wait_for(waiting), async {
        tokio::task::yield_now().await;
        drop(claim); // released without ever producing the file
    });

    assert_eq!(abandoned.into_keys().collect::<Vec<_>>(), vec![key]);
}

#[tokio::test]
async fn a_waiter_returns_once_the_claiming_slot_has_staged_it() {
    let dir = scratch("staged-by-another");
    let staging = Staging::new(dir.to_path_buf());
    let key = ("abc".to_string(), 1i64, false);
    let path = staging.path_for("abc", false);
    let wanted = || BTreeMap::from([(key.clone(), path.clone())]);

    let (claim, _) = staging.claim(wanted());
    let (_, waiting) = staging.claim(wanted());
    let (abandoned, ()) = tokio::join!(staging.wait_for(waiting), async {
        tokio::task::yield_now().await;
        std::fs::write(&path, b"x").unwrap();
        drop(claim);
    });

    assert!(abandoned.is_empty());
}

/// `output_paths` supersedes the split lists; the union is only for an older client.
#[test]
fn declared_outputs_prefers_output_paths() {
    #[allow(deprecated)]
    let command = Command {
        output_paths: vec!["new".into()],
        output_files: vec!["old-file".into()],
        output_directories: vec!["old-dir".into()],
        ..Default::default()
    };
    assert_eq!(declared_outputs(&command), vec!["new".to_string()]);
}

#[test]
fn declared_outputs_falls_back_to_the_union_of_the_split_lists() {
    #[allow(deprecated)]
    let command = Command {
        output_files: vec!["f".into()],
        output_directories: vec!["d".into()],
        ..Default::default()
    };
    assert_eq!(
        declared_outputs(&command),
        vec!["f".to_string(), "d".to_string()]
    );
}

/// Small streams ride inline; large ones become a blob the result points at. Getting this
/// backwards either bloats every ActionResult or costs a round trip on every action.
#[test]
fn stream_output_is_inlined_only_below_the_limit() {
    let mut collected = Collected::default();
    assert!(stage_stream_output(&mut collected, &vec![b'x'; INLINE_OUTPUT_LIMIT]).is_none());
    assert!(collected.messages.is_empty());

    let big = vec![b'x'; INLINE_OUTPUT_LIMIT + 1];
    let staged = stage_stream_output(&mut collected, &big).expect("must be staged as a blob");
    assert_eq!(staged, digest::of(&big));
    assert_eq!(collected.messages.len(), 1);
}
