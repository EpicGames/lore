// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use std::time::Instant;

use lore_base::types::Address;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Hash;
use lore_revision::fs::fuse::inode::InodeEntry;
use lore_revision::fs::fuse::inode::InodeKind;
use lore_revision::fs::fuse::read::FAIL_FAST_WINDOW;
use lore_revision::fs::fuse::read::READAHEAD_WHOLE_FILE_MAX;
use lore_revision::fs::fuse::read::RECENT_MISSES_MAX;
use lore_revision::fs::fuse::read::RecentMisses;
use lore_revision::fs::fuse::read::note_read;
use lore_revision::fs::fuse::read::readahead_wanted;

fn file_entry(size: u64) -> InodeEntry {
    InodeEntry {
        ino: 7,
        parent: 1,
        name_hash: 0,
        name: lore_revision::fs::fuse::inode::Name::from_slice(b"f"),
        kind: InodeKind::File {
            address: Address::default(),
            size,
            executable: false,
            local: false,
        },
        nlookup: 1,
        mtime: 0,
        orphaned: false,
        read_cursor: AtomicU64::new(0),
        readahead_started: AtomicBool::new(false),
        access_recorded: AtomicBool::new(false),
        write_generation: AtomicU64::new(0),
        incarnation: 0,
        host_epoch: 0,
        renames: AtomicU32::new(0),
        rename_changes: AtomicU64::new(0),
        host_file: 0,
        node: 0,
    }
}

fn miss(marker: u64) -> Address {
    Address {
        hash: Hash::from_u64(marker),
        ..Address::default()
    }
}

fn expired() -> Instant {
    Instant::now()
        .checked_sub(FAIL_FAST_WINDOW + Duration::from_millis(1))
        .expect("the clock reaches back past the window")
}

#[test]
fn a_miss_fails_fast_only_within_the_window() {
    let misses = RecentMisses::default();
    misses.record(&miss(1));
    assert!(misses.missed_recently(&miss(1)));
    assert!(!misses.missed_recently(&miss(2)));

    misses.at.insert(miss(1).hash, expired());
    assert!(!misses.missed_recently(&miss(1)));
    assert!(
        misses.at.is_empty(),
        "an expired miss is forgotten when asked about"
    );
}

#[test]
fn a_full_map_forgets_expired_misses_and_otherwise_remembers_no_more() {
    let misses = RecentMisses::default();
    for marker in 0..RECENT_MISSES_MAX as u64 {
        misses.at.insert(miss(marker).hash, expired());
    }
    misses.record(&miss(u64::MAX));
    assert_eq!(misses.at.len(), 1, "expired misses make room");
    assert!(misses.missed_recently(&miss(u64::MAX)));

    for marker in 0..RECENT_MISSES_MAX as u64 {
        misses.record(&miss(marker));
    }
    assert_eq!(misses.at.len(), RECENT_MISSES_MAX);
    misses.record(&miss(u64::MAX - 1));
    assert_eq!(
        misses.at.len(),
        RECENT_MISSES_MAX,
        "a full map of live misses stays full"
    );
    assert!(!misses.missed_recently(&miss(u64::MAX - 1)));
}

#[test]
fn two_sequential_reads_start_readahead_once() {
    let entry = file_entry(8 << 20);

    assert!(
        !note_read(&entry, 0, 4096),
        "a first read at 0 is not yet sequential"
    );
    assert!(
        note_read(&entry, 4096, 4096),
        "the next contiguous read starts it"
    );
    assert!(!note_read(&entry, 8192, 4096), "it starts only once");
    assert!(!note_read(&entry, 12288, 4096));
}

#[test]
fn random_reads_do_not_start_readahead() {
    let entry = file_entry(8 << 20);

    for offset in [1_000_000u64, 5_000, 3_000_000, 77, 2_000_000] {
        assert!(!note_read(&entry, offset, 512));
    }
    assert!(
        !entry
            .readahead_started
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[test]
fn large_file_does_not_start_whole_file_readahead() {
    assert!(readahead_wanted(READAHEAD_WHOLE_FILE_MAX));
    assert!(!readahead_wanted(READAHEAD_WHOLE_FILE_MAX + 1));
    assert!(readahead_wanted(FRAGMENT_SIZE_THRESHOLD as u64 + 1));
}

#[test]
fn single_fragment_file_does_not_start_readahead() {
    // At most one fragment: a local read already proved all of it local.
    assert!(!readahead_wanted(FRAGMENT_SIZE_THRESHOLD as u64));
    assert!(!readahead_wanted(10));
}
