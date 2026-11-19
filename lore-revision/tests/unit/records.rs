// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use futures::FutureExt;
use lore_base::lore_spawn;
use lore_base::lore_spawn_blocking;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::test_util::TempDir;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::records::FORMAT_VERSION;
use lore_revision::records::InstanceRecords;
use lore_revision::records::Lookup;
use lore_revision::records::MAGIC;
use lore_revision::records::NO_STORE_RECORDS;
use lore_revision::records::RECORDS_DIR;
use lore_revision::records::Record;
use lore_revision::records::RecordBatch;
use lore_revision::records::RecordKey;
use lore_revision::records::SHARD_RECORDS_MAX;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::RecordsLocation;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::VfsConfig;
use lore_revision::repository::VfsType;
use lore_revision::repository::get_cached_in_memory_stores;
use lore_revision::repository::load_and_connect;
use lore_revision::repository::records_location;
use lore_revision::repository::repository_release;
use lore_revision::state;
use lore_revision::test_util::hold_in_memory_store_misses;
use lore_revision::util::path::RelativePath;
use lore_storage::FlushOptions;
use lore_storage::test_util::fail_packfile_syncs;
use lore_storage::test_util::packfile_sync_failures_left;

use crate::helper::committed_repository;
use crate::helper::relative_path;
use crate::helper::stage_paths;
use crate::helper::test_execution_with;
use crate::helper::test_file_write;
use crate::repository::test_helpers::store_fragment;

/// A key in shard `shard`, told apart from the shard's other keys by `fill`.
fn key(shard: u8, fill: u8) -> RecordKey {
    let mut key = [fill; 16];
    key[0] = shard;
    key
}

/// Writes `body` as shard `shard`'s file in the records of the dot directory `dot_path`.
fn write_shard(dot_path: &Path, shard: u8, body: &[u8]) {
    let directory = dot_path.join(RECORDS_DIR);
    std::fs::create_dir_all(&directory).expect("create the records directory");
    test_file_write(shard_path(dot_path, shard).as_path(), body);
}

fn shard_path(dot_path: &Path, shard: u8) -> PathBuf {
    dot_path.join(RECORDS_DIR).join(format!("{shard:02x}"))
}

/// A shard file of `version` holding `entries`, in the order given.
fn shard_file(version: u32, entries: &[(RecordKey, u64)]) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&version.to_le_bytes());
    for (key, mtime) in entries {
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&mtime.to_le_bytes());
    }
    bytes
}

async fn token(dir: &TempDir) -> RepositoryWriteToken {
    RepositoryWriteToken::acquire(dir.path()).await
}

/// Runs `test` in the execution context a command runs in.
async fn in_context(test: impl std::future::Future<Output = ()>) {
    LORE_CONTEXT
        .scope(test_execution_with(LoreGlobalArgs::default()), test)
        .await;
}

/// Writes back `records`, which must succeed.
async fn write_back(records: &Arc<InstanceRecords>, token: &RepositoryWriteToken) {
    records
        .write_back(token, false, ())
        .await
        .expect("write back the records");
}

#[tokio::test]
async fn records_written_back_are_read_by_the_next_holder() {
    in_context(async {
        let dir = TempDir::new("records-round-trip-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        records.set(key(1, 2), 200).await;
        records.set(key(1, 1), 100).await;
        records.set(key(200, 3), 300).await;
        records.remove(&key(1, 2)).await;
        write_back(&records, &token).await;
        drop(records);

        let records = InstanceRecords::open(dir.path()).await;
        assert_eq!(records.lookup(&key(1, 1)).await, Lookup::Recorded(100));
        assert!(matches!(
            records.lookup(&key(1, 2)).await,
            Lookup::Missing(_)
        ));
        assert_eq!(records.lookup(&key(200, 3)).await, Lookup::Recorded(300));
        assert_eq!(
            std::fs::read(shard_path(dir.path(), 1)).expect("read the shard"),
            shard_file(FORMAT_VERSION, &[(key(1, 1), 100)]),
            "the header, then the records sorted by key, with the removed one left out"
        );
    })
    .await;
}

/// Records tell from their directory's listing alone whether it holds a shard: the mark, a
/// temporary file and a directory that does not exist hold none.
#[tokio::test]
async fn records_tell_from_the_listing_whether_the_directory_holds_a_shard() {
    in_context(async {
        let dir = TempDir::new("records-listing-");
        let token = token(&dir).await;
        let directory = dir.path().join(RECORDS_DIR);
        let records = InstanceRecords::open(dir.path()).await;
        assert!(!records.has_shard_files().await, "no directory, no shard");
        drop(records);

        std::fs::create_dir_all(&directory).expect("create the directory");
        test_file_write(&directory.join(NO_STORE_RECORDS), b"");
        test_file_write(&directory.join("07.tmp"), b"cut short");
        let records = InstanceRecords::open(dir.path()).await;
        assert!(
            !records.has_shard_files().await,
            "a mark and a temporary file"
        );
        records.set(key(7, 1), 700).await;
        write_back(&records, &token).await;
        drop(records);
        assert!(
            InstanceRecords::open(dir.path())
                .await
                .has_shard_files()
                .await
        );
    })
    .await;
}

/// A process holds one set of records per directory, whatever path names it, until the last
/// holder lets it go, so one write-back writes both holders' times.
#[tokio::test]
async fn holders_of_one_directory_share_one_set_of_records() {
    in_context(async {
        let dir = TempDir::new("records-shared-");
        let token = token(&dir).await;
        std::fs::create_dir(dir.path().join("sub")).expect("a directory to name the path through");
        let first = InstanceRecords::open(dir.path()).await;
        let second = InstanceRecords::open(&dir.path().join("sub").join("..")).await;
        assert!(Arc::ptr_eq(&first, &second));
        first.set(key(3, 1), 300).await;
        second.set(key(3, 2), 302).await;
        write_back(&first, &token).await;
        drop((first, second));

        let reopened = InstanceRecords::open(dir.path()).await;
        assert_eq!(reopened.lookup(&key(3, 1)).await, Lookup::Recorded(300));
        assert_eq!(reopened.lookup(&key(3, 2)).await, Lookup::Recorded(302));
    })
    .await;
}

/// Records tell whether a time was set, adopted or removed since they were opened. A lookup is
/// no change, and records opened again, once let go, start with none.
#[tokio::test]
async fn records_tell_whether_they_changed_since_they_were_opened() {
    in_context(async {
        let dir = TempDir::new("records-changed-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        assert!(matches!(
            records.lookup(&key(5, 1)).await,
            Lookup::Missing(_)
        ));
        assert!(!records.has_changed());
        records.set(key(5, 1), 500).await;
        assert!(records.has_changed());
        write_back(&records, &token).await;
        assert!(records.has_changed(), "a write-back leaves the change told");
        drop(records);

        let records = InstanceRecords::open(dir.path()).await;
        assert!(!records.has_changed());
        records.remove(&key(5, 1)).await;
        assert!(records.has_changed(), "a removal is a change");
    })
    .await;
}

/// Records kept in memory hold the times set in them, read no mutable store, and mark no shard to
/// write back.
#[tokio::test]
async fn records_kept_in_memory_mark_no_shard_to_write_back() {
    in_context(async {
        let records = InstanceRecords::in_memory();
        assert!(!records.has_store_records());
        records.set(key(1, 1), 100).await;
        records.set(key(1, 2), 200).await;
        records.remove(&key(1, 2)).await;
        let mut batch = RecordBatch::with_capacity(1);
        batch.push(Record::new(key(9, 1), 900));
        records.set_all(batch).await.expect("set the batch");

        assert_eq!(records.lookup(&key(1, 1)).await, Lookup::Recorded(100));
        assert!(matches!(
            records.lookup(&key(1, 2)).await,
            Lookup::Missing(_)
        ));
        assert_eq!(records.lookup(&key(9, 1)).await, Lookup::Recorded(900));
        assert!(records.has_changed());
        assert!(!records.is_dirty(), "no shard to write back");
    })
    .await;
}

/// A time set for a key the shard holds is changed in place, and seen at once.
#[tokio::test]
async fn a_new_time_for_a_held_key_is_seen_at_once() {
    in_context(async {
        let dir = TempDir::new("records-in-place-");
        write_shard(
            dir.path(),
            3,
            &shard_file(FORMAT_VERSION, &[(key(3, 1), 100)]),
        );
        let records = InstanceRecords::open(dir.path()).await;
        records.set(key(3, 1), 200).await;
        assert_eq!(records.lookup(&key(3, 1)).await, Lookup::Recorded(200));
        records.remove(&key(3, 1)).await;
        assert!(matches!(
            records.lookup(&key(3, 1)).await,
            Lookup::Missing(_)
        ));
    })
    .await;
}

/// A shard larger than the bytes read as its file is opened is read whole.
#[tokio::test]
async fn a_large_shard_is_read_whole() {
    in_context(async {
        let dir = TempDir::new("records-large-");
        let token = token(&dir).await;
        let count = 20_000u32;
        let large = |index: u32| {
            let mut key = [0u8; 16];
            key[0] = 9;
            key[1..5].copy_from_slice(&index.to_be_bytes());
            key
        };
        let records = InstanceRecords::open(dir.path()).await;
        let mut batch = RecordBatch::with_capacity(0);
        for index in (0..count).rev() {
            batch.push(Record::new(large(index), u64::from(index) + 1));
        }
        records.set_all(batch).await.expect("set the records");
        write_back(&records, &token).await;
        drop(records);

        let records = InstanceRecords::open(dir.path()).await;
        for index in [0, count / 2, count - 1] {
            assert_eq!(
                records.lookup(&large(index)).await,
                Lookup::Recorded(u64::from(index) + 1)
            );
        }
    })
    .await;
}

/// A shard of a format version this client does not know is treated as absent, and the next
/// write-back replaces it with one this client wrote.
#[tokio::test]
async fn a_shard_of_an_unknown_version_is_absent_and_replaced() {
    in_context(async {
        let dir = TempDir::new("records-unknown-version-");
        let token = token(&dir).await;
        write_shard(
            dir.path(),
            1,
            &shard_file(FORMAT_VERSION + 1, &[(key(1, 1), 100)]),
        );

        let records = InstanceRecords::open(dir.path()).await;
        assert!(matches!(
            records.lookup(&key(1, 1)).await,
            Lookup::Missing(_)
        ));
        write_back(&records, &token).await;

        assert_eq!(
            std::fs::read(shard_path(dir.path(), 1)).expect("read the shard"),
            shard_file(FORMAT_VERSION, &[]),
            "rewritten in this version"
        );
    })
    .await;
}

/// Shards that are cut short, of another format, unsorted, holding a key twice or holding
/// another shard's key are each treated as absent.
#[tokio::test]
async fn a_shard_that_does_not_parse_is_absent() {
    in_context(async {
        let dir = TempDir::new("records-corrupt-");
        let mut partial = shard_file(FORMAT_VERSION, &[(key(1, 1), 100)]);
        partial.extend_from_slice(&[0u8; 6]);
        let mut foreign = shard_file(FORMAT_VERSION, &[(key(2, 1), 100)]);
        foreign[0] ^= 0xff;
        write_shard(dir.path(), 1, &partial);
        write_shard(dir.path(), 2, &foreign);
        write_shard(dir.path(), 3, &MAGIC[..2]);
        write_shard(
            dir.path(),
            4,
            &shard_file(FORMAT_VERSION, &[(key(4, 2), 100), (key(4, 1), 100)]),
        );
        write_shard(
            dir.path(),
            5,
            &shard_file(FORMAT_VERSION, &[(key(5, 1), 100), (key(5, 1), 200)]),
        );
        write_shard(
            dir.path(),
            6,
            &shard_file(FORMAT_VERSION, &[(key(6, 1), 100), (key(7, 1), 100)]),
        );

        let records = InstanceRecords::open(dir.path()).await;
        for shard in 1..=6 {
            assert!(
                matches!(records.lookup(&key(shard, 1)).await, Lookup::Missing(_)),
                "shard {shard}"
            );
        }
    })
    .await;
}

/// A replace sets its time only where the key still holds the time expected, none included: a
/// time set since is kept.
#[tokio::test]
async fn a_replace_keeps_a_time_set_since_the_time_expected() {
    in_context(async {
        let dir = TempDir::new("records-replace-");
        let records = InstanceRecords::open(dir.path()).await;

        records.replace(key(6, 1), 0, 100).await;
        assert!(matches!(
            records.lookup(&key(6, 1)).await,
            Lookup::Recorded(100)
        ));
        records.replace(key(6, 1), 50, 200).await;
        assert!(matches!(
            records.lookup(&key(6, 1)).await,
            Lookup::Recorded(100)
        ));
        records.replace(key(6, 1), 100, 300).await;
        assert!(matches!(
            records.lookup(&key(6, 1)).await,
            Lookup::Recorded(300)
        ));
        records.replace(key(6, 2), 300, 400).await;
        assert!(matches!(
            records.lookup(&key(6, 2)).await,
            Lookup::Missing(_)
        ));
    })
    .await;
}

/// A time read from elsewhere after a lookup missed is not adopted once the shard has changed,
/// since the change may be a newer record of the path, or its removal.
#[tokio::test]
async fn a_time_is_not_adopted_over_a_change_since_the_lookup() {
    in_context(async {
        let dir = TempDir::new("records-adopt-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;

        let Lookup::Missing(generation) = records.lookup(&key(4, 1)).await else {
            panic!("nothing is recorded yet");
        };
        records.set(key(4, 1), 200).await;
        records.remove(&key(4, 1)).await;
        // Merged, so the removal is no longer held anywhere.
        write_back(&records, &token).await;
        records.adopt(key(4, 1), 100, generation).await;
        write_back(&records, &token).await;
        assert!(matches!(
            records.lookup(&key(4, 1)).await,
            Lookup::Missing(_)
        ));

        let Lookup::Missing(generation) = records.lookup(&key(4, 1)).await else {
            panic!("the record was removed");
        };
        records.adopt(key(4, 1), 100, generation).await;
        write_back(&records, &token).await;
        assert_eq!(records.lookup(&key(4, 1)).await, Lookup::Recorded(100));
    })
    .await;
}

/// A time set for a key the shard does not hold, and its removal, are seen by a lookup before the
/// shard merges them, the latest for the key first.
#[tokio::test]
async fn a_time_set_for_a_new_key_is_seen_before_the_shard_merges_it() {
    in_context(async {
        let dir = TempDir::new("records-set-seen-");
        let records = InstanceRecords::open(dir.path()).await;
        records.set(key(5, 1), 200).await;
        assert_eq!(records.lookup(&key(5, 1)).await, Lookup::Recorded(200));
        records.set(key(5, 1), 300).await;
        assert_eq!(records.lookup(&key(5, 1)).await, Lookup::Recorded(300));
        records.remove(&key(5, 1)).await;
        assert!(matches!(
            records.lookup(&key(5, 1)).await,
            Lookup::Missing(_)
        ));
    })
    .await;
}

/// A removal not merged yet wins over a time adopted after the lookup that missed.
#[tokio::test]
async fn a_removal_waiting_to_be_merged_wins_over_a_time_adopted_after_it() {
    in_context(async {
        let dir = TempDir::new("records-remove-then-adopt-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        records.set(key(5, 1), 200).await;
        records.remove(&key(5, 1)).await;
        let Lookup::Missing(generation) = records.lookup(&key(5, 1)).await else {
            panic!("the removal is seen before the merge");
        };
        records.adopt(key(5, 1), 100, generation).await;
        write_back(&records, &token).await;
        assert!(matches!(
            records.lookup(&key(5, 1)).await,
            Lookup::Missing(_)
        ));
    })
    .await;
}

/// Removing a key the shard holds no time for leaves nothing to write back, and still keeps a
/// time read before the removal from being adopted.
#[tokio::test]
async fn removing_a_key_not_held_changes_nothing_to_write() {
    in_context(async {
        let dir = TempDir::new("records-remove-not-held-");
        let records = InstanceRecords::open(dir.path()).await;
        let Lookup::Missing(generation) = records.lookup(&key(2, 1)).await else {
            panic!("nothing is recorded yet");
        };
        records.remove(&key(2, 1)).await;
        assert!(!records.is_dirty(), "the removal changes no shard");
        records.adopt(key(2, 1), 100, generation).await;
        assert!(
            !records.is_dirty(),
            "the time read before the removal is not adopted"
        );
    })
    .await;
}

/// A batch of the times the records already hold changes no shard, and leaves nothing to write
/// back.
#[tokio::test]
async fn a_batch_of_the_times_held_changes_nothing_to_write() {
    in_context(async {
        let dir = TempDir::new("records-batch-held-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        let batch = || {
            let mut batch = RecordBatch::with_capacity(2);
            batch.push(Record::new(key(3, 1), 100));
            batch.push(Record::new(key(3, 2), 200));
            batch
        };
        records.set_all(batch()).await.expect("set the records");
        write_back(&records, &token).await;
        records
            .set_all(batch())
            .await
            .expect("set the records again");
        assert!(!records.is_dirty(), "the same times change no shard");
        assert_eq!(records.lookup(&key(3, 2)).await, Lookup::Recorded(200));
    })
    .await;
}

/// A batch that reaches every shard sets each of them, and the next holder reads them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_across_every_shard_sets_each() {
    in_context(async {
        let dir = TempDir::new("records-batch-every-shard-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        let mut batch = RecordBatch::with_capacity(512);
        for shard in 0..=u8::MAX {
            for fill in [1, 2] {
                batch.push(Record::new(
                    key(shard, fill),
                    u64::from(shard) * 10 + u64::from(fill),
                ));
            }
        }
        records.set_all(batch).await.expect("set the records");
        write_back(&records, &token).await;
        drop(records);

        let records = InstanceRecords::open(dir.path()).await;
        for shard in 0..=u8::MAX {
            for fill in [1, 2] {
                assert_eq!(
                    records.lookup(&key(shard, fill)).await,
                    Lookup::Recorded(u64::from(shard) * 10 + u64::from(fill))
                );
            }
        }
    })
    .await;
}

/// Misses in one shard adopt their times side by side: one adoption does not reject another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_adoptions_in_one_shard_all_land() {
    in_context(async {
        let dir = TempDir::new("records-adopt-concurrent-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        let mut misses = Vec::new();
        for fill in 1..=8 {
            let Lookup::Missing(generation) = records.lookup(&key(6, fill)).await else {
                panic!("nothing is recorded yet");
            };
            misses.push((fill, generation));
        }
        let adoptions = misses.into_iter().map(|(fill, generation)| {
            let records = records.clone();
            lore_spawn!(async move {
                records
                    .adopt(key(6, fill), u64::from(fill) * 10, generation)
                    .await;
            })
        });
        for adoption in futures::future::join_all(adoptions).await {
            adoption.expect("the adoption ran");
        }
        write_back(&records, &token).await;
        for fill in 1..=8 {
            assert_eq!(
                records.lookup(&key(6, fill)).await,
                Lookup::Recorded(u64::from(fill) * 10)
            );
        }
    })
    .await;
}

/// Lookups that reach a shard not read yet all wait for one read of its file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_lookups_read_a_shard_once() {
    in_context(async {
        let dir = TempDir::new("records-load-concurrent-");
        write_shard(
            dir.path(),
            7,
            &shard_file(FORMAT_VERSION, &[(key(7, 1), 100)]),
        );
        let records = InstanceRecords::open(dir.path()).await;
        let lookups = (0..16).map(|_| {
            let records = records.clone();
            lore_spawn!(async move { records.lookup(&key(7, 1)).await })
        });
        for lookup in futures::future::join_all(lookups).await {
            assert_eq!(lookup.expect("the lookup ran"), Lookup::Recorded(100));
        }
        assert_eq!(records.loads(), 1, "the shard file is read once");
    })
    .await;
}

/// Lookups answer from a shard while its write-backs are in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookups_answer_during_a_write_back() {
    in_context(async {
        let dir = TempDir::new("records-lookup-flush-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        let mut batch = RecordBatch::with_capacity(0);
        for fill in 1..=200 {
            batch.push(Record::new(key(8, fill), u64::from(fill)));
        }
        records.set_all(batch).await.expect("set the records");

        let lookups = {
            let records = records.clone();
            lore_spawn!(async move {
                for round in 0..50 {
                    for fill in 1..=200 {
                        assert_eq!(
                            records.lookup(&key(8, fill)).await,
                            Lookup::Recorded(u64::from(fill)),
                            "round {round}"
                        );
                    }
                    tokio::task::yield_now().await;
                }
            })
        };
        for round in 0..20 {
            records.set(key(8, 250), round + 1).await;
            write_back(&records, &token).await;
        }
        lookups.await.expect("the lookups answered");
    })
    .await;
}

/// A shard that fails to write is reported and stays changed, and the others are still written.
#[tokio::test]
async fn a_failed_shard_write_is_reported_and_tried_again() {
    in_context(async {
        let dir = TempDir::new("records-flush-failure-");
        let token = token(&dir).await;
        let records = InstanceRecords::open(dir.path()).await;
        for shard in [4, 5, 6] {
            records.set(key(shard, 1), u64::from(shard)).await;
        }
        // A directory where shard 5's file goes, which its write cannot rename over.
        std::fs::create_dir_all(shard_path(dir.path(), 5).join("blocked"))
            .expect("block the shard");

        assert!(records.write_back(&token, false, ()).await.is_err());
        assert!(
            records.is_dirty(),
            "the failed shard is still to be written"
        );
        {
            let written = InstanceRecords::open(dir.path()).await;
            assert_eq!(written.lookup(&key(4, 1)).await, Lookup::Recorded(4));
            assert_eq!(written.lookup(&key(6, 1)).await, Lookup::Recorded(6));
        }

        // Test fixture writes; not subject to repository write-token discipline.
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_dir_all(shard_path(dir.path(), 5)).expect("unblock the shard");
        write_back(&records, &token).await;
        assert!(!records.is_dirty());
        let written = InstanceRecords::open(dir.path()).await;
        assert_eq!(written.lookup(&key(5, 1)).await, Lookup::Recorded(5));
    })
    .await;
}

/// Dropping the records writes nothing: the flush that ends a command writes them back.
#[tokio::test]
async fn dropping_the_records_writes_nothing() {
    in_context(async {
        let dir = TempDir::new("records-drop-");
        let records = InstanceRecords::open(dir.path()).await;
        records.set(key(1, 1), 100).await;
        drop(records);
        assert!(!dir.path().join(RECORDS_DIR).exists());
    })
    .await;
}

/// A well-formed shard file holding more records than a shard can is treated as absent.
#[tokio::test]
async fn a_shard_past_the_record_limit_is_absent() {
    in_context(async {
        let dir = TempDir::new("records-limit-");
        let count = SHARD_RECORDS_MAX + 1;
        let mut body =
            Vec::with_capacity(MAGIC.len() + size_of::<u32>() + count * size_of::<Record>());
        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        for index in 0..count as u64 {
            let mut key = [9u8; 16];
            key[8..].copy_from_slice(&index.to_be_bytes());
            body.extend_from_slice(&key);
            body.extend_from_slice(&1u64.to_le_bytes());
        }
        write_shard(dir.path(), 9, &body);
        drop(body);

        let records = InstanceRecords::open(dir.path()).await;
        assert!(matches!(
            records.lookup(&key(9, 1)).await,
            Lookup::Missing(_)
        ));
        assert!(records.is_dirty(), "the shard is replaced at write-back");
    })
    .await;
}

/// Signals when its drop starts, then waits for a release, standing in for a write whose
/// completion has not landed.
struct HeldCompletion {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Drop for HeldCompletion {
    fn drop(&mut self) {
        let _ = self.started.send(());
        let _ = self.release.recv();
    }
}

/// A write of a shard's file waits for an earlier write of it to land, even one whose write-back
/// is no longer awaited, so the two never share the shard's temporary file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shard_write_waits_for_the_last_one_to_land() {
    in_context(async {
        let dir = TempDir::new("records-write-order-");
        std::fs::create_dir_all(dir.path().join(RECORDS_DIR))
            .expect("create the records directory");
        let records = InstanceRecords::open(dir.path()).await;
        records.set(key(7, 1), 100).await;
        let (started, started_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let held = HeldCompletion {
            started,
            release: release_rx,
        };
        let first = lore_spawn!(Arc::clone(&records).write_shard_file(7, false, held));
        lore_spawn_blocking!(move || started_rx.recv())
            .await
            .expect("join the wait")
            .expect("the first write ends");

        records.set(key(7, 2), 200).await;
        let mut second = std::pin::pin!(Arc::clone(&records).write_shard_file(7, false, ()));
        assert!(second.as_mut().now_or_never().is_none());
        assert!(
            records.is_dirty(),
            "the second write waits for the first to land before it claims the shard"
        );
        release.send(()).expect("release the first write");
        first
            .await
            .expect("join the first write")
            .expect("the first write");
        second.await.expect("the second write");

        let written = InstanceRecords::open(dir.path()).await;
        assert_eq!(written.lookup(&key(7, 1)).await, Lookup::Recorded(100));
        assert_eq!(written.lookup(&key(7, 2)).await, Lookup::Recorded(200));
    })
    .await;
}

/// Whether any file under `root` holds an entry under the store key `key`, zeroed or not. The
/// mutable store writes a key's bytes as they are, apart from the key type it files in byte 2.
fn store_files_hold(root: &Path, key: &Hash) -> bool {
    let needle = &key.data()[3..];
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory).expect("read the directory") {
            let path = entry.expect("read the entry").path();
            if path.is_dir() {
                directories.push(path);
            } else if std::fs::read(&path)
                .expect("read the file")
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return true;
            }
        }
    }
    false
}

/// The time the mutable store holds for `path` under the instance's store key, 0 for none.
async fn store_time(repository: &RepositoryContext, path: &RelativePath) -> u64 {
    let key = state::file_modified_time_key(repository.salt(), repository.instance_id, path);
    repository
        .read_mutable_store()
        .load(repository.root_id(), key, KeyType::Untyped)
        .await
        .map_or(0, |value| {
            u64::from_ne_bytes(value.data()[..8].try_into().expect("eight bytes"))
        })
}

/// Writes `mtime` into the mutable store for `path`, as a client that kept the records there did.
async fn write_store_time(repository: &RepositoryContext, path: &RelativePath, mtime: u64) {
    let key = state::file_modified_time_key(repository.salt(), repository.instance_id, path);
    repository
        .try_write_mutable_store()
        .expect("opened for writing")
        .store(
            repository.root_id(),
            key,
            Hash::from_u64(mtime),
            KeyType::Untyped,
        )
        .await
        .expect("store the time");
}

/// What the instance's records hold for `path`, once the times collected are merged.
async fn instance_record(repository: &RepositoryContext, path: &RelativePath) -> Lookup {
    repository
        .flush(FlushOptions::default())
        .await
        .expect("flush");
    repository
        .instance_records()
        .expect("the instance keeps its own records")
        .lookup(&state::file_modified_time_record_key(
            repository.salt(),
            path,
        ))
        .await
}

/// Runs `test` in a task on the test's own runtime, under the execution `args` give, against a
/// committed repository holding `files`, which it is handed with the repository's path.
async fn in_repository_with<F, Fut>(args: LoreGlobalArgs, files: &[&str], test: F)
where
    F: FnOnce(Arc<RepositoryContext>, PathBuf) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let files: Vec<String> = files.iter().map(|file| file.to_string()).collect();
    in_execution_with(args, async move {
        let (repository, tempdir) = committed_repository("", &files).await;
        test(repository, tempdir.to_path_buf()).await;
        drop(tempdir);
    })
    .await;
}

/// Runs `body` in the execution context of a command given `args`.
async fn in_execution_with(
    args: LoreGlobalArgs,
    body: impl std::future::Future<Output = ()> + Send + 'static,
) {
    #[allow(clippy::disallowed_methods)]
    runtime()
        .spawn(LORE_CONTEXT.scope(test_execution_with(args), body))
        .await
        .expect("Test task failed");
}

async fn in_repository<F, Fut>(test: F)
where
    F: FnOnce(Arc<RepositoryContext>, PathBuf) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    in_repository_with(LoreGlobalArgs::default(), &[], test).await;
}

/// The instance at `path` reopened as one an older client created, whose records the mutable
/// store can hold. Takes the only context on it, so that its repository lock is released.
async fn reopen_as_older_instance(
    repository: Arc<RepositoryContext>,
    path: &Path,
) -> Arc<RepositoryContext> {
    let dot_path = repository
        .dot_dir_path()
        .expect("a dot directory")
        .to_path_buf();
    drop(repository);
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    std::fs::remove_file(dot_path.join(RECORDS_DIR).join(NO_STORE_RECORDS))
        .expect("remove the mark");
    let repository = lore_revision::repository::load_and_connect(path, RepositoryAccess::ReadWrite)
        .await
        .expect("reopen the repository");
    assert!(
        repository
            .instance_records()
            .expect("the instance keeps its own records")
            .has_store_records()
    );
    repository
}

/// A new instance records nothing in the mutable store: not on commit, and not from a batch as a
/// clone writes its times.
#[tokio::test]
async fn a_new_instance_records_nothing_in_the_store() {
    in_repository_with(
        LoreGlobalArgs::default(),
        &["committed.file"],
        |repository, path| async move {
            let cloned = relative_path("cloned.file");
            let collected = state::RecordedModifiedTimes::default();
            collected.push(state::file_modified_time_entry(&repository, &cloned, 5678));
            collected.store(repository.clone()).await;
            assert_eq!(
                instance_record(&repository, &cloned).await,
                Lookup::Recorded(5678)
            );
            assert!(
                matches!(
                    instance_record(&repository, &relative_path("committed.file")).await,
                    Lookup::Recorded(_)
                ),
                "the commit recorded the file's time in the instance"
            );

            repository
                .flush(FlushOptions::default())
                .await
                .expect("flush");
            let keys: Vec<(&str, Hash)> = ["committed.file", "seed.txt", "cloned.file"]
                .into_iter()
                .map(|file| {
                    let key = state::file_modified_time_key(
                        repository.salt(),
                        repository.instance_id,
                        &relative_path(file),
                    );
                    (file, key)
                })
                .collect();
            // The repository's lock files hold a byte-range lock while it is open, and Windows
            // refuses a read of a locked range, so the files are read once the lock is released.
            drop(repository);
            for (file, key) in keys {
                assert!(!store_files_hold(&path, &key), "{file} is in the store");
            }
        },
    )
    .await;
}

/// A new instance does not read the store for a path it has no record of, and does not zero the
/// store's record when it records the path.
#[tokio::test]
async fn a_new_instance_does_not_read_the_store() {
    in_repository(|repository, _| async move {
        let path = relative_path("store-only.file");
        write_store_time(&repository, &path, 1234).await;
        assert_eq!(
            state::file_modified_time(repository.clone(), &path).await,
            0
        );
        state::file_modified_time_store(repository.clone(), &path, 5678).await;
        assert_eq!(store_time(&repository, &path).await, 1234, "left alone");
    })
    .await;
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read the directory") {
        let entry = entry.expect("read the entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target).expect("create the directory");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy the file");
        }
    }
}

/// The records in a copy of an instance's directory describe the copy's files under the copy's
/// own identity.
#[tokio::test]
async fn a_copy_given_a_new_identity_reads_the_records_it_carries() {
    in_repository_with(
        LoreGlobalArgs::default(),
        &["copied.file"],
        |repository, path| async move {
            let file = relative_path("copied.file");
            let recorded = state::file_modified_time(repository.clone(), &file).await;
            assert_ne!(recorded, 0, "the commit recorded the file's time");
            repository
                .flush(FlushOptions::default())
                .await
                .expect("flush");
            let old_identity = repository.instance_id;
            drop(repository);

            let copy = TempDir::new("records-copy-");
            copy_tree(&path, copy.path());
            lore_revision::instance::InstanceId::generate()
                .write_to_file(
                    copy.path()
                        .join(lore_revision::repository::DOT_LORE)
                        .join("instance"),
                )
                .await
                .expect("give the copy an identity of its own");

            let copied = lore_revision::repository::load_and_connect(
                copy.path(),
                RepositoryAccess::ReadWrite,
            )
            .await
            .expect("open the copy");
            assert!(copied.instance_id != old_identity);
            assert!(
                !copied
                    .instance_records()
                    .expect("the copy keeps its own records")
                    .has_store_records(),
                "the copy carries the mark"
            );
            assert_eq!(
                state::file_modified_time(copied.clone(), &file).await,
                recorded
            );
        },
    )
    .await;
}

/// An instance's records reach its dot directory with the flush a command ends with, which keeps
/// the repository lock until they are written, and are read when the instance is opened again.
#[tokio::test]
async fn records_are_written_back_after_the_command() {
    in_repository(|repository, path| async move {
        let file = relative_path("after-the-command.file");
        state::file_modified_time_store(repository.clone(), &file, 5678).await;
        let key = state::file_modified_time_record_key(repository.salt(), &file);
        let dot_path = repository
            .dot_dir_path()
            .expect("a dot directory")
            .to_path_buf();

        repository.try_spawn_post_command_flush(FlushOptions::default());
        drop(repository);
        // The flush's task holds a sibling of the instance's token until it ends.
        drop(RepositoryWriteToken::acquire(&path).await);

        let records = InstanceRecords::open(&dot_path).await;
        assert_eq!(records.lookup(&key).await, Lookup::Recorded(5678));
        let reopened =
            lore_revision::repository::load_and_connect(&path, RepositoryAccess::ReadWrite)
                .await
                .expect("reopen the repository");
        assert_eq!(state::file_modified_time(reopened, &file).await, 5678);
    })
    .await;
}

/// Records a time for `file` in `repository`'s instance, and stores a fragment whose packfile is not
/// synced, so that a flush that syncs fails once [`fail_packfile_syncs`] has armed it. Returns the
/// record's key and the instance's dot directory.
async fn record_with_a_fragment_pending(
    repository: &Arc<RepositoryContext>,
    file: &RelativePath,
) -> (RecordKey, PathBuf) {
    state::file_modified_time_store(repository.clone(), file, 5678).await;
    let payload = b"a payload whose sync fails";
    store_fragment(
        repository,
        payload,
        FragmentFlags::empty(),
        payload.len() as u64,
    )
    .await;
    let dot_path = repository
        .dot_dir_path()
        .expect("a dot directory")
        .to_path_buf();
    (
        state::file_modified_time_record_key(repository.salt(), file),
        dot_path,
    )
}

/// The flush a command ends with writes no records once its store flush has failed: the anchors
/// on disk still name the revision before the command, which the records do not describe.
#[tokio::test]
async fn records_are_not_written_back_after_a_failed_store_flush() {
    in_repository(|repository, path| async move {
        let file = relative_path("unflushed.file");
        let (key, dot_path) = record_with_a_fragment_pending(&repository, &file).await;

        fail_packfile_syncs(1);
        repository.try_spawn_post_command_flush(FlushOptions::SYNC_DATA);
        drop(repository);
        // The flush's task holds a sibling of the instance's token until it ends.
        drop(RepositoryWriteToken::acquire(&path).await);
        assert_eq!(
            packfile_sync_failures_left(),
            0,
            "the flush ran, and its immutable store's flush failed"
        );

        let records = InstanceRecords::open(&dot_path).await;
        assert!(matches!(records.lookup(&key).await, Lookup::Missing(_)));
    })
    .await;
}

/// A flush whose store flush fails returns the error and writes no records.
#[tokio::test]
async fn a_failed_store_flush_writes_no_records() {
    in_repository(|repository, _| async move {
        let file = relative_path("unflushed.file");
        let (key, dot_path) = record_with_a_fragment_pending(&repository, &file).await;

        fail_packfile_syncs(1);
        assert!(
            repository.flush(FlushOptions::SYNC_DATA).await.is_err(),
            "the flush returns the store's error"
        );
        assert_eq!(
            packfile_sync_failures_left(),
            0,
            "the immutable store's flush failed"
        );
        drop(repository);

        let records = InstanceRecords::open(&dot_path).await;
        assert!(matches!(records.lookup(&key).await, Lookup::Missing(_)));
    })
    .await;
}

/// The arguments of a command over an instance whose stores are in memory.
fn in_memory_args() -> LoreGlobalArgs {
    LoreGlobalArgs {
        in_memory: 1,
        ..LoreGlobalArgs::default()
    }
}

/// An in-memory instance keeps its records in memory beside its stores: a commit, a time recorded
/// and a batch reach them, and neither its mutable store nor its Lore directory holds any.
#[tokio::test]
async fn an_in_memory_instance_keeps_its_records_in_memory() {
    in_repository_with(in_memory_args(), &[], |repository, path| async move {
        let committed = relative_path("committed.file");
        let committed_path = path.join("committed.file");
        test_file_write(&committed_path, b"committed");
        std::fs::File::options()
            .write(true)
            .open(&committed_path)
            .and_then(|file| {
                file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30))
            })
            .expect("date the file before any clock the commit reads");
        stage_paths(&repository, &path, &["committed.file"], true, false).await;
        assert!(matches!(
            instance_record(&repository, &committed).await,
            Lookup::Missing(_)
        ));
        lore_revision::commit::commit_boxed(
            repository.clone(),
            repository
                .try_write_token()
                .expect("the repository is open for writing"),
            lore_revision::commit::CommitOptions::new("Commit".to_string()),
        )
        .await
        .expect("commit");
        assert_eq!(
            instance_record(&repository, &committed).await,
            Lookup::Recorded((1 << 30) * 1000),
            "the commit recorded the file's time"
        );

        let file = relative_path("in-memory.file");
        state::file_modified_time_store(repository.clone(), &file, 5678).await;
        assert_eq!(
            state::file_modified_time(repository.clone(), &file).await,
            5678
        );
        assert_eq!(store_time(&repository, &file).await, 0, "not in the store");
        state::file_modified_time_clear(repository.clone(), &file).await;
        assert_eq!(
            state::file_modified_time(repository.clone(), &file).await,
            0
        );

        let cloned = relative_path("cloned.file");
        let collected = state::RecordedModifiedTimes::default();
        collected.push(state::file_modified_time_entry(&repository, &cloned, 6789));
        collected.store(repository.clone()).await;
        assert_eq!(
            instance_record(&repository, &cloned).await,
            Lookup::Recorded(6789)
        );
        assert_eq!(
            store_time(&repository, &cloned).await,
            0,
            "not in the store"
        );

        repository
            .flush(FlushOptions::default())
            .await
            .expect("flush the instance");
        assert!(!path.join(DOT_LORE).join(RECORDS_DIR).exists());
    })
    .await;
}

/// Every context over an in-memory instance's stores shares its records, which the process keeps
/// with the stores until it releases them.
#[tokio::test]
async fn contexts_over_an_in_memory_instance_share_its_records() {
    in_repository_with(in_memory_args(), &[], |repository, path| async move {
        let file = relative_path("shared.file");
        state::file_modified_time_store(repository.clone(), &file, 5678).await;
        let other = load_and_connect(&path, RepositoryAccess::ReadOnly)
            .await
            .expect("open the instance again");
        let records = repository
            .instance_records()
            .expect("the instance keeps its records in memory");
        assert!(Arc::ptr_eq(
            records,
            other
                .instance_records()
                .expect("the instance keeps its records in memory")
        ));
        assert_eq!(state::file_modified_time(other.clone(), &file).await, 5678);

        let records = Arc::downgrade(records);
        drop((repository, other));
        assert!(
            records.upgrade().is_some(),
            "kept for the instance's next command"
        );
        repository_release(&path);
        assert!(records.upgrade().is_none(), "released with the stores");
    })
    .await;
}

/// Opens of an instance with in-memory stores that both find none of its stores cached share the
/// stores and records the first of them creates, which the cache keeps.
#[tokio::test]
async fn racing_first_opens_of_an_in_memory_instance_share_its_records() {
    in_repository(|repository, path| async move {
        drop(repository);
        let (reached, mut missed) = tokio::sync::mpsc::unbounded_channel();
        let open_held = |resume: Arc<tokio::sync::Notify>| {
            let path = path.clone();
            lore_spawn!(LORE_CONTEXT.scope(
                test_execution_with(in_memory_args()),
                hold_in_memory_store_misses(reached.clone(), resume, async move {
                    load_and_connect(&path, RepositoryAccess::ReadOnly).await
                }),
            ))
        };
        let (first_resume, second_resume) = (Arc::default(), Arc::default());
        let first = open_held(Arc::clone(&first_resume));
        let within = std::time::Duration::from_secs(10);
        assert!(
            matches!(
                tokio::time::timeout(within, missed.recv()).await,
                Ok(Some(()))
            ),
            "the first open finds no stores cached"
        );
        let second = open_held(Arc::clone(&second_resume));
        assert!(
            matches!(
                tokio::time::timeout(within, missed.recv()).await,
                Ok(Some(()))
            ),
            "the second open finds no stores cached"
        );

        first_resume.notify_one();
        let first = first
            .await
            .expect("the first open ran")
            .expect("open the instance");
        second_resume.notify_one();
        let second = second
            .await
            .expect("the second open ran")
            .expect("open the instance");
        let records = first
            .instance_records()
            .expect("the instance keeps its records in memory");
        assert!(Arc::ptr_eq(
            records,
            second
                .instance_records()
                .expect("the instance keeps its records in memory")
        ));
        let cached =
            get_cached_in_memory_stores(&path.join(DOT_LORE)).expect("the stores are cached");
        assert!(Arc::ptr_eq(records, &cached.records));
        drop((first, second, cached));
        repository_release(&path);
    })
    .await;
}

/// The context that creates an in-memory instance records in the set cached beside its stores,
/// which the instance's later commands share.
#[tokio::test]
async fn creating_an_in_memory_instance_records_beside_its_stores() {
    in_execution_with(in_memory_args(), async {
        let tempdir = crate::helper::generate_tempdir();
        let path = tempdir.to_path_buf();
        let token = RepositoryWriteToken::acquire(&path).await;
        let created = lore_revision::repository::create_local(
            &path,
            &token,
            lore_revision::lore::RepositoryId::from(uuid::Uuid::now_v7()),
            lore_revision::lore::BranchId::from(uuid::Uuid::now_v7()),
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            RepositoryConfig::default(),
            false,
        )
        .await
        .expect("create the instance");
        let cached = get_cached_in_memory_stores(&path.join(DOT_LORE))
            .expect("the instance's stores are cached");
        assert!(Arc::ptr_eq(
            created
                .instance_records()
                .expect("the instance keeps its records in memory"),
            &cached.records
        ));
        drop((created, cached, token));
        repository_release(&path);
    })
    .await;
}

/// A command over an instance with in-memory stores keeps its records apart from the set in the
/// instance's Lore directory, where it shares the repository lock a command over the stores on
/// disk took, and writes none of that lock's records back: its flush does not reach the stores on
/// disk they describe.
#[tokio::test]
async fn an_in_memory_command_keeps_its_records_apart_from_the_lore_directory() {
    in_repository(|repository, path| async move {
        let on_disk = load_and_connect(&path, RepositoryAccess::ReadOnly)
            .await
            .expect("open the instance");
        drop(repository);
        let in_memory = LORE_CONTEXT
            .scope(
                test_execution_with(in_memory_args()),
                load_and_connect(&path, RepositoryAccess::ReadWrite),
            )
            .await
            .expect("open the instance with in-memory stores");
        let on_disk_records = on_disk
            .instance_records()
            .expect("the instance keeps its records on disk");
        assert!(!Arc::ptr_eq(
            on_disk_records,
            in_memory
                .instance_records()
                .expect("the command keeps its records in memory")
        ));

        let file = relative_path("apart.file");
        state::file_modified_time_store(in_memory.clone(), &file, 5678).await;
        assert_eq!(
            state::file_modified_time(in_memory.clone(), &file).await,
            5678
        );
        assert_eq!(state::file_modified_time(on_disk.clone(), &file).await, 0);

        on_disk_records.set(key(4, 1), 400).await;
        in_memory
            .flush(FlushOptions::default())
            .await
            .expect("flush the in-memory stores");
        in_memory
            .write_back_instance_records()
            .await
            .expect("write back the records");
        assert!(
            on_disk_records.is_dirty(),
            "left to the commands over the stores on disk"
        );
        in_memory.try_spawn_post_command_flush(FlushOptions::default());
        drop(in_memory);
        // A flush that writes the records back holds a sibling of the instance's token until it
        // ends.
        drop(RepositoryWriteToken::acquire(&path).await);
        assert!(
            on_disk_records.is_dirty(),
            "left to the commands over the stores on disk"
        );
        repository_release(&path);
    })
    .await;
}

/// A lookup that misses in an older instance's records reads the time an older client left in
/// the store, copies it into the records, and leaves the store's record in place.
#[tokio::test]
async fn a_store_record_is_copied_in_and_left_in_the_store() {
    in_repository(|repository, path| async move {
        let repository = reopen_as_older_instance(repository, &path).await;
        let file = relative_path("older-client.file");
        write_store_time(&repository, &file, 1234).await;
        assert!(matches!(
            instance_record(&repository, &file).await,
            Lookup::Missing(_)
        ));

        assert_eq!(
            state::file_modified_time(repository.clone(), &file).await,
            1234
        );
        assert_eq!(
            instance_record(&repository, &file).await,
            Lookup::Recorded(1234)
        );
        assert_eq!(
            store_time(&repository, &file).await,
            1234,
            "left in the store"
        );
    })
    .await;
}

#[tokio::test]
async fn recording_a_new_time_zeroes_an_older_instances_store_record() {
    in_repository(|repository, path| async move {
        let repository = reopen_as_older_instance(repository, &path).await;
        let file = relative_path("rewritten.file");
        write_store_time(&repository, &file, 1234).await;

        state::file_modified_time_store(repository.clone(), &file, 5678).await;
        assert_eq!(
            instance_record(&repository, &file).await,
            Lookup::Recorded(5678)
        );
        assert_eq!(
            store_time(&repository, &file).await,
            0,
            "zeroed in the store"
        );
    })
    .await;
}

#[tokio::test]
async fn a_batch_of_new_times_zeroes_an_older_instances_store_records() {
    in_repository(|repository, path| async move {
        let repository = reopen_as_older_instance(repository, &path).await;
        let file = relative_path("synced.file");
        write_store_time(&repository, &file, 1234).await;

        let collected = state::RecordedModifiedTimes::default();
        collected.push(state::file_modified_time_entry(&repository, &file, 5678));
        collected.store(repository.clone()).await;
        assert_eq!(
            instance_record(&repository, &file).await,
            Lookup::Recorded(5678)
        );
        assert_eq!(
            store_time(&repository, &file).await,
            0,
            "zeroed in the store"
        );
    })
    .await;
}

#[tokio::test]
async fn clearing_removes_the_record_and_zeroes_an_older_instances_store_record() {
    in_repository(|repository, path| async move {
        let repository = reopen_as_older_instance(repository, &path).await;
        let file = relative_path("cleared.file");
        write_store_time(&repository, &file, 1234).await;
        assert_eq!(
            state::file_modified_time(repository.clone(), &file).await,
            1234
        );

        state::file_modified_time_clear(repository.clone(), &file).await;
        assert!(matches!(
            instance_record(&repository, &file).await,
            Lookup::Missing(_)
        ));
        assert_eq!(
            store_time(&repository, &file).await,
            0,
            "zeroed in the store"
        );
        assert_eq!(
            state::file_modified_time(repository.clone(), &file).await,
            0
        );
    })
    .await;
}

/// SWFS instances keep their records in the mutable store, other instances whose stores are in
/// memory keep them in memory, and every other instance, a FUSE mount included, in its Lore
/// directory.
#[test]
fn each_instance_keeps_its_records_where_its_kind_and_its_stores_say() {
    for vfs_type in [VfsType::None, VfsType::Fuse, VfsType::Swfs] {
        let config = RepositoryConfig {
            vfs: Some(VfsConfig {
                vfs_type,
                base_time: None,
            }),
            ..RepositoryConfig::default()
        };
        for in_memory in [false, true] {
            let expected = match (vfs_type, in_memory) {
                (VfsType::Swfs, _) => RecordsLocation::MutableStore,
                (_, true) => RecordsLocation::Memory,
                _ => RecordsLocation::DotDirectory,
            };
            assert_eq!(
                records_location(&config, in_memory),
                expected,
                "{vfs_type}, in memory: {in_memory}"
            );
        }
    }
}
