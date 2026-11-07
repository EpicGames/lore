// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_fuse::reply::Timeouts;
use lore_revision::fs::fuse::inode::Name;
use lore_revision::fs::fuse::spelling::DEFAULT_TIMEOUTS;
use lore_revision::fs::fuse::spelling::NEGATIVE_TIMEOUTS;
use lore_revision::fs::fuse::spelling::SPELLING_BOUND;
use lore_revision::fs::fuse::spelling::SpellingRecord;

fn timeouts_eq(a: Timeouts, b: Timeouts) -> bool {
    a.entry == b.entry && a.attr == b.attr
}

#[test]
fn records_each_distinct_spelling_once() {
    let record = SpellingRecord::new();
    assert!(timeouts_eq(
        record.record(1, 9, b"Foo", true),
        DEFAULT_TIMEOUTS
    ));
    record.record(1, 9, b"foo", true);
    record.record(1, 9, b"Foo", true);
    record.record(1, 9, b"foo", true);
    let spellings = record.take(1, 9).expect("recorded");
    let seen: Vec<&[u8]> = spellings.seen.iter().map(|s| &**s).collect();
    assert_eq!(seen, vec![&b"Foo"[..], &b"foo"[..]]);
    assert!(!spellings.negative);
    assert!(!spellings.overflowed);
}

#[test]
fn negative_lookup_is_recorded() {
    let record = SpellingRecord::new();
    let timeouts = record.record(4, 2, b"missing.h", false);
    assert!(timeouts_eq(timeouts, NEGATIVE_TIMEOUTS));
    assert!(
        timeouts.entry > Duration::ZERO,
        "negatives are cached finitely"
    );
    let spellings = record.take(4, 2).expect("negative recorded");
    assert!(spellings.negative);
    assert_eq!(&*spellings.seen[0], b"missing.h");
}

#[test]
fn bound_exceeded_marks_overflowed_and_yields_zero_timeouts() {
    let record = SpellingRecord::new();
    let spellings: Vec<String> = (0..SPELLING_BOUND)
        .map(|i| {
            // Distinct case variants of one name.
            "abcdefgh"
                .chars()
                .enumerate()
                .map(|(j, c)| {
                    if (i >> j) & 1 == 1 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect()
        })
        .collect();
    for spelling in &spellings {
        assert!(timeouts_eq(
            record.record(1, 5, spelling.as_bytes(), true),
            DEFAULT_TIMEOUTS
        ));
    }
    assert!(timeouts_eq(
        record.record(1, 5, b"ABCDEFGH", true),
        Timeouts::ZERO
    ));
    // Once overflowed, even known spellings and negatives get zero.
    assert!(timeouts_eq(
        record.record(1, 5, spellings[0].as_bytes(), true),
        Timeouts::ZERO
    ));
    assert!(timeouts_eq(
        record.record(1, 5, b"AbCdEfGh", false),
        Timeouts::ZERO
    ));
    let taken = record.take(1, 5).expect("recorded");
    assert!(taken.overflowed);
    assert_eq!(taken.seen.len(), SPELLING_BOUND);
}

#[test]
fn take_returns_spellings_and_clears() {
    let record = SpellingRecord::new();
    record.record(1, 5, b"a", true);
    record.record(1, 6, b"b", false);
    record.record(2, 5, b"c", true);
    assert!(record.take(1, 5).is_some());
    assert!(record.take(1, 5).is_none(), "take clears the key");

    let mut out = Vec::new();
    record.take_children_of(1, &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, 6);
    assert!(out[0].1.negative);
    out.clear();
    record.take_children_of(1, &mut out);
    assert!(out.is_empty());
    assert!(record.take(2, 5).is_some(), "other parents are untouched");
}

#[test]
fn copy_children_of_leaves_the_record_intact() {
    let record = SpellingRecord::new();
    record.record(1, 5, b"Dir", true);
    record.record(1, 5, b"dir", true);
    record.record(1, 6, b"file", true);
    record.record(2, 7, b"inner", true);

    let mut out = Vec::new();
    record.copy_children_of(1, &mut out);
    out.sort();
    assert_eq!(out, [&b"Dir"[..], b"dir", b"file"].map(Name::from_slice));
    assert_eq!(record.take(1, 5).map(|s| s.seen.len()), Some(2));
    assert!(record.take(2, 7).is_some());
}

#[test]
fn recording_known_spelling_allocates_nothing() {
    let record = SpellingRecord::new();
    record.record(1, 5, b"Known", true);
    record.record(1, 5, b"missing", false);

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    lore_base::allocator::set_allocation_counting(true);
    let _restore = Restore;

    let before = lore_base::allocator::thread_allocation_count();
    let positive = std::hint::black_box(record.record(1, 5, b"Known", true));
    let negative = std::hint::black_box(record.record(1, 5, b"missing", false));
    let after = lore_base::allocator::thread_allocation_count();

    assert!(timeouts_eq(positive, DEFAULT_TIMEOUTS));
    assert!(timeouts_eq(negative, NEGATIVE_TIMEOUTS));
    assert_eq!(after, before, "a known spelling must not allocate");
}

/// Every shard passes through a full table on the way, where an entry-API call reserves room
/// and grows the table even for a key it then finds.
#[test]
fn recording_a_known_spelling_allocates_nothing_in_a_full_shard() {
    let record = SpellingRecord::new();

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    lore_base::allocator::set_allocation_counting(true);
    let _restore = Restore;

    for name_hash in 0..4096 {
        record.record(1, name_hash, b"Known", true);
        let before = lore_base::allocator::thread_allocation_count();
        let timeouts = std::hint::black_box(record.record(1, name_hash, b"Known", true));
        let after = lore_base::allocator::thread_allocation_count();
        assert!(timeouts_eq(timeouts, DEFAULT_TIMEOUTS));
        assert_eq!(
            after, before,
            "key {name_hash}: a known spelling must not allocate"
        );
    }
}
