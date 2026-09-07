// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic;

use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::format_bucket_path;
use lore_storage::local::mutable_store::*;
use lore_storage::local::store_lock::Intent;
use lore_storage::store_types::KeyType;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

fn write_bucket_file(path: &Path, version: u32) {
    let entry = MutableStoreEntry::default();
    let mut header = MutableStoreHeader::new_zeroed();
    header.version = version;
    header.count = 1;
    let mut bytes =
        Vec::with_capacity(size_of::<MutableStoreHeader>() + 4 + size_of::<MutableStoreEntry>());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(entry.as_bytes());
    std::fs::write(path, bytes).unwrap();
}

/// A bucket larger than the head the composite open returns is loaded by scattering one
/// vectored read into both `GrowVec`s. Per-index contents make a misplaced chunk visible as
/// swapped entries rather than as a length mismatch.
#[tokio::test]
async fn deserialize_scatters_a_bucket_larger_than_the_head_read() {
    let dir = lore_base::test_util::TempDir::new("ms_scatter_");
    let path = dir.path().join("bucket");

    let head = lore_storage::local::immutable_store::BUCKET_HEAD_READ;
    let per_entry = size_of::<u32>() + size_of::<MutableStoreEntry>();
    let count = (head / per_entry) + 64;
    assert!(
        size_of::<MutableStoreHeader>() + count * per_entry > head,
        "the bucket has to exceed the head read for this to test anything"
    );

    let mut header = MutableStoreHeader::new_zeroed();
    header.version = MutableStoreVersion::LazyFanOut as u32;
    header.count = count as u32;

    let mut bytes = Vec::with_capacity(size_of::<MutableStoreHeader>() + count * per_entry);
    bytes.extend_from_slice(header.as_bytes());
    for index in 0..count {
        bytes.extend_from_slice(&(index as u32).to_le_bytes());
    }
    for index in 0..count {
        let entry = MutableStoreEntry {
            key: Hash::from([index as u8; 32]),
            ..Default::default()
        };
        bytes.extend_from_slice(entry.as_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let (sorted_index, entry, version) = MutableStoreBucket::deserialize_files(path, false)
        .await
        .unwrap();

    assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
    assert_eq!(sorted_index.len(), count);
    assert_eq!(entry.len(), count);
    for index in 0..count {
        assert_eq!(sorted_index[index], index as u32, "sorted index at {index}");
        assert_eq!(
            entry[index].key,
            Hash::from([index as u8; 32]),
            "key at {index}"
        );
    }
}

#[tokio::test]
async fn deserialize_accepts_typed_items_v2() {
    let dir = lore_base::test_util::TempDir::new("ms_v2_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, MutableStoreVersion::TypedItems as u32);
    let result = MutableStoreBucket::deserialize_files(path, false).await;
    assert!(result.is_ok(), "v2 (TypedItems) bucket should deserialize");
}

#[tokio::test]
async fn deserialize_accepts_lazy_fan_out_v3() {
    let dir = lore_base::test_util::TempDir::new("ms_v3_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, MutableStoreVersion::LazyFanOut as u32);
    let result = MutableStoreBucket::deserialize_files(path, false).await;
    assert!(result.is_ok(), "v3 (LazyFanOut) bucket should deserialize");
}

#[tokio::test]
async fn deserialize_rejects_unknown_future_version() {
    let dir = lore_base::test_util::TempDir::new("ms_v100_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, 100);
    let result = MutableStoreBucket::deserialize_files(path, false).await;
    assert!(result.is_err(), "v100 bucket should be rejected as too new");
}

/// A torn write can leave a bucket file at its correct byte length but entirely
/// zero-filled, so the header reads count=0 while the size implies a nonzero count. This
/// recovers to an empty bucket rather than hard-erroring.
#[tokio::test]
async fn deserialize_recovers_zero_filled_bucket() {
    let dir = lore_base::test_util::TempDir::new("ms_zerofill_");
    let path = dir.path().join("bucket");
    // Correct length for a 1-entry bucket, but all zeros.
    let len = size_of::<MutableStoreHeader>() + size_of::<u32>() + size_of::<MutableStoreEntry>();
    std::fs::write(&path, vec![0u8; len]).unwrap();

    let (sorted_index, entry, version) = MutableStoreBucket::deserialize_files(path, false)
        .await
        .expect("zero-filled bucket should recover to empty");
    assert_eq!(sorted_index.len(), 0);
    assert_eq!(entry.len(), 0);
    assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
}

#[tokio::test]
async fn deserialize_authoritative_errors_and_preserves_corrupt_bucket() {
    let dir = lore_base::test_util::TempDir::new("ms_auth_corrupt_");
    let path = dir.path().join("bucket");
    let len = size_of::<MutableStoreHeader>() + size_of::<u32>() + size_of::<MutableStoreEntry>();
    std::fs::write(&path, vec![0u8; len]).unwrap();

    let result = MutableStoreBucket::deserialize_files(path.clone(), true).await;
    assert!(
        result.is_err(),
        "authoritative store must not reset a corrupt bucket"
    );
    assert!(
        path.exists(),
        "authoritative store must preserve the corrupt bucket file"
    );
}

/// Client-shaped settings: groups start at level 1.
fn client_settings() -> MutableStoreSettings {
    MutableStoreSettings {
        initial_fan_out_level: 1,
        ..Default::default()
    }
}

/// Store `count` keys spread across groups, at bucket bytes that route away from bucket 0
/// once a group is at 256 — so a group misread as pre-fan-out sends a lookup to a different
/// bucket than the one holding it.
async fn store_keys(store: &Arc<LocalMutableStore>, partition: Partition, count: u8) -> Vec<Hash> {
    use lore_storage::mutable_store::MutableStore;
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    let mut keys = Vec::new();
    for index in 0..count {
        let mut key = Hash::default();
        key.data_mut()[0] = index;
        key.data_mut()[1] = 0xAB;
        dyn_store
            .clone()
            .store(
                partition,
                key,
                Hash::from_u64(index as u64 + 1),
                KeyType::BranchMetadata,
            )
            .await
            .expect("store succeeds");
        keys.push(key);
    }
    keys
}

/// Run the background timer's flush for every group's bucket 0, and nothing else. At level 1
/// that is the only addressable bucket.
async fn run_delayed_flush(store: &Arc<LocalMutableStore>) {
    let weak = Arc::downgrade(store);
    for group_index in 0..GROUP_COUNT {
        // Sweeps every dirty bucket in the group, so no bucket index is named.
        LocalMutableStore::flush_delayed(weak.clone(), group_index, 0).await;
    }
}

/// Every `level` marker under a store's index directory.
fn level_markers(index_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(groups) = std::fs::read_dir(index_root) else {
        return found;
    };
    for group in groups.flatten() {
        let marker = group
            .path()
            .join(lore_storage::local::fan_out::MARKER_FILENAME);
        if marker.exists() {
            found.push(marker);
        }
    }
    found
}

/// The mutable twin of the immutable store's regression: a group persisted only by the
/// delayed flush must be reopened at the level it was written at. Left marker-less, a level-1
/// group is read back as a pre-fan-out 256-bucket layout and everything in `index_00` moves
/// out of reach — here that is branch heads and revision metadata.
///
/// Reachable when a sub-256 store is given a non-zero flush delay. The client's level-1
/// default sets the delay to 0, which stops a write scheduling the sweep, and the server
/// runs the delayed flush with groups at 256, where a missing marker reads back correctly;
/// this pins the invariant rather than leaving it to those defaults.
#[tokio::test]
async fn a_group_the_delayed_flush_persisted_reopens_at_its_written_level() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_delayed_level_");
    let partition = Partition::default();
    let index_root = dir.path().join("mutable").join("index");

    let keys = {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                client_settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            1,
            "a client store starts its groups at level 1"
        );

        let keys = store_keys(&store, partition, 32).await;
        // Persist the way the background timer does, and nothing else: no `flush`, so the
        // two-phase commit that would write the markers never runs.
        run_delayed_flush(&store).await;
        keys
    };

    let mut checked = 0;
    for group in std::fs::read_dir(&index_root)
        .expect("index dir exists")
        .flatten()
    {
        let has_bucket = std::fs::read_dir(group.path())
            .expect("group dir")
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("index_"))
            });
        if !has_bucket {
            continue;
        }
        checked += 1;
        assert_eq!(
            lore_storage::local::fan_out::read_level_marker(&group.path())
                .await
                .expect("marker readable"),
            Some(1),
            "group {} must record the level its bucket files were written at",
            group.path().display()
        );
    }
    assert!(
        checked > 0,
        "the delayed flush has to have written something"
    );

    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            client_settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    for (index, key) in keys.iter().enumerate() {
        assert_eq!(
            store.group[key.data()[0] as usize]
                .bucket_count
                .load(atomic::Ordering::Relaxed),
            1,
            "group for {key} reopened at the wrong level"
        );
        let loaded = dyn_store
            .clone()
            .load(partition, *key, KeyType::BranchMetadata)
            .await
            .unwrap_or_else(|err| {
                panic!("{key} was stored and persisted but reads back as {err:?}")
            });
        assert_eq!(loaded, Hash::from_u64(index as u64 + 1));
    }
}

/// A store at the flat 256-bucket layout — every legacy store, and every server store —
/// gains no markers: such a group already reads back at the level it was written at.
#[tokio::test]
async fn a_flat_layout_store_gains_no_level_markers() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_flat_level_");
    let partition = Partition::default();
    let index_root = dir.path().join("mutable").join("index");
    let settings = || MutableStoreSettings {
        initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
        ..Default::default()
    };

    let keys = {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "this store starts at the flat layout"
        );

        let keys = store_keys(&store, partition, 32).await;
        let weak = Arc::downgrade(&store);
        for group_index in 0..GROUP_COUNT {
            LocalMutableStore::flush_delayed(weak.clone(), group_index, 0).await;
        }
        keys
    };

    assert_eq!(
        level_markers(&index_root),
        Vec::<PathBuf>::new(),
        "a group already at 256 reads back at 256 without a marker"
    );

    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.path()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .expect("store reopens"),
    );
    let dyn_store: Arc<dyn MutableStore> = store.clone();
    for (index, key) in keys.iter().enumerate() {
        assert_eq!(
            store.group[key.data()[0] as usize]
                .bucket_count
                .load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "group for {key} reopened at the wrong level"
        );
        let loaded = dyn_store
            .clone()
            .load(partition, *key, KeyType::BranchMetadata)
            .await
            .unwrap_or_else(|err| panic!("{key} reads back as {err:?}"));
        assert_eq!(loaded, Hash::from_u64(index as u64 + 1));
    }
}

/// A group that already carries a marker keeps the level it records: the initial-level write
/// is for groups that have never had one, and must not overwrite a committed level.
#[tokio::test]
async fn a_marked_group_keeps_the_level_it_recorded() {
    use lore_storage::mutable_store::MutableStore;
    let dir = lore_base::test_util::TempDir::new("ms_marked_level_");
    let partition = Partition::default();
    let index_root = dir.path().join("mutable").join("index");

    {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                client_settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store opens"),
        );
        let _ = store_keys(&store, partition, 8).await;
        let dyn_store: Arc<dyn MutableStore> = store.clone();
        // A real flush commits the level through the two-phase path.
        dyn_store.flush(false).await.expect("flush succeeds");
    }

    let before: Vec<(PathBuf, Vec<u8>)> = level_markers(&index_root)
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).expect("marker readable");
            (path, bytes)
        })
        .collect();
    assert!(
        !before.is_empty(),
        "the flush has to have committed a level"
    );

    {
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.path()),
                client_settings(),
                make_in_memory_immutable().await,
            )
            .await
            .expect("store reopens"),
        );
        let _ = store_keys(&store, partition, 16).await;
        run_delayed_flush(&store).await;
    }

    for (path, bytes) in before {
        assert_eq!(
            std::fs::read(&path).expect("marker still readable"),
            bytes,
            "marker at {} was rewritten",
            path.display()
        );
    }
}

#[test]
fn lazy_fan_out_version_is_three() {
    assert_eq!(MutableStoreVersion::LazyFanOut as u32, 3);
}

#[tokio::test]
async fn latest_version_constant_in_deserialize_path_matches_lazy_fan_out() {
    let dir = lore_base::test_util::TempDir::new("ms_latest_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, MutableStoreVersion::LazyFanOut as u32);
    let (_, _, version) = MutableStoreBucket::deserialize_files(path, false)
        .await
        .unwrap();
    assert_eq!(version, MutableStoreVersion::LazyFanOut as u32);
}

#[test]
fn mutable_store_settings_default_is_client_friendly() {
    let s = MutableStoreSettings::default();
    assert_eq!(s.flush_delay_seconds, DEFAULT_FLUSH_DELAY_SECONDS);
    assert_eq!(s.initial_fan_out_level, 1);
    assert_eq!(
        s.fan_out_threshold,
        lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT
    );
}

/// End-to-end: after a bucket file is zero-filled, the store still opens and the bucket is
/// usable for a store/load round-trip.
#[tokio::test]
async fn store_recovers_from_zero_filled_bucket_and_remains_usable() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_e2e_recover_");
    let store_path = dir.path().to_path_buf();
    let partition = Partition::default();
    let mut key = Hash::default();
    key.data_mut()[0] = 0x10;
    key.data_mut()[1] = 0xAB;
    let value = Hash::from_u64(42);

    {
        let store: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings {
                    initial_fan_out_level: 1,
                    ..Default::default()
                },
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        store
            .clone()
            .store(partition, key, value, KeyType::BranchMetadata)
            .await
            .unwrap();
        store.clone().flush(true).await.unwrap();
    }

    // initial_fan_out_level=1 → bucket index is always 0; group is (typed) key[0].
    // `LocalMutableStore::new` roots the store under a `mutable/` subdirectory.
    let group_index = key.data()[0] as usize;
    let bucket_path = format_bucket_path(&store_path.join("mutable"), group_index, 0);
    assert!(
        bucket_path.exists(),
        "bucket file should exist after flush at {bucket_path:?}"
    );

    // Torn write: correct byte length, entirely zero-filled.
    let len = std::fs::metadata(&bucket_path).unwrap().len() as usize;
    std::fs::write(&bucket_path, vec![0u8; len]).unwrap();

    let store: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );

    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .unwrap();
    let reloaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .unwrap();
    assert_eq!(reloaded, value, "bucket must be usable after recovery");
}

#[tokio::test]
async fn local_mutable_store_satisfies_conformance_battery() {
    let store = lore_storage::local::mutable_store::create(
        None::<&std::path::Path>,
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .expect("create store");
    lore_storage::mutable_conformance::verify_mutable_store(
        store,
        lore_storage::mutable_conformance::Capabilities::new("LocalMutableStore"),
    )
    .await;
}

async fn make_in_memory_immutable() -> Arc<dyn ImmutableStore> {
    lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("Failed to create in-memory immutable store")
}

#[tokio::test]
async fn store_initializes_group_bucket_count_from_settings_level_1() {
    use std::sync::atomic::Ordering;
    let store = LocalMutableStore::new(
        None::<&Path>,
        MutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    for group in store.group.iter() {
        assert_eq!(group.bucket_count.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn store_initializes_group_bucket_count_from_settings_level_256() {
    use std::sync::atomic::Ordering;
    let store = LocalMutableStore::new(
        None::<&Path>,
        MutableStoreSettings {
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ..Default::default()
        },
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    for group in store.group.iter() {
        assert_eq!(
            group.bucket_count.load(Ordering::Relaxed),
            lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX
        );
    }
}

/// **A migration store takes no claim of its own, so its caller must hold one.**
///
/// `for_migration` builds a view over an old on-disk layout with `lock: None`,
/// which means `hold` returns nothing and no epoch is advanced however much the
/// migration rewrites. Every other process on the directory would read the same
/// epoch before and after, serve the state it had, and flush that back over the
/// migrated files.
///
/// This pins the property the caller's obligation rests on: the migration store is
/// silent, so silence has to be made impossible some other way.
#[tokio::test]
async fn a_migration_store_holds_nothing_and_announces_nothing() {
    let dir = lore_base::test_util::TempDir::new("ms_migration_claim_");
    let path = dir.to_path_buf().join("mutable");
    std::fs::create_dir_all(&path).expect("store directory");

    let migrating = LocalMutableStore::for_migration(path.clone(), Vec::new());
    assert!(
        migrating
            .hold(Intent::Write)
            .await
            .expect("holds")
            .is_none(),
        "a migration view has no lock, so it can claim nothing"
    );
    assert!(
        !path.join("epoch").exists(),
        "and announces nothing, however much it rewrites — which is why the caller \
         has to hold the claim across the whole migration"
    );
}

/// **A claim outlives the store object that took it.**
///
/// What the migration rests on. It takes a claim from the live store, drops that
/// store so the rewrite has sole ownership of the files, and re-creates one
/// afterwards to read the migrated data back — so the claim has to survive the
/// object. It does because the flock belongs to the directory, not to a store: the
/// store built afterwards joins the same one rather than contending with it, which
/// before that was a process blocking against itself with an unbounded wait.
#[tokio::test]
async fn a_claim_outlives_the_store_that_took_it() {
    let dir = lore_base::test_util::TempDir::new("ms_claim_outlives_");
    let settings = MutableStoreSettings::default;

    let first = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            settings(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let claim = first
        .hold(Intent::Read)
        .await
        .expect("claims")
        .expect("a disk-backed store claims something");
    drop(first);

    let immutable = make_in_memory_immutable().await;
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        LocalMutableStore::new(Some(dir.to_path_buf()), settings(), immutable),
    )
    .await
    .expect("a store built while the claim is held joins it rather than waiting on it")
    .unwrap();
    let lock = second.lock.clone().expect("a disk-backed store has a lock");
    assert!(
        lock.is_held(),
        "the claim taken by the dropped store must still hold the directory"
    );

    drop(claim);
    assert!(!lock.is_held(), "and releasing it releases the directory");
}

/// **Stamping the version claims the store; recognising it costs nothing.**
///
/// The probe, the version read and the stamp are all decisions about on-disk state,
/// and the stamp is a write — so another process must not be rewriting it
/// underneath them. The case that recurs is a `version` file holding a value this
/// client does not recognise, which is what a newer client writes: it leaves the
/// version at `Initial`, so an older client re-stamps on every open.
///
/// The epoch is the observable. A stamp takes a write claim and advances it; an
/// open that reads a version it recognises writes nothing and must leave it alone.
#[tokio::test]
async fn stamping_the_version_claims_the_store() {
    let dir = lore_base::test_util::TempDir::new("ms_version_claim_");
    let epoch = dir.to_path_buf().join("mutable").join("epoch");

    let store = LocalMutableStore::new(
        Some(dir.to_path_buf()),
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    drop(store);

    let after_stamp = std::fs::read(&epoch).ok();
    assert!(
        after_stamp.is_some(),
        "creating the store stamped its version, which must be claimed and announced"
    );

    // Reopening reads a version it recognises, so there is nothing to stamp.
    let store = LocalMutableStore::new(
        Some(dir.to_path_buf()),
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    drop(store);

    assert_eq!(
        std::fs::read(&epoch).ok(),
        after_stamp,
        "an open that writes nothing must not invalidate every other process's state"
    );
}

/// **A store serves what another process wrote, not what it had cached.**
///
/// Two stores over one directory, which is what two processes are. The first
/// caches an empty bucket with a miss; the second stores the key and flushes. The
/// first must find it on its next operation, which it can only do by noticing the
/// epoch moved and dropping the bucket it held.
///
/// The only end-to-end exercise of [`MutableStoreGroup::invalidate`]: the
/// lock-interleave probe drives reads alone, so nothing there advances an epoch.
#[tokio::test]
async fn a_store_drops_what_another_one_changed_underneath_it() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_refresh_");
    let partition = Partition::default();
    let key = Hash::from_u64(0x5eed);
    let value = Hash::from_u64(0x1234);

    let reader: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    assert!(
        reader
            .clone()
            .load(partition, key, KeyType::BranchMetadata)
            .await
            .is_err(),
        "nothing has been written yet, and the miss is what caches the bucket"
    );

    {
        let writer: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                Some(dir.to_path_buf()),
                MutableStoreSettings::default(),
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        writer
            .clone()
            .store(partition, key, value, KeyType::BranchMetadata)
            .await
            .unwrap();
        writer.clone().flush(true).await.unwrap();
    }

    assert_eq!(
        reader
            .clone()
            .load(partition, key, KeyType::BranchMetadata)
            .await
            .unwrap(),
        value,
        "the cached empty bucket must have been dropped and re-read"
    );
}

/// **A delayed flush releases the store once it has written.** Its own write claim
/// counts as a writer in flight, so declaring the store clean while that claim is
/// alive is refused, and the flock would stay held until some later flush.
#[tokio::test]
async fn a_delayed_flush_releases_the_store() {
    let dir = lore_base::test_util::TempDir::new("ms_delayed_release_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            client_settings(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store_keys(&store, Partition::default(), 2).await;
    assert!(lock.is_held(), "unflushed writes hold the store");

    run_delayed_flush(&store).await;
    assert!(!lock.is_held(), "and the sweeps that wrote them release it");
}

/// **A write keeps the store claimed until it is flushed, and a flush with nothing
/// to write announces nothing.**
#[tokio::test]
async fn a_write_holds_the_store_until_flushed_and_an_idle_flush_is_silent() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_flush_claim_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store_keys(&store, Partition::default(), 1).await;
    assert!(lock.is_held(), "an unflushed write holds the store");
    store.clone().flush(true).await.unwrap();
    assert!(!lock.is_held(), "and the flush that settles it releases it");

    let epoch = dir.to_path_buf().join("mutable").join("epoch");
    let before = std::fs::read(&epoch).ok();
    store.clone().flush(true).await.unwrap();
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "nothing to write, nothing said"
    );
    assert!(!lock.is_held());
}

/// **A listing keeps its claim while its producers are still reading.** They go on
/// after the stream is handed back, so a claim released when `list` returns would
/// leave them reading bucket files no claim covers.
#[tokio::test]
async fn a_listing_keeps_the_store_claimed_while_it_reads() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_list_claim_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store_keys(&store, Partition::default(), 1).await;
    store.clone().flush(true).await.unwrap();
    assert!(!lock.is_held());

    // Holds the first bucket the group-0 producer reads, so it is still at work.
    let blocker = store.group[0].bucket(0).clone().write_owned().await;
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store
            .clone()
            .list(Partition::default(), KeyType::BranchMetadata),
    )
    .await
    .expect("a listing hands its stream back while its producers wait")
    .unwrap();
    assert!(lock.is_held(), "a listing still reading holds its claim");

    drop(blocker);
    drop(stream);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while lock.is_held() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the producers finished and released the claim"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// **A load claims to read and a compare-and-swap claims to write.** A load changes
/// nothing, so it announces nothing and keeps nothing claimed; a compare-and-swap
/// changes the store, so it announces the change and keeps the store claimed until
/// it is flushed.
#[tokio::test]
async fn a_load_announces_nothing_and_a_compare_and_swap_holds_the_store() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_load_swap_claims_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    store.clone().flush(true).await.unwrap();
    assert!(!lock.is_held());
    let epoch = dir.to_path_buf().join("mutable").join("epoch");
    let before = std::fs::read(&epoch).ok();

    let key = Hash::from([0x5au8; 32]);
    let _ = store
        .clone()
        .load(Partition::default(), key, KeyType::BranchMetadata)
        .await;
    assert_eq!(
        std::fs::read(&epoch).ok(),
        before,
        "a load announces nothing"
    );
    assert!(!lock.is_held(), "and keeps nothing claimed");

    store
        .clone()
        .compare_and_swap(
            Partition::default(),
            key,
            Hash::default(),
            Hash::from([0xa5u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .expect("swaps");
    assert_ne!(
        std::fs::read(&epoch).ok(),
        before,
        "a compare-and-swap announces its change"
    );
    assert!(
        lock.is_held(),
        "and keeps the store claimed until it is flushed"
    );

    store.clone().flush(true).await.unwrap();
    assert!(!lock.is_held(), "which releases it");
}

/// **A compare-and-swap to the value already stored flags nothing.** It succeeds, and
/// leaves the bucket unflagged: a flagged bucket is rewritten by the next flush and keeps
/// the store claimed until then, over a value that did not change — which is what
/// recording a branch tip that has not moved does.
#[tokio::test]
async fn a_compare_and_swap_to_the_stored_value_flags_nothing() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_cas_unchanged_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let key = Hash::from([0x5au8; 32]);
    let value = Hash::from([0xa5u8; 32]);
    store
        .clone()
        .store(Partition::default(), key, value, KeyType::BranchMetadata)
        .await
        .unwrap();
    store.clone().flush(true).await.unwrap();
    let (group_index, bucket_index) = populated_bucket(&store).await;
    let flagged = || store.group[group_index].dirty[bucket_index].load(atomic::Ordering::Relaxed);
    assert!(!flagged(), "the flush wrote the stored value");

    let previous = store
        .clone()
        .compare_and_swap(
            Partition::default(),
            key,
            value,
            value,
            KeyType::BranchMetadata,
        )
        .await
        .expect("swaps");
    assert_eq!(previous, value, "the swap succeeds");
    assert!(
        !flagged(),
        "and flags nothing for a value that did not change"
    );
}

/// **A claim to rewrite announces the rewrite and keeps nothing claimed.** The version
/// migration rewrites the store's files wholesale, so the epoch has to move before it
/// does; but nothing it writes goes through a bucket a flush would clear, so a dirty
/// mark would keep the store claimed for good.
#[tokio::test]
async fn a_claim_to_rewrite_announces_it_and_keeps_nothing_claimed() {
    let dir = lore_base::test_util::TempDir::new("ms_rewrite_claim_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    assert!(!lock.is_held());
    let epoch = dir.to_path_buf().join("mutable").join("epoch");
    let before = std::fs::read(&epoch).ok();

    let claim = store
        .hold_to_rewrite()
        .await
        .expect("claims")
        .expect("a disk-backed store claims something");
    assert_ne!(
        std::fs::read(&epoch).ok(),
        before,
        "the rewrite is announced before it happens"
    );
    drop(claim);
    assert!(!lock.is_held(), "and nothing is left for a flush to clear");
}

/// **A bucket flagged again after a sweep began schedules the next sweep.** The running
/// sweep may already have written that bucket, so leaving it to that sweep would leave
/// it flagged, keeping the store claimed with nothing coming to write it.
#[tokio::test]
async fn a_bucket_flagged_while_a_sweep_runs_schedules_the_next() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_sweep_schedule_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings {
                flush_delay_seconds: 60 * 60,
                ..Default::default()
            },
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let key = Hash::from([0x5au8; 32]);
    store
        .clone()
        .store(
            Partition::default(),
            key,
            Hash::from([0xa5u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .unwrap();
    let (group_index, bucket_index) = populated_bucket(&store).await;
    let group = store.group[group_index].clone();
    assert!(
        group.scheduled.load(atomic::Ordering::Relaxed),
        "the write scheduled a sweep"
    );

    // That sweep has woken and written the bucket, and is still running.
    group.scheduled.store(false, atomic::Ordering::Relaxed);
    group.dirty[bucket_index].store(false, atomic::Ordering::Relaxed);
    let running = group.flush.lock().await.len();

    store
        .clone()
        .store(
            Partition::default(),
            key,
            Hash::from([0xb6u8; 32]),
            KeyType::BranchMetadata,
        )
        .await
        .unwrap();
    assert_eq!(
        group.flush.lock().await.len(),
        running + 1,
        "a write during a running sweep schedules another"
    );
}

/// **An untyped listing takes no claim.** It lists nothing, so it has nothing to hold
/// the store for, and must not wait for another process using it.
#[tokio::test]
async fn an_untyped_listing_takes_no_claim() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_list_untyped_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            MutableStoreSettings::default(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    // Another process's claim: a second description of the lock file excludes this
    // process's own.
    let elsewhere = lore_base::fs::lock::FSLock::acquire_exact_path(
        &dir.to_path_buf().join("mutable").join("lock"),
    )
    .await
    .expect("takes the store's lock");

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.clone().list(Partition::default(), KeyType::Untyped),
    )
    .await
    .expect("an untyped listing does not wait for the store")
    .unwrap();
    drop(elsewhere);
}

/// **A change that fails to reach disk keeps its flag and the store's claim**, whether
/// the write was synced or not. The flag is cleared as the write begins, so a failure
/// that left it clear would never write the change, and a flush scan would find the
/// store clean and release its flock over state that is only in memory.
#[tokio::test]
async fn a_change_that_fails_to_reach_disk_keeps_its_flag_and_the_claim() {
    use lore_storage::mutable_store::MutableStore;

    for sync_data in [false, true] {
        let dir = lore_base::test_util::TempDir::new("ms_failed_change_");
        let store = Arc::new(
            LocalMutableStore::new(
                Some(dir.to_path_buf()),
                MutableStoreSettings::default(),
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );
        let lock = store.lock.clone().expect("a disk-backed store has a lock");
        let key = Hash::from([0x5au8; 32]);
        // Written once, so the group's level is committed and the write below takes the
        // regular path to the bucket's own file.
        store
            .clone()
            .store(
                Partition::default(),
                key,
                Hash::from([0xa5u8; 32]),
                KeyType::BranchMetadata,
            )
            .await
            .unwrap();
        store.clone().flush(true).await.unwrap();
        assert!(!lock.is_held());

        let (group_index, bucket_index) = populated_bucket(&store).await;
        let root = store.path.clone().expect("a disk-backed store has a path");
        let file = format_bucket_path(&root, group_index, bucket_index);
        std::fs::remove_file(&file).expect("the bucket was written");
        std::fs::create_dir_all(file.join("occupied"))
            .expect("a directory where the bucket file goes");

        // A change, made the way every change is: under a write claim.
        drop(store.hold(Intent::Write).await.unwrap());
        let group = store.group[group_index].clone();
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

        assert!(
            store.clone().flush(sync_data).await.is_err(),
            "sync_data {sync_data}: the bucket cannot be written"
        );
        assert!(
            group.dirty[bucket_index].load(atomic::Ordering::Relaxed),
            "sync_data {sync_data}: the change keeps its flag, to be written again"
        );
        assert!(
            lock.is_held(),
            "sync_data {sync_data}: and the store stays claimed while it is not on disk"
        );
    }
}

/// The group and bucket index of the first bucket in `store` holding an entry.
async fn populated_bucket(store: &LocalMutableStore) -> (usize, usize) {
    for (group_index, group) in store.group.iter().enumerate() {
        for bucket_index in 0..group.bucket_count.load(atomic::Ordering::Relaxed) {
            if !group.bucket(bucket_index).read().await.entry.is_empty() {
                return (group_index, bucket_index);
            }
        }
    }
    panic!("a store populates a bucket");
}

/// **A flush settles a bucket it has nothing to write for**, so an emptied bucket
/// above the committed level does not keep the store claimed.
#[tokio::test]
async fn a_flush_settles_an_emptied_bucket_it_has_nothing_to_write_for() {
    use lore_storage::mutable_store::MutableStore;

    let dir = lore_base::test_util::TempDir::new("ms_empty_settle_");
    let store = Arc::new(
        LocalMutableStore::new(
            Some(dir.to_path_buf()),
            client_settings(),
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let lock = store.lock.clone().expect("a disk-backed store has a lock");
    let _ = store.group[0].bucket(0);
    store.group[0].dirty[0].store(true, atomic::Ordering::Relaxed);

    store.clone().flush(true).await.unwrap();
    assert!(
        !store.group[0].dirty[0].load(atomic::Ordering::Relaxed),
        "the flag was settled"
    );
    assert!(!lock.is_held(), "so nothing keeps the store claimed");
}

/// **A reload takes the levels a fresh survey read and drops every flag**, for the
/// reason the immutable store's test gives.
#[tokio::test]
async fn invalidating_a_group_takes_the_surveyed_levels_and_clears_its_flags() {
    let store = LocalMutableStore::new(
        None::<std::path::PathBuf>,
        MutableStoreSettings::default(),
        make_in_memory_immutable().await,
    )
    .await
    .unwrap();
    let group = store.group[0].clone();
    let _ = group.bucket(3);
    group.dirty[3].store(true, atomic::Ordering::Relaxed);

    group.invalidate(64, 32, 7).await;

    assert!(
        !group.dirty[3].load(atomic::Ordering::Relaxed),
        "no flag survives a reload"
    );
    assert_eq!(group.bucket_count.load(atomic::Ordering::Relaxed), 64);
    assert_eq!(group.committed_level.load(atomic::Ordering::Relaxed), 32);
    assert_eq!(group.serialize_version.load(atomic::Ordering::Relaxed), 7);
}

#[tokio::test]
async fn level_1_store_and_load_round_trip() {
    use lore_storage::mutable_store::MutableStore;
    let store: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            None::<&Path>,
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            make_in_memory_immutable().await,
        )
        .await
        .unwrap(),
    );
    let partition = Partition::default();
    let mut key = Hash::default();
    // Set bytes that, at level 256, would route to bucket 0xAB; at level 1 must still route to bucket 0.
    key.data_mut()[0] = 0x10;
    key.data_mut()[1] = 0xAB;
    let value = Hash::from_u64(42);
    store
        .clone()
        .store(partition, key, value, KeyType::BranchMetadata)
        .await
        .unwrap();
    let loaded = store
        .clone()
        .load(partition, key, KeyType::BranchMetadata)
        .await
        .unwrap();
    assert_eq!(loaded, value);
}

/// At fan-out levels < 256 a single bucket holds entries spanning several bucket-byte
/// (`data[1]`) values, so within the bucket the full-hash sort orders entries primarily
/// by bucket byte and only secondarily by `data[2]` (the key-type byte). A binary
/// search that compares only `data[2]` can land on an entry whose bucket byte differs
/// from the target's and erroneously conclude no match exists, missing entries that
/// are actually present. The fix carves the bucket's `sorted_index` into one slice per
/// bucket-byte value before running the per-slice key-type search. This regression
/// test inserts one `Instance` and one `BranchMetadata` entry into the same bucket
/// at each level in the ladder and verifies `list(Instance)` returns the `Instance`
/// entry; phase two adds two more `Instance` entries plus a mix of filler entries
/// across the bucket's bucket-byte range and verifies all three `Instance` entries
/// are enumerated.
#[tokio::test]
async fn list_finds_typed_entries_at_each_fan_out_level() {
    use futures::StreamExt;
    use lore_storage::mutable_store::MutableStore;

    for &level in &[1usize, 32, 64, 128, 256] {
        let store: Arc<dyn MutableStore> = Arc::new(
            LocalMutableStore::new(
                None::<&Path>,
                MutableStoreSettings {
                    initial_fan_out_level: level,
                    ..Default::default()
                },
                make_in_memory_immutable().await,
            )
            .await
            .unwrap(),
        );

        let partition = Partition::default();
        let stride = 256 / level;

        // Phase 1: insert one Instance and one BranchMetadata in the same bucket and
        // verify list(Instance) finds the Instance. The simple two-entry shape is the
        // original failing case from the test_background_prune_during_clone smoke
        // flake.
        let d1_inst1 = 0u8;
        let d1_meta = if stride >= 2 { 1u8 } else { 0u8 };

        let mut k_inst1 = Hash::default();
        k_inst1.data_mut()[0] = 0x42;
        k_inst1.data_mut()[1] = d1_inst1;

        let mut k_meta = Hash::default();
        k_meta.data_mut()[0] = 0x42;
        k_meta.data_mut()[1] = d1_meta;
        if d1_inst1 == d1_meta {
            k_meta.data_mut()[3] = 1;
        }

        let v_inst1 = Hash::from_u64(1);
        let v_meta = Hash::from_u64(2);
        store
            .clone()
            .store(partition, k_inst1, v_inst1, KeyType::Instance)
            .await
            .unwrap();
        store
            .clone()
            .store(partition, k_meta, v_meta, KeyType::BranchMetadata)
            .await
            .unwrap();

        let mut stream = store
            .clone()
            .list(partition, KeyType::Instance)
            .await
            .unwrap();
        let mut found_phase1: Vec<(Hash, Hash)> = Vec::new();
        while let Some(item) = stream.next().await {
            found_phase1.push(item);
        }
        assert_eq!(
            found_phase1.len(),
            1,
            "level {level} phase 1: list(Instance) returned {} entries, expected 1",
            found_phase1.len()
        );
        assert_eq!(
            found_phase1[0].1, v_inst1,
            "level {level} phase 1: wrong value returned"
        );

        // Phase 2: insert two more Instance entries plus a mix of non-Instance entries
        // — all into the same bucket. At fan-out levels < 256 the entries take distinct
        // bucket-byte values within the single bucket's range, exercising the per-slice
        // walk over scattered Instance entries. At level 256 only one bucket-byte value
        // routes to a given bucket, so the two extras share `data[1]` with the first
        // and are differentiated via `data[5]`; this exercises the within-bucket
        // `stride == 1` fast path with multiple Instance entries packed together.
        let (d1_inst2, d1_inst3) = if stride >= 2 {
            ((stride / 2) as u8, (stride - 1) as u8)
        } else {
            (0u8, 0u8)
        };

        let mut k_inst2 = Hash::default();
        k_inst2.data_mut()[0] = 0x42;
        k_inst2.data_mut()[1] = d1_inst2;
        k_inst2.data_mut()[5] = 1;

        let mut k_inst3 = Hash::default();
        k_inst3.data_mut()[0] = 0x42;
        k_inst3.data_mut()[1] = d1_inst3;
        k_inst3.data_mut()[5] = 2;

        let v_inst2 = Hash::from_u64(11);
        let v_inst3 = Hash::from_u64(12);
        store
            .clone()
            .store(partition, k_inst2, v_inst2, KeyType::Instance)
            .await
            .unwrap();
        store
            .clone()
            .store(partition, k_inst3, v_inst3, KeyType::Instance)
            .await
            .unwrap();

        let other_kts = [
            KeyType::BranchMetadata,
            KeyType::BranchId,
            KeyType::BranchLatestPointer,
            KeyType::RepositoryMetadata,
            KeyType::RepositoryId,
        ];
        let filler_d1_max = stride.min(8);
        let mut counter: u64 = 100;
        for d1_idx in 0..filler_d1_max {
            let d1 = d1_idx as u8;
            for &kt in &other_kts {
                let mut k = Hash::default();
                k.data_mut()[0] = 0x42;
                k.data_mut()[1] = d1;
                k.data_mut()[6] = (counter & 0xff) as u8;
                k.data_mut()[7] = ((counter >> 8) & 0xff) as u8;
                store
                    .clone()
                    .store(partition, k, Hash::from_u64(counter), kt)
                    .await
                    .unwrap();
                counter += 1;
            }
        }

        // Phase 3: list(Instance) must return all three Instance entries despite the
        // filler entries scattered through the bucket.
        let mut stream = store
            .clone()
            .list(partition, KeyType::Instance)
            .await
            .unwrap();
        let mut found_phase3: Vec<Hash> = Vec::new();
        while let Some((_k, v)) = stream.next().await {
            found_phase3.push(v);
        }
        found_phase3.sort();
        let mut expected = vec![v_inst1, v_inst2, v_inst3];
        expected.sort();
        assert_eq!(
            found_phase3, expected,
            "level {level} phase 3: expected three Instance entries, got {found_phase3:?}"
        );

        // Phase 4 (fan-out levels > 1 only): populate two additional buckets — bucket 5
        // and bucket 10 — each with one Instance entry plus filler entries spanning the
        // full bucket-byte sub-range of that bucket. This exercises cross-bucket
        // enumeration AND the per-slice walk inside each non-zero bucket: at fan-out
        // levels < 256 the new buckets each hold entries with `stride` distinct
        // bucket-byte values, so finding the Instance still requires walking past
        // non-matching slices. Skipped at level 1 because only bucket 0 exists.
        if level > 1 {
            let extra_buckets = [5usize, 10usize];
            let mut extra_inst_values: Vec<Hash> = Vec::new();
            for (next_inst_value, &bucket_idx) in (13u64..).zip(extra_buckets.iter()) {
                let d1_lo = bucket_idx * stride;
                let d1_hi = d1_lo + stride;

                let mut k_inst_extra = Hash::default();
                k_inst_extra.data_mut()[0] = 0x42;
                k_inst_extra.data_mut()[1] = d1_lo as u8;
                let v_inst_extra = Hash::from_u64(next_inst_value);
                store
                    .clone()
                    .store(partition, k_inst_extra, v_inst_extra, KeyType::Instance)
                    .await
                    .unwrap();
                extra_inst_values.push(v_inst_extra);

                for d1_value in d1_lo..d1_hi {
                    for &kt in &other_kts {
                        let mut k = Hash::default();
                        k.data_mut()[0] = 0x42;
                        k.data_mut()[1] = d1_value as u8;
                        k.data_mut()[6] = (counter & 0xff) as u8;
                        k.data_mut()[7] = ((counter >> 8) & 0xff) as u8;
                        store
                            .clone()
                            .store(partition, k, Hash::from_u64(counter), kt)
                            .await
                            .unwrap();
                        counter += 1;
                    }
                }
            }

            let mut stream = store
                .clone()
                .list(partition, KeyType::Instance)
                .await
                .unwrap();
            let mut found_phase4: Vec<Hash> = Vec::new();
            while let Some((_k, v)) = stream.next().await {
                found_phase4.push(v);
            }
            found_phase4.sort();
            let mut expected = vec![v_inst1, v_inst2, v_inst3];
            expected.extend(extra_inst_values);
            expected.sort();
            assert_eq!(
                found_phase4,
                expected,
                "level {level} phase 4: expected {} Instance entries across multiple \
                 buckets, got {found_phase4:?}",
                expected.len()
            );
        }
    }
}
