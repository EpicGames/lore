// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Partition;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_storage::ContentSource;
use lore_storage::FRAGMENT_SIZE_THRESHOLD;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::node;
use crate::fs::filesystem_provider::test_store_create;

/// The bytes of the content the snapshot gives for `path`, or `None` where it gives none.
async fn content_of(snapshot: &FrozenSnapshot, path: &str) -> Option<Vec<u8>> {
    let source = snapshot
        .content(&node(path))
        .expect("reading the snapshot")?;
    Some(
        source
            .read_all()
            .await
            .expect("reading the content")
            .to_vec(),
    )
}

#[test]
fn a_listed_file_is_found_by_its_node_path() {
    let fake = Arc::new(FakeFrozen::default().with_file(r"\sub\n.txt", b"nested"));
    let snapshot = FrozenSnapshot::capture(fake).unwrap();

    let found = snapshot.get(&node("sub/n.txt")).expect("listed");

    assert_eq!(found.size, 6);
    assert!(!found.is_directory);
    assert!(snapshot.get(&node("sub/other.txt")).is_none());
}

#[test]
fn lookups_fold_case_as_the_files_do() {
    let fake = Arc::new(FakeFrozen::default().with_file(r"\Dir\File.TXT", b"x"));
    let snapshot = FrozenSnapshot::capture(fake).unwrap();

    assert!(snapshot.get(&node("dir/file.txt")).is_some());
    assert!(snapshot.get(&node("DIR/FILE.txt")).is_some());
}

#[test]
fn a_listed_directory_is_found_and_is_not_content() {
    let fake = Arc::new(FakeFrozen::default().with_directory(r"\sub"));
    let snapshot = FrozenSnapshot::capture(fake.clone()).unwrap();

    assert!(snapshot.get(&node("sub")).expect("listed").is_directory);
    assert!(snapshot.content(&node("sub")).unwrap().is_none());
    assert_eq!(fake.reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_read_asks_for_the_swfs_form_of_the_path_and_returns_the_bytes() {
    let fake = Arc::new(FakeFrozen::default().with_file(r"\sub\n.txt", b"nested new"));
    let snapshot = FrozenSnapshot::capture(fake.clone()).unwrap();

    let content = content_of(&snapshot, "sub/n.txt").await;

    assert_eq!(content, Some(b"nested new".to_vec()));
    let paths = fake.read_paths.lock();
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|path| path == r"\sub\n.txt"), "{paths:?}");
}

#[test]
fn a_path_not_listed_has_no_content_and_costs_no_read() {
    let fake = Arc::new(FakeFrozen::default().with_file(r"\b.txt", b"b"));
    let snapshot = FrozenSnapshot::capture(fake.clone()).unwrap();

    assert!(snapshot.content(&node("a.txt")).unwrap().is_none());
    assert_eq!(fake.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn a_listed_file_swfs_has_no_content_for_reads_as_none() {
    let fake = Arc::new(FakeFrozen::default().with_unreadable_file(r"\dir\c2.txt", 4));
    let snapshot = FrozenSnapshot::capture(fake).unwrap();

    assert!(snapshot.get(&node("dir/c2.txt")).is_some());
    assert!(snapshot.content(&node("dir/c2.txt")).unwrap().is_none());
}

#[tokio::test]
async fn an_empty_file_reads_as_empty_without_asking_swfs() {
    let fake = Arc::new(FakeFrozen::default().with_file(r"\empty.txt", b""));
    let snapshot = FrozenSnapshot::capture(fake.clone()).unwrap();

    assert_eq!(content_of(&snapshot, "empty.txt").await, Some(Vec::new()));
    assert_eq!(fake.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn a_failed_listing_fails_the_capture() {
    let fake = Arc::new(FakeFrozen {
        fail_list: true,
        ..Default::default()
    });

    assert!(FrozenSnapshot::capture(fake).is_err());
}

#[test]
fn a_failed_read_is_an_error_rather_than_missing_content() {
    let fake = Arc::new(FakeFrozen {
        fail_read: true,
        ..FakeFrozen::default().with_file(r"\b.txt", b"b")
    });
    let snapshot = FrozenSnapshot::capture(fake).unwrap();

    assert!(snapshot.content(&node("b.txt")).is_err());
}

#[test]
fn the_empty_snapshot_lists_nothing() {
    let snapshot = FrozenSnapshot::empty();

    assert!(snapshot.get(&node("b.txt")).is_none());
    assert!(snapshot.content(&node("b.txt")).unwrap().is_none());
}

/// A frozen file of any size is read from SWFS a window at a time, so hashing one many times the
/// window asks SWFS for no more than a window at once, and hashes as the same bytes held whole.
#[tokio::test]
async fn a_large_frozen_file_is_read_from_swfs_a_window_at_a_time() {
    let window = 2 * FRAGMENT_SIZE_THRESHOLD;
    let large: Vec<u8> = (0..8 * window).map(|index| (index % 251) as u8).collect();
    let fake = Arc::new(FakeFrozen::default().with_file(r"\big.bin", &large));
    let snapshot = FrozenSnapshot::capture(fake.clone()).unwrap();
    let (immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let partition = Partition::from([0x5au8; 16]);

    let (frozen, whole) = LORE_CONTEXT
        .scope(execution, async move {
            let source = snapshot
                .content(&node("big.bin"))
                .unwrap()
                .expect("SWFS holds the file's bytes");
            let frozen =
                lore_storage::write::hash_file(immutable_store.clone(), partition, &source, None)
                    .await
                    .expect("hashing the frozen file");
            let whole = lore_storage::write::hash_file(
                immutable_store,
                partition,
                &ContentSource::memory(large.into()),
                None,
            )
            .await
            .expect("hashing the bytes held whole");
            (frozen, whole)
        })
        .await;

    assert_eq!(frozen, whole);
    let largest = fake.largest_read.load(Ordering::Relaxed);
    assert!(
        largest <= window,
        "a read asked SWFS for {largest} bytes, past the window of {window}"
    );
}

/// SWFS lists the root among the frozen files. It is no child of itself: a listing of the root
/// showed it as an entry with no name, which a reset's walk then removed as a file at the root.
#[test]
fn the_root_swfs_lists_is_not_a_child_of_itself() {
    let fake = Arc::new(
        FakeFrozen::default()
            .with_directory(r"\")
            .with_file(r"\a.txt", b"a"),
    );
    let snapshot = FrozenSnapshot::capture(fake).unwrap();

    let names: Vec<&str> = snapshot
        .children_of(&node(""))
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(names, vec![r"\a.txt"]);
}

#[test]
fn a_file_below_a_moved_directory_is_found_at_the_same_place_below_the_origin() {
    let fake = Arc::new(
        FakeFrozen::default()
            .with_directory(r"\moved")
            .with_move(r"\old", r"\moved"),
    );
    let snapshot = FrozenSnapshot::capture(fake).unwrap();

    assert_eq!(
        snapshot.moved_from(&node("moved/sub/n.txt")),
        Some(node("old/sub/n.txt"))
    );
    assert_eq!(snapshot.moved_from(&node("elsewhere/n.txt")), None);
    // The directory itself is not below its own move.
    assert_eq!(snapshot.moved_from(&node("moved")), None);
}

/// The modified list names a move's destination as renamed, and its origin, which it lists as
/// deleted, not.
#[test]
fn a_move_names_its_destination_and_not_its_origin_as_renamed() {
    let snapshot = FrozenSnapshot::capture(Arc::new(
        FakeFrozen::default().with_move("\\a.txt", "\\b.txt"),
    ))
    .expect("listing the frozen files");

    assert!(snapshot.lists_renamed("B.txt"));
    assert!(!snapshot.lists_renamed("a.txt"));
    assert!(snapshot.renamed_at_or_above(&node("b.txt")));
}
