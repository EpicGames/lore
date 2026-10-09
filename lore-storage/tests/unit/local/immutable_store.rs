// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;

use bytes::Bytes;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentFlags;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::hash;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::StoreError;
use lore_storage::local::immutable_store::info::info_path_for_store_root;
use lore_storage::store_types::StoreMatch;
use lore_storage::store_types::StoreMatchResult;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

mod info;
#[cfg(feature = "oodle")]
mod oodle_migration;

use lore_storage::local::immutable_store::*;

fn write_bucket_file(path: &Path, version: u32) {
    let entry = ImmutableStoreEntry::default();
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = version;
    header.count = 1;
    let mut bytes = Vec::with_capacity(
        size_of::<ImmutableStoreHeader>() + 4 + size_of::<ImmutableStoreEntry>(),
    );
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(entry.as_bytes());
    std::fs::write(path, bytes).unwrap();
}

/// A bucket larger than [`BUCKET_HEAD_READ`] does not fit the head the composite open
/// returns, so it is loaded by scattering one vectored read straight into the chunk
/// allocations of both `GrowVec`s. Entry contents are distinct per index, so a scatter that
/// landed a chunk at the wrong offset would show up as swapped entries rather than as a
/// length mismatch.
#[tokio::test]
async fn deserialize_scatters_a_bucket_larger_than_the_head_read() {
    let dir = lore_base::test_util::TempDir::new("is_scatter_");
    let path = dir.path().join("bucket");

    let per_entry = size_of::<u32>() + size_of::<ImmutableStoreEntry>();
    let count = (BUCKET_HEAD_READ / per_entry) + 64;
    assert!(
        size_of::<ImmutableStoreHeader>() + count * per_entry > BUCKET_HEAD_READ,
        "the bucket has to exceed the head read for this to test anything"
    );

    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = count as u32;

    let mut bytes = Vec::with_capacity(size_of::<ImmutableStoreHeader>() + count * per_entry);
    bytes.extend_from_slice(header.as_bytes());
    for index in 0..count {
        bytes.extend_from_slice(&(index as u32).to_le_bytes());
    }
    for index in 0..count {
        let mut entry = ImmutableStoreEntry::default();
        entry.address.hash = Hash::from([index as u8; 32]);
        entry.data.size_content = index as u64;
        entry.data.pack_offset = index as u32;
        bytes.extend_from_slice(entry.as_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    let (sorted_index, entry, _upgrade, _dirty) =
        ImmutableStoreBucket::deserialize_files(path).await.unwrap();

    assert_eq!(sorted_index.len(), count);
    assert_eq!(entry.len(), count);
    for index in 0..count {
        assert_eq!(sorted_index[index], index as u32, "sorted index at {index}");
        assert_eq!(
            entry[index].address.hash,
            Hash::from([index as u8; 32]),
            "entry hash at {index}"
        );
        assert_eq!(entry[index].data.size_content, index as u64);
        assert_eq!(entry[index].data.pack_offset, index as u32);
    }
}

/// Client-shaped settings: the ones a repository store is opened with.
fn client_settings() -> ImmutableStoreSettings {
    ImmutableStoreSettings {
        protect_local_fragment: true,
        implicit_durable_stored: false,
        ..Default::default()
    }
}

/// A client store over `dir` flushing after no delay, in the background as `flush_background`
/// says.
async fn open_flushing(dir: &Path, flush_background: bool) -> Arc<LocalImmutableStore> {
    LocalImmutableStore::new(
        Some(dir.to_path_buf()),
        ImmutableStoreSettings {
            flush_background,
            flush_delay_seconds: 0,
            ..client_settings()
        },
    )
    .await
    .expect("store opens")
}

/// Whether a delayed flush was pending for `address`'s group, having awaited it.
async fn awaited_flush(store: &LocalImmutableStore, address: Address) -> bool {
    store.group[address.hash.data()[0] as usize]
        .flush
        .lock()
        .await
        .join_next()
        .await
        .is_some()
}

/// A fragment of `payload` stored with `flags`.
fn stored_fragment(payload: &Bytes, flags: FragmentFlags) -> Fragment {
    Fragment {
        flags: flags.as_u32(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    }
}

/// A durable write schedules the delayed flush only with `flush_background`, so only then does
/// its entry reach the on-disk index without an explicit flush.
#[tokio::test]
async fn a_durable_write_reaches_the_index_only_with_background_flush() {
    let partition = Partition::from([9u8; 16]);
    let (address, payload) = payload_off_bucket_zero(0);
    let fragment = stored_fragment(&payload, FragmentFlags::PayloadStoredDurable);
    for flush_background in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_durable_flush_");
        {
            let store = open_flushing(dir.path(), flush_background).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store
                .put(partition, address, fragment, Some(payload.clone()), false)
                .await
                .expect("put succeeds");
            assert_eq!(awaited_flush(&store, address).await, flush_background);
        }
        let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> =
            open_flushing(dir.path(), flush_background).await;
        assert_eq!(
            store.get(partition, address).await.is_ok(),
            flush_background,
            "flush_background {flush_background}"
        );
    }
}

/// Marking a flushed local entry durable, without a payload to store, reaches the on-disk index
/// in the background only with `flush_background`.
#[tokio::test]
async fn a_durable_mark_on_a_local_entry_reaches_the_index_only_with_background_flush() {
    let partition = Partition::from([9u8; 16]);
    let (address, payload) = payload_off_bucket_zero(0);
    for flush_background in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_durable_mark_");
        {
            let store = open_flushing(dir.path(), flush_background).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store
                .clone()
                .put(
                    partition,
                    address,
                    stored_fragment(&payload, FragmentFlags::PayloadStoredLocal),
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("local put succeeds");
            assert!(awaited_flush(&store, address).await);
            dyn_store
                .put(
                    partition,
                    address,
                    stored_fragment(&payload, FragmentFlags::PayloadStoredDurable),
                    None,
                    false,
                )
                .await
                .expect("durable mark succeeds");
            assert_eq!(awaited_flush(&store, address).await, flush_background);
        }
        let found = open_flushing(dir.path(), flush_background)
            .await
            .find(partition, address)
            .await
            .expect("the local entry was flushed");
        assert_eq!(
            found.data.flags & FragmentFlags::PayloadStoredDurable
                == FragmentFlags::PayloadStoredDurable,
            flush_background,
            "flush_background {flush_background}"
        );
    }
}

/// A durable copy into another partition reaches the on-disk index in the background only with
/// `flush_background`.
#[tokio::test]
async fn a_durable_copy_reaches_the_index_only_with_background_flush() {
    let source = Partition::from([9u8; 16]);
    let destination = Partition::from([10u8; 16]);
    let (address, payload) = payload_off_bucket_zero(0);
    for flush_background in [false, true] {
        let dir = lore_base::test_util::TempDir::new("is_durable_copy_");
        {
            let store = open_flushing(dir.path(), flush_background).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store
                .clone()
                .put(
                    source,
                    address,
                    stored_fragment(&payload, FragmentFlags::PayloadStoredLocal),
                    Some(payload.clone()),
                    false,
                )
                .await
                .expect("local put succeeds");
            assert!(awaited_flush(&store, address).await);
            dyn_store
                .copy(
                    source,
                    address,
                    destination,
                    address.context,
                    CopyBehavior {
                        durable: true,
                        do_not_replicate: false,
                    },
                )
                .await
                .expect("copy succeeds");
            assert_eq!(awaited_flush(&store, address).await, flush_background);
        }
        let found = open_flushing(dir.path(), flush_background)
            .await
            .find(destination, address)
            .await
            .expect("the source entry was flushed");
        assert_eq!(
            found.partition == destination,
            flush_background,
            "flush_background {flush_background}"
        );
    }
}

/// Server-shaped settings: groups start at the full 256 buckets.
fn server_settings() -> ImmutableStoreSettings {
    ImmutableStoreSettings {
        protect_local_fragment: false,
        implicit_durable_stored: true,
        initial_fan_out_level: BUCKET_COUNT,
        ..Default::default()
    }
}

/// Store `count` fragments and return their addresses.
async fn put_fragments(
    store: &Arc<LocalImmutableStore>,
    partition: Partition,
    count: u16,
) -> Vec<Address> {
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    let mut addresses = Vec::new();
    for seed in 0..count {
        let (address, payload) = payload_off_bucket_zero(seed);
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        dyn_store
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("put succeeds");
        addresses.push(address);
    }
    addresses
}

/// Run the background timer's flush for every group, and nothing else.
async fn run_delayed_flush(store: &Arc<LocalImmutableStore>) {
    let weak = Arc::downgrade(store);
    for group_index in 0..GROUP_COUNT {
        ImmutableStoreGroup::flush_delayed(weak.clone(), group_index, 0).await;
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

/// Content whose hash puts it in a bucket other than 0 once a group is at level 256, so a
/// group misread as pre-fan-out sends a lookup to a different bucket than the one holding it.
fn payload_off_bucket_zero(seed: u16) -> (Address, Bytes) {
    for salt in 0..1024u16 {
        let payload = Bytes::from(seed.to_le_bytes().repeat(32 + salt as usize));
        let hash = hash::hash_slice(payload.as_ref());
        if hash.data()[1] != 0 {
            return (
                Address {
                    hash,
                    context: Context::from([0u8; 16]),
                },
                payload,
            );
        }
    }
    panic!("no payload hashed away from bucket 0");
}

/// The delayed background flush writes bucket files without going through the two-phase
/// commit, and clears the dirty flags as it goes — so the flush that ends the command finds
/// the group clean and skips it. Left like that, the group has committed bucket files and no
/// level marker, which [`crate::local::fan_out::read_group_level`] reads back as a legacy
/// pre-fan-out layout at 256 buckets. A client group is written at level 1, where everything
/// lands in `index_00`; at 256 the hash maps somewhere else entirely and that file is never
/// opened again, so the entry is on disk and unreachable — `Address not found` with the
/// payload still sitting in the packstore.
#[tokio::test]
async fn a_group_the_delayed_flush_persisted_reopens_at_its_written_level() {
    let dir = lore_base::test_util::TempDir::new("is_delayed_level_");
    let partition = Partition::from([7u8; 16]);

    let addresses = {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            1,
            "a client store starts its groups at level 1"
        );

        let addresses = put_fragments(&store, partition, 32).await;
        // Persist exactly the way the background timer does, and nothing else: no
        // `flush`, so the two-phase commit that would write the markers never runs.
        run_delayed_flush(&store).await;
        addresses
    };

    let index_root = dir.path().join("immutable").join("index");
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

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
        .await
        .expect("store reopens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    for address in &addresses {
        let group = &store.group[address.hash.data()[0] as usize];
        assert_eq!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            1,
            "group for {address} reopened at the wrong level"
        );
        dyn_store
            .clone()
            .get(partition, *address)
            .await
            .unwrap_or_else(|err| {
                panic!("{address} was written and persisted but reads back as {err:?}")
            });
    }
}

mod oodle_migration_seed {
    use super::*;

    /// A store with fragments on disk and no info file, as one written by a binary predating
    /// the file. Reports the highest group index holding data.
    async fn store_predating_the_info_file(dir: &lore_base::test_util::TempDir) -> i32 {
        let highest = {
            let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
                .await
                .expect("store opens");
            let addresses = put_fragments(&store, Partition::from([7u8; 16]), 32).await;
            let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
            dyn_store.flush(true).await.expect("store flushes");
            addresses
                .iter()
                .map(|address| address.hash.data()[0] as i32)
                .max()
                .expect("fragments were written")
        };
        std::fs::remove_file(info_path_for_store_root(&dir.path().join("immutable")))
            .expect("the info file a newer binary wrote is removed");
        highest
    }

    /// A group that was never written holds no payload of any codec, so a store made only of
    /// them starts with nothing to migrate - and says so rather than sweeping all 256.
    #[tokio::test]
    async fn a_store_with_no_written_group_has_nothing_to_migrate() {
        let dir = lore_base::test_util::TempDir::new("is_seed_fresh_");
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");

        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1
        );
    }

    /// A store written before the info file existed is seeded from the groups actually on
    /// disk, so the pass starts at the highest one holding data.
    #[cfg(feature = "oodle")]
    #[tokio::test]
    async fn a_store_predating_the_info_file_is_seeded_from_its_written_groups() {
        let dir = lore_base::test_util::TempDir::new("is_seed_existing_");
        let highest = store_predating_the_info_file(&dir).await;

        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store reopens");
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            highest
        );
    }

    /// A binary that cannot decode Oodle can do nothing about a store that holds it, so it
    /// records no work rather than a bookmark it could never act on.
    #[cfg(not(feature = "oodle"))]
    #[tokio::test]
    async fn a_binary_without_oodle_records_no_migration() {
        let dir = lore_base::test_util::TempDir::new("is_seed_no_oodle_");
        store_predating_the_info_file(&dir).await;

        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store reopens");
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1
        );
    }
}

/// A store written before lazy fan-out carries no level markers and a flat 256-bucket
/// layout, which `read_group_level` reports as `PreFanOut`. Such a group already reads back
/// at the level it was written at, so nothing may start writing markers into it: that would
/// change which flush path the next flush takes on a store this version has to leave alone.
#[tokio::test]
async fn a_pre_fan_out_store_gains_no_level_markers() {
    let dir = lore_base::test_util::TempDir::new("is_prefanout_");
    let partition = Partition::from([3u8; 16]);
    let index_root = dir.path().join("immutable").join("index");

    // A group as the previous format left it: one bucket file at the pre-fan-out version,
    // no marker beside it.
    let legacy_group = index_root.join("2a");
    std::fs::create_dir_all(&legacy_group).expect("group dir");
    write_bucket_file(
        &legacy_group.join("index_10"),
        ImmutableStoreVersion::LastAccessInEntry as u32,
    );

    let addresses = {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");
        assert_eq!(
            store.group[0x2a]
                .bucket_count
                .load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "a marker-less group holding bucket files opens pre-fan-out"
        );
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "a legacy store keeps even its fresh groups at the flat layout"
        );

        let addresses = put_fragments(&store, partition, 32).await;
        run_delayed_flush(&store).await;
        addresses
    };

    assert_eq!(
        level_markers(&index_root),
        Vec::<PathBuf>::new(),
        "a pre-fan-out store must come back out with the layout it went in with"
    );

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
        .await
        .expect("store reopens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    for address in &addresses {
        let group = &store.group[address.hash.data()[0] as usize];
        assert_eq!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "group for {address} reopened at the wrong level"
        );
        dyn_store
            .clone()
            .get(partition, *address)
            .await
            .unwrap_or_else(|err| panic!("{address} reads back as {err:?}"));
    }
}

/// A server store starts its groups at the full 256 buckets, where a missing marker is read
/// back as exactly that. It gains no markers either, for the same reason.
#[tokio::test]
async fn a_server_shaped_store_gains_no_level_markers() {
    let dir = lore_base::test_util::TempDir::new("is_server_level_");
    let partition = Partition::from([9u8; 16]);
    let index_root = dir.path().join("immutable").join("index");

    let addresses = {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), server_settings())
            .await
            .expect("store opens");
        assert_eq!(
            store.group[0].bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "a server store starts at the flat layout"
        );

        let addresses = put_fragments(&store, partition, 32).await;
        run_delayed_flush(&store).await;
        addresses
    };

    assert_eq!(
        level_markers(&index_root),
        Vec::<PathBuf>::new(),
        "a group already at 256 reads back at 256 without a marker"
    );

    let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), server_settings())
        .await
        .expect("store reopens");
    let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
    for address in &addresses {
        let group = &store.group[address.hash.data()[0] as usize];
        assert_eq!(
            group.bucket_count.load(atomic::Ordering::Relaxed),
            BUCKET_COUNT,
            "group for {address} reopened at the wrong level"
        );
        dyn_store
            .clone()
            .get(partition, *address)
            .await
            .unwrap_or_else(|err| panic!("{address} reads back as {err:?}"));
    }
}

/// A group that already carries a marker keeps the level it records: the initial-level write
/// is for groups that have never had one, and must not overwrite a committed level.
#[tokio::test]
async fn a_marked_group_keeps_the_level_it_recorded() {
    let dir = lore_base::test_util::TempDir::new("is_marked_level_");
    let partition = Partition::from([5u8; 16]);
    let index_root = dir.path().join("immutable").join("index");

    {
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store opens");
        let dyn_store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = store.clone();
        let _ = put_fragments(&store, partition, 8).await;
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
        let store = LocalImmutableStore::new(Some(dir.path().to_path_buf()), client_settings())
            .await
            .expect("store reopens");
        let _ = put_fragments(&store, partition, 16).await;
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
fn lazy_fan_out_version_is_five() {
    assert_eq!(ImmutableStoreVersion::LazyFanOut as u32, 5);
}

#[test]
fn format_bucket_path_is_index_group_bucket() {
    let root = Path::new("/store");
    for index in [0usize, 0xab, 255] {
        let byte = index as u8;
        assert_eq!(
            format_bucket_path(root, index, index),
            root.join("index").join(format!("{byte:02x}")).join(format!(
                "{}{byte:02x}",
                lore_storage::local::fan_out::BUCKET_FILENAME_PREFIX
            ))
        );
    }
    assert_eq!(
        format_bucket_path(root, 0x0f, 0xf0),
        Path::new("/store/index/0f/index_f0")
    );
}

#[tokio::test]
async fn deserialize_accepts_last_access_in_entry_v4() {
    let dir = lore_base::test_util::TempDir::new("is_v4_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, ImmutableStoreVersion::LastAccessInEntry as u32);
    let result = ImmutableStoreBucket::deserialize_files(path).await;
    assert!(
        result.is_ok(),
        "v4 (LastAccessInEntry) bucket should deserialize"
    );
}

#[tokio::test]
async fn deserialize_accepts_lazy_fan_out_v5() {
    let dir = lore_base::test_util::TempDir::new("is_v5_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, ImmutableStoreVersion::LazyFanOut as u32);
    let result = ImmutableStoreBucket::deserialize_files(path).await;
    assert!(result.is_ok(), "v5 (LazyFanOut) bucket should deserialize");
}

#[tokio::test]
async fn deserialize_rejects_unknown_future_version() {
    let dir = lore_base::test_util::TempDir::new("is_v100_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, 100);
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    assert!(result.is_err(), "v100 bucket should be rejected as too new");
    // Future-version files MUST be preserved on disk — recovery would clobber data
    // written by a newer binary.
    assert!(
        path.exists(),
        "future-version bucket file must be preserved, not deleted"
    );
}

/// Write a v5 bucket file whose header claims `header_count` entries but contains
/// only `actual_entries_on_disk` entry slots — the crash-mid-flush shape.
fn write_bucket_file_with_count_mismatch(
    path: &Path,
    header_count: u32,
    actual_entries_on_disk: u32,
) {
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = header_count;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(header.as_bytes());
    for i in 0..actual_entries_on_disk {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    for _ in 0..actual_entries_on_disk {
        let entry = ImmutableStoreEntry::default();
        bytes.extend_from_slice(entry.as_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn deserialize_recovers_from_bad_count_header() {
    // Mirrors the production server log shape (header.count > what file fits).
    let dir = lore_base::test_util::TempDir::new("is_badcount_");
    let path = dir.path().join("bucket");
    write_bucket_file_with_count_mismatch(&path, 670, 518);
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let (sorted_index, entry, _, mark_dirty) =
        result.expect("count-mismatch corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!mark_dirty);
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn deserialize_recovers_from_invalid_version() {
    // 0xFFFF is above the future-version sentinel range, so it's corruption.
    let dir = lore_base::test_util::TempDir::new("is_badver_");
    let path = dir.path().join("bucket");
    write_bucket_file(&path, 0xFFFF);
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let (sorted_index, entry, _, _) = result.expect("invalid-version corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn deserialize_recovers_from_truncated_entries() {
    let dir = lore_base::test_util::TempDir::new("is_trunc_");
    let path = dir.path().join("bucket");
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = 3;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(header.as_bytes());
    for i in 0u32..3 {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    let entry = ImmutableStoreEntry::default();
    bytes.extend_from_slice(entry.as_bytes());
    bytes.extend_from_slice(&entry.as_bytes()[..size_of::<ImmutableStoreEntry>() / 2]);
    std::fs::write(&path, bytes).unwrap();

    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let (sorted_index, entry, _, _) = result.expect("truncated-entries corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn deserialize_recovers_from_short_header() {
    // File too small to even hold the header.
    let dir = lore_base::test_util::TempDir::new("is_shorthdr_");
    let path = dir.path().join("bucket");
    std::fs::write(&path, [0u8; 4]).unwrap();
    let result = ImmutableStoreBucket::deserialize_files(path.clone()).await;
    let (sorted_index, entry, _, _) = result.expect("short-header corruption must recover");
    assert!(sorted_index.is_empty());
    assert!(entry.is_empty());
    assert!(!path.exists(), "corrupt bucket file must be removed");
}

#[tokio::test]
async fn store_recovers_from_corrupt_bucket_and_remains_usable() {
    // End-to-end: corrupt a bucket file and verify the store is still usable for
    // writes and reads on that bucket. Original content is lost (expected).
    use lore_storage::options::ReadOptions;
    use lore_storage::options::WriteOptions;
    use lore_storage::read::read;
    use lore_storage::write::StoreResult;
    use lore_storage::write::write_content;

    let dir = lore_base::test_util::TempDir::new("is_e2e_recover_");
    let store_path = dir.path().to_path_buf();
    let partition = Partition::from([0x42u8; 16]);
    let context = Context::from([0x07u8; 16]);
    let payload = Bytes::from(vec![0xCDu8; 256]);

    let address = {
        let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
            Some(&store_path),
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let StoreResult { address, .. } = write_content(
            store.clone(),
            partition,
            context,
            payload.clone(),
            WriteOptions::default(),
            None,
            lore_storage::write_tracker::WriteContext::none(),
            None,
        )
        .await
        .unwrap();

        store.clone().flush(true).await.unwrap();
        address
    };

    // initial_fan_out_level=1 → bucket index is always 0; group is hash[0].
    let group_index = address.hash.data()[0] as usize;
    let bucket_path = store_path
        .join("immutable")
        .join("index")
        .join(format!("{group_index:02x}"))
        .join("index_00");
    assert!(
        bucket_path.exists(),
        "bucket file should exist after flush at {bucket_path:?}"
    );

    // Crash-mid-flush shape: header claims N entries, body is short.
    let mut header = ImmutableStoreHeader::new_zeroed();
    header.version = ImmutableStoreVersion::LazyFanOut as u32;
    header.count = 4096;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&[0u8; size_of::<u32>() * 4]);
    std::fs::write(&bucket_path, bytes).unwrap();

    let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
        Some(&store_path),
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Original content is gone (data lost), but the bucket is operational.
    let read_result = read(
        store.clone(),
        partition,
        address,
        None,
        ReadOptions::default(),
        None,
    )
    .await;
    assert!(
        read_result.is_err(),
        "originally stored content must be reported missing after recovery"
    );

    let StoreResult {
        address: new_address,
        ..
    } = write_content(
        store.clone(),
        partition,
        context,
        payload.clone(),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(new_address, address);

    let (_fragment, bytes) = read(
        store.clone(),
        partition,
        new_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_ref());
}

#[test]
fn immutable_store_settings_default_includes_fan_out_fields() {
    let s = ImmutableStoreSettings::default();
    assert_eq!(s.initial_fan_out_level, 1);
    assert_eq!(
        s.fan_out_threshold,
        lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT
    );
}

#[tokio::test]
async fn store_initializes_group_bucket_count_from_settings_level_1() {
    use std::sync::atomic::Ordering;
    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            initial_fan_out_level: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for group in &store.group {
        assert_eq!(group.bucket_count.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn store_initializes_group_bucket_count_from_settings_level_256() {
    use std::sync::atomic::Ordering;
    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for group in &store.group {
        assert_eq!(
            group.bucket_count.load(Ordering::Relaxed),
            lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX
        );
    }
}

/// The compaction resume point is decided by what the group work reports, not by the
/// stop flag read after it: a group that gave up has packfiles left to rewrite, and
/// advancing past it would leave them for a pass that never comes. Both answers are
/// driven through a real sweep, so a stray report inside the packfile loop is caught.
#[tokio::test]
async fn group_compaction_reports_whether_it_finished() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_stop_report_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    let partition = Partition::from([0x0cu8; 16]);
    for index in 0u8..8 {
        let payload = vec![index; 4096];
        let address = Address {
            hash: lore_storage::hash::hash_slice(&payload),
            context: Context::from([index; 16]),
        };
        let fragment = Fragment {
            // Non-durable, so eviction is forbidden to reclaim it and the packfile
            // sweep has payloads to move.
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        store
            .clone()
            .put(
                partition,
                address,
                fragment,
                Some(Bytes::from(payload)),
                false,
            )
            .await
            .unwrap();
    }
    store.clone().flush(true).await.unwrap();

    // Compaction runs per group and the hash decides which one the payloads landed in.
    let (group_index, _bucket_index) = populated_bucket(&store).await;

    // A target below what the group holds drives the sweep and the truncate, rather
    // than breaking on the size check before either runs.
    let completed = store
        .clone()
        .compact_group_packfiles(
            group_index,
            store.path.clone(),
            1,
            true,
            false,
            CompactionInstruments::default(),
            None,
        )
        .await
        .unwrap();
    assert!(
        completed,
        "a group that ran to the end must report complete"
    );

    let _stopped = GcStopRequest::raise(&store.stop_requests, false);

    let completed = store
        .clone()
        .compact_group_packfiles(
            group_index,
            store.path.clone(),
            1,
            true,
            false,
            CompactionInstruments::default(),
            None,
        )
        .await
        .unwrap();
    assert!(
        !completed,
        "a stopped group must report incomplete so the caller holds the resume point"
    );
}

/// A step that commits to work reports one begin and owes exactly one end; a call that
/// gives up before committing reports neither, and does no work from the resume point.
#[tokio::test]
async fn compaction_reports_one_end_for_every_begin() {
    use lore_storage::immutable_store::ImmutableStore;

    #[derive(Default)]
    struct CountingSink {
        begins: AtomicUsize,
        ends: AtomicUsize,
    }

    impl lore_storage::gc_event::GcEventSink for CountingSink {
        fn eviction_begin(&self, _target_fragments: u64) {}
        fn eviction_progress(&self, _evicted: u64) {}
        fn eviction_end(&self, _total_evicted: u64) {}
        fn compaction_begin(&self, _target_bytes: u64) {
            self.begins.fetch_add(1, atomic::Ordering::Relaxed);
        }
        fn compaction_progress(&self, _compacted_bytes: u64) {}
        fn compaction_end(&self, _total_compacted_bytes: u64) {
            self.ends.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    let dir = lore_base::test_util::TempDir::new("is_sink_pairing_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    // Content, so the pass finds itself above the limit and announces a begin.
    let payload = vec![0x5au8; 4096];
    store
        .clone()
        .put(
            Partition::from([0x0du8; 16]),
            Address {
                hash: lore_storage::hash::hash_slice(&payload),
                context: Context::default(),
            },
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();
    store.clone().flush(true).await.unwrap();

    let sink = Arc::new(CountingSink::default());

    let resume = store
        .clone()
        .compact_packfiles(1, None, false, Some(sink.clone()))
        .await
        .unwrap()
        .expect("a step over a 256 group store leaves groups to come");

    assert_eq!(
        sink.begins.load(atomic::Ordering::Relaxed),
        1,
        "a committed step announces exactly one begin"
    );
    assert_eq!(
        sink.ends.load(atomic::Ordering::Relaxed),
        1,
        "a committed step owes an end for the begin it reported"
    );

    let _stopped = GcStopRequest::raise(&store.stop_requests, false);

    assert_eq!(
        store
            .clone()
            .compact_packfiles(1, Some(resume), false, Some(sink.clone()))
            .await
            .unwrap(),
        None,
        "a stopped call must not take another round from the resume point"
    );
    assert_eq!(
        sink.begins.load(atomic::Ordering::Relaxed),
        1,
        "a call that gives up before committing must not announce a begin"
    );
    assert_eq!(
        sink.ends.load(atomic::Ordering::Relaxed),
        1,
        "a call that gives up before committing reports neither begin nor end"
    );
}

/// A stop asks the passes in flight to give up; it is not a switch that stays off. The
/// store is shared by path, so a caller quiescing it leaves the others collecting.
#[tokio::test]
async fn a_stop_lifts_once_it_has_drained() {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(None, ImmutableStoreSettings::default())
        .await
        .unwrap();

    store.clone().stop_gc(false).await;

    assert!(
        !store.gc_stop_requested(),
        "a stop that is not terminating must lift so a shared store keeps collecting"
    );
}

/// Two callers overlap whenever handles closing on one path race each other or a
/// shutdown. The first to drain must not lift the second's request, or the second waits
/// out a whole pass instead of the pass giving up at its next packfile.
#[tokio::test]
async fn a_stop_stays_raised_while_another_is_outstanding() {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(None, ImmutableStoreSettings::default())
        .await
        .unwrap();

    {
        let _outstanding = GcStopRequest::raise(&store.stop_requests, false);
        store.clone().stop_gc(false).await;
        assert!(
            store.gc_stop_requested(),
            "a drain that completes must leave another caller's request raised"
        );
    }

    assert!(
        !store.gc_stop_requested(),
        "the outstanding request going away leaves the store collecting again"
    );
}

/// A last-access stamp far enough in the past that a resolve marks the bucket for rewrite.
const STALE_ACCESS: u64 = 1;

/// Answer the group and bucket index of the first bucket in `store` holding an entry. The
/// hash decides where a put lands, so a test that has to reach the entry it stored searches
/// rather than derives.
async fn populated_bucket(store: &Arc<LocalImmutableStore>) -> (usize, usize) {
    for (group_index, group) in store.group.iter().enumerate() {
        for (bucket_index, cell) in group.bucket.iter().enumerate() {
            if let Some(bucket) = cell.get()
                && !bucket.read().await.entry.is_empty()
            {
                return (group_index, bucket_index);
            }
        }
    }
    panic!("a put must populate a bucket");
}

/// Store one fragment in `store`, set its last-access stamp to `stamp`, and clear the dirty
/// flag of the bucket it landed in. Answers that bucket and the address naming the entry.
async fn backdated_fragment(
    store: &Arc<LocalImmutableStore>,
    stamp: u64,
) -> ((usize, usize), Partition, Address) {
    use lore_storage::immutable_store::ImmutableStore;

    let partition = Partition::from([0x11u8; 16]);
    let payload = vec![0x22u8; 128];
    let address = Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: Context::from([0x33u8; 16]),
    };
    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    store
        .clone()
        .put(
            partition,
            address,
            fragment,
            Some(Bytes::from(payload)),
            false,
        )
        .await
        .unwrap();

    let (group_index, bucket_index) = populated_bucket(store).await;
    let group = &store.group[group_index];
    group.bucket(bucket_index).write().await.entry[0]
        .data
        .last_access = stamp;
    group.dirty[bucket_index].store(false, atomic::Ordering::Relaxed);

    ((group_index, bucket_index), partition, address)
}

/// Resolve one backdated fragment in an in-memory store. Answers the stamp its entry carries
/// afterward and whether the resolve marked the bucket for rewrite.
async fn resolve_one_fragment(atime: bool, stamp: u64) -> (u64, bool) {
    use lore_storage::immutable_store::ImmutableStore;

    let store = LocalImmutableStore::new(
        None,
        ImmutableStoreSettings {
            atime,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let ((group_index, bucket_index), partition, address) = backdated_fragment(&store, stamp).await;

    let mut results = [StoreMatchResult::default(); 1];
    store
        .clone()
        .query(partition, &[address], &mut results)
        .await
        .unwrap();

    let group = &store.group[group_index];
    (
        group.bucket(bucket_index).read().await.entry[0]
            .data
            .last_access,
        group.dirty[bucket_index].load(atomic::Ordering::Relaxed),
    )
}

/// Eviction and compaction rank entries by last access, so a resolve moves the stamp to now
/// whatever its age — ranking by write time reclaims a fragment every command reads ahead of
/// one nothing has touched since it landed. A move this small rides along with whatever
/// writes the bucket next rather than rewriting it on its own.
#[tokio::test]
async fn a_small_move_advances_the_stamp_without_dirtying_the_bucket() {
    let recent = LocalImmutableStore::last_access().saturating_sub(10);

    let (last_access, dirty) = resolve_one_fragment(true, recent).await;

    assert!(last_access > recent, "a resolve always advances the stamp");
    assert!(!dirty, "a small move must not schedule a rewrite");
}

/// A stamp that moved past the window is worth a bucket rewrite of its own.
#[tokio::test]
async fn a_stale_stamp_dirties_the_bucket_holding_it() {
    let (_last_access, dirty) = resolve_one_fragment(true, STALE_ACCESS).await;

    assert!(dirty, "a stamp this far behind has to reach disk");
}

/// A store that never reclaims records no access, so a resolve neither moves the stamp nor
/// dirties the bucket holding it.
#[tokio::test]
async fn a_resolve_records_nothing_without_atime() {
    let (last_access, dirty) = resolve_one_fragment(false, STALE_ACCESS).await;

    assert_eq!(last_access, STALE_ACCESS);
    assert!(!dirty);
}

/// Ranking by access is only worth anything if a stamp outlives the process that made it, so
/// this reads the bucket back off disk rather than out of the store that wrote it.
#[tokio::test]
async fn a_stamp_reaches_the_bucket_file() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_atime_persist_");
    let store =
        LocalImmutableStore::new(Some(dir.to_path_buf()), ImmutableStoreSettings::default())
            .await
            .unwrap();

    let ((group_index, bucket_index), partition, address) =
        backdated_fragment(&store, STALE_ACCESS).await;

    let mut results = [StoreMatchResult::default(); 1];
    store
        .clone()
        .query(partition, &[address], &mut results)
        .await
        .unwrap();
    store.clone().flush(true).await.unwrap();

    let root = store.path.clone().expect("a disk-backed store has a path");
    let (_sorted_index, entry, _upgrade, _dirty) = ImmutableStoreBucket::deserialize_files(
        format_bucket_path(&root, group_index, bucket_index),
    )
    .await
    .unwrap();

    assert_eq!(entry.len(), 1, "the stamp must have dirtied the bucket");
    assert!(
        entry[0].data.last_access > STALE_ACCESS,
        "the stamp a resolve made must survive the flush"
    );
}

fn payload_data(pack_file: u32, encoding: u32, storage: u32) -> ImmutableData {
    ImmutableData {
        flags: encoding | storage,
        size_payload: if pack_file == 0 { 0 } else { 100 },
        size_content: 256,
        pack_offset: if pack_file == 0 { 0 } else { 200 },
        pack_file,
        last_access: 0,
    }
}

#[test]
fn merge_from_copy_source_adopts_payload_and_encoding() {
    // Target had its own uncompressed payload. Source has the same content stored
    // compressed in a different pack file. After merge, target adopts source's pack
    // pointer and the encoding flag that describes those bytes — keeping target's
    // pre-existing flags would mis-describe the new payload.
    let mut target = payload_data(1, 0, 0);
    let source = payload_data(2, FragmentFlags::PayloadCompressedZstd.bits(), 0);

    target.merge_from_copy_source(source, false);

    assert_eq!(target.pack_file, 2, "target adopts source's pack_file");
    assert_eq!(target.pack_offset, 200);
    assert_eq!(target.size_payload, 100);
    assert_ne!(
        target.flags & FragmentFlags::PayloadCompressedZstd.bits(),
        0,
        "encoding flag must follow the adopted payload",
    );
    assert_ne!(
        target.flags & FragmentFlags::PayloadStoredLocal.bits(),
        0,
        "adopted bytes are locally available",
    );
}

#[test]
fn merge_from_copy_source_preserves_target_durable() {
    // Target had PayloadStoredDurable from a prior remote round-trip on the destination
    // tuple. A subsequent local copy must not unset that bit.
    let mut target = payload_data(0, 0, FragmentFlags::PayloadStoredDurable.bits());
    let source = payload_data(2, 0, FragmentFlags::PayloadStoredDurable.bits());

    target.merge_from_copy_source(source, false);

    assert_ne!(
        target.flags & FragmentFlags::PayloadStoredDurable.bits(),
        0,
        "target's prior Durable must be preserved",
    );
}

#[test]
fn merge_from_copy_source_durable_only_from_caller() {
    // Source carries Durable; target had none. With `durable=false`, source's Durable
    // must NOT propagate. With `durable=true`, the caller's intent sets the bit.
    let source = payload_data(2, 0, FragmentFlags::PayloadStoredDurable.bits());

    let mut local_only = payload_data(0, 0, 0);
    local_only.merge_from_copy_source(source, false);
    assert_eq!(
        local_only.flags & FragmentFlags::PayloadStoredDurable.bits(),
        0,
        "Durable must not propagate from source on a local-only copy",
    );

    let mut remote_confirmed = payload_data(0, 0, 0);
    remote_confirmed.merge_from_copy_source(source, true);
    assert_ne!(
        remote_confirmed.flags & FragmentFlags::PayloadStoredDurable.bits(),
        0,
        "caller's `durable=true` sets the bit",
    );
}

#[tokio::test]
async fn copy_adopts_source_payload_and_decompresses_through_target_partition() {
    // Prime target with uncompressed payload at one address, prime source with the same
    // hash but compressed payload, then copy source → target. The target entry must
    // adopt source's payload pointer along with the matching encoding flag so a read
    // against the target partition decompresses correctly and returns the original bytes.
    use std::sync::atomic::Ordering;

    use lore_storage::compress::COMPRESSION_MODE;
    use lore_storage::compress::CompressionMode;
    use lore_storage::options::ReadOptions;
    use lore_storage::options::WriteOptions;
    use lore_storage::read::read;
    use lore_storage::write::StoreResult;
    use lore_storage::write::write_content;

    let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
        None::<&Path>,
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .unwrap();

    let target_partition = Partition::from([0x01u8; 16]);
    let source_partition = Partition::from([0x02u8; 16]);
    let context = Context::from([0x03u8; 16]);
    // Highly compressible content so compression actually triggers when enabled.
    let payload: Vec<u8> = vec![0xABu8; 4096];

    // Prime target (uncompressed).
    let prev_mode = COMPRESSION_MODE.swap(CompressionMode::NoCompression as u32, Ordering::AcqRel);
    let StoreResult {
        address: target_address,
        ..
    } = write_content(
        store.clone(),
        target_partition,
        context,
        Bytes::from(payload.clone()),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();

    // Prime source (compressed).
    COMPRESSION_MODE.store(CompressionMode::Zstd as u32, Ordering::Release);
    let StoreResult {
        address: source_address,
        ..
    } = write_content(
        store.clone(),
        source_partition,
        context,
        Bytes::from(payload.clone()),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();
    // Restore mode for any other tests sharing this process.
    COMPRESSION_MODE.store(prev_mode, Ordering::Release);

    assert_eq!(target_address, source_address, "same content → same hash");

    // Copy source → target with durable=false (pure local). Pass the source context as the
    // destination context so the address tuple is preserved (cross-partition copy with the
    // hash + context invariant the original test relied on).
    store
        .clone()
        .copy(
            source_partition,
            source_address,
            target_partition,
            source_address.context,
            CopyBehavior {
                durable: false,
                do_not_replicate: false,
            },
        )
        .await
        .unwrap();

    // Read from target partition: payload bytes must round-trip identically. The helper
    // must have adopted source's pack pointer AND encoding flag together — if encoding
    // and bytes were ever desynchronized, decompression in `read` would fail or return
    // garbage.
    let (_fragment, bytes) = read(
        store.clone(),
        target_partition,
        target_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_slice());
}

#[tokio::test]
async fn copy_same_partition_new_context_adopts_payload_without_transfer() {
    // Same partition, different context — the in-partition deduplication path. The destination
    // entry must adopt the source's payload pointer (no payload transfer) and the read against
    // the new `(partition, hash, target_context)` tuple must return the same bytes that were
    // originally written under the source context.
    use lore_storage::options::ReadOptions;
    use lore_storage::options::WriteOptions;
    use lore_storage::read::read;
    use lore_storage::write::StoreResult;
    use lore_storage::write::write_content;

    let store: Arc<dyn lore_storage::immutable_store::ImmutableStore> = create(
        None::<&Path>,
        ImmutableStoreCreateOptions::none(),
        false,
        ImmutableStoreSettings::default(),
    )
    .await
    .unwrap();

    let partition = Partition::from([0xA1u8; 16]);
    let source_context = Context::from([0xB1u8; 16]);
    let target_context = Context::from([0xB2u8; 16]);
    let payload: Vec<u8> = b"in-partition new-context dedup payload".to_vec();

    // Seed the source tuple `(partition, hash, source_context)`.
    let StoreResult {
        address: source_address,
        ..
    } = write_content(
        store.clone(),
        partition,
        source_context,
        Bytes::from(payload.clone()),
        WriteOptions::default(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(source_address.context, source_context);

    // Copy within the same partition, retagging the destination with `target_context`. The
    // store's `copy` is the only call we make — there must be no payload transfer; the
    // destination tuple gets its own entry that points at the source's payload data.
    store
        .clone()
        .copy(
            partition,
            source_address,
            partition,
            target_context,
            CopyBehavior {
                durable: false,
                do_not_replicate: false,
            },
        )
        .await
        .unwrap();

    // The destination address shares the source's hash but takes the target context.
    let destination_address = Address {
        hash: source_address.hash,
        context: target_context,
    };

    let (_fragment, bytes) = read(
        store.clone(),
        partition,
        destination_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_slice());

    // Source tuple must remain readable independently — copy creates a new entry, it does
    // not consume or repoint the source.
    let (_fragment, bytes) = read(
        store.clone(),
        partition,
        source_address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes.as_ref(), payload.as_slice());
}

/// The local store is the reference implementation of the store contract: it resolves an
/// address in a single bucket pass, so it can establish every level at no extra cost and has
/// no reason to under-report any of them.
#[tokio::test]
async fn satisfies_the_immutable_store_contract() {
    let dir = lore_base::test_util::TempDir::new("is_conformance_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("create store");

    lore_storage::conformance::verify_immutable_store(
        store,
        lore_storage::conformance::Capabilities::new("LocalImmutableStore").stores_metadata_only(),
    )
    .await;
}

/// Healing a payload that no longer verifies has to leave the store answering absence, so
/// that the content is offered again.
///
/// Clearing the payload pointer alone would not: an entry still answers a full match, which
/// tells a peer the store holds content it can no longer serve, and a peer told that stops
/// offering it. The association the caller asked about and its siblings under other contexts
/// go together, since they name the one payload that failed.
///
/// A payload is shared by every association that deduplicated onto it, so one proven bad is
/// bad in whichever partition names it, and all of them go. Leaving one behind leaves that
/// partition being told the content is here until it happens to verify for itself.
#[tokio::test]
async fn healing_a_corrupt_payload_drops_the_associations_that_named_it() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_drop_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let payload = Bytes::from_static(b"a payload that will stop verifying");
    let hash = lore_storage::hash::hash_slice(payload.as_ref());
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    let partition = Partition::from([0x61u8; 16]);
    let address = Address {
        hash,
        context: Context::from([0x62u8; 16]),
    };
    let sibling = Address {
        hash,
        context: Context::from([0x63u8; 16]),
    };
    let neighbour = Partition::from([0x64u8; 16]);

    for (in_partition, at) in [
        (partition, address),
        (partition, sibling),
        (neighbour, address),
    ] {
        store
            .clone()
            .put(in_partition, at, fragment, Some(payload.clone()), false)
            .await
            .expect("put the payload");
    }

    // Corrupt the stored bytes in place, which is what verification is there to notice.
    let group_index = hash.data()[0] as usize;
    let found = store
        .clone()
        .find(partition, address)
        .await
        .expect("the entry is there to corrupt");
    store.group[group_index]
        .packstore
        .obliterate(
            found.data.pack_file,
            found.data.pack_offset,
            found.data.size_payload,
        )
        .await
        .expect("overwrite the payload");

    let result = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, true)
        .await
        .expect("verify answers");
    assert!(
        result.healed,
        "a payload that does not verify was not healed"
    );

    for gone in [address, sibling] {
        let resolved = lore_storage::immutable_store::query_one(
            &(store.clone() as Arc<dyn ImmutableStore>),
            partition,
            gone,
        )
        .await
        .expect("query answers");
        assert_eq!(
            resolved.match_made,
            StoreMatch::MatchNone,
            "a healed association still answers a match, so nothing will offer {gone} again"
        );
    }

    let resolved = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        neighbour,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        resolved.match_made,
        StoreMatch::MatchNone,
        "another partition was left naming the payload this store proved it cannot serve"
    );
}

/// An entry that names no payload is the wedged state itself, and verifying has to be able
/// to undo it.
///
/// Nothing failed here - there is no payload to fail - so this is the state left behind by
/// whatever dropped one, and left alone it is permanent: the entry answers a full match, the
/// store is asked for bytes it does not have, and the peer that holds them is told not to
/// send them. Reporting it as verified is what makes it permanent, so it counts as a failure
/// and heals the same way.
#[tokio::test]
async fn verifying_an_entry_with_no_payload_reports_it_and_heals_the_wedge() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_wedge_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let payload = Bytes::from_static(b"bytes this store will never hold");
    let hash = lore_storage::hash::hash_slice(payload.as_ref());
    let partition = Partition::from([0x71u8; 16]);
    let address = Address {
        hash,
        context: Context::from([0x72u8; 16]),
    };

    // A header with no payload, which is what an entry left behind by a drop looks like.
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            None,
            false,
        )
        .await
        .expect("put the header alone");

    let before = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        before.match_made,
        StoreMatch::MatchFull,
        "the wedge this heals is an entry with no payload answering a full match"
    );

    let reported = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, false)
        .await
        .expect("verify answers");
    assert!(
        reported.verification_result.is_err(),
        "an entry the store cannot serve was reported as verified"
    );
    assert!(!reported.healed, "verify healed without being asked to");

    let healed = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, true)
        .await
        .expect("verify answers");
    assert!(healed.healed, "the wedged entry was not healed");

    let after = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        after.match_made,
        StoreMatch::MatchNone,
        "the entry survived healing, so the address stays wedged"
    );
}

/// Healing has to survive a reopen, which means the emptied bucket reaching disk.
///
/// Dropping the last entry a bucket holds leaves nothing to write, and a flush that treats
/// that as nothing to do leaves the file the bucket was last written to in place. The store
/// then reports the association again the next time it opens, healed in memory only.
#[tokio::test]
async fn healing_the_last_entry_in_a_bucket_survives_reopening_the_store() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_reopen_");
    let path = std::path::PathBuf::from(dir.as_ref());

    let payload = Bytes::from_static(b"a payload that will not survive its own bucket");
    let hash = lore_storage::hash::hash_slice(payload.as_ref());
    let partition = Partition::from([0x91u8; 16]);
    let address = Address {
        hash,
        context: Context::from([0x92u8; 16]),
    };

    {
        let store = LocalImmutableStore::new(
            Some(path.clone()),
            ImmutableStoreSettings {
                isolate_partitions: true,
                ..Default::default()
            },
        )
        .await
        .expect("create store");

        store
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put the payload");

        // On disk before anything goes wrong, so the file healing has to account for exists.
        store.clone().flush(true).await.expect("flush the bucket");

        let group_index = hash.data()[0] as usize;
        let found = store
            .clone()
            .find(partition, address)
            .await
            .expect("the entry is there to corrupt");
        store.group[group_index]
            .packstore
            .obliterate(
                found.data.pack_file,
                found.data.pack_offset,
                found.data.size_payload,
            )
            .await
            .expect("overwrite the payload");

        let healed = store
            .clone()
            .verify_fragment(address, partition, StoreMatch::MatchFull, true)
            .await
            .expect("verify answers");
        assert!(healed.healed, "the corrupt payload was not healed");

        store.clone().flush(true).await.expect("flush the removal");
    }

    let reopened = LocalImmutableStore::new(
        Some(path),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("reopen store");

    let resolved = lore_storage::immutable_store::query_one(
        &(reopened as Arc<dyn ImmutableStore>),
        partition,
        address,
    )
    .await
    .expect("query answers");
    assert_eq!(
        resolved.match_made,
        StoreMatch::MatchNone,
        "reopening the store loaded back the association healing removed"
    );
}

/// An obliterated fragment is meant to hold no payload, so verifying one reports nothing
/// wrong and heals nothing.
///
/// A tombstone is indistinguishable from a wedged entry by its pack file alone - both name
/// none - so what separates them is the flag. Reporting a deletion as corruption would put
/// an operator onto a fault that is not there, and healing one would flush the bucket and
/// claim a repair having changed nothing.
#[tokio::test]
async fn verifying_an_obliterated_fragment_reports_nothing_and_heals_nothing() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_heal_tombstone_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let payload = Bytes::from_static(b"content that is about to be obliterated");
    let partition = Partition::from([0x81u8; 16]);
    let address = Address {
        hash: lore_storage::hash::hash_slice(payload.as_ref()),
        context: Context::from([0x82u8; 16]),
    };

    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(payload),
            false,
        )
        .await
        .expect("put the payload");

    store
        .clone()
        .obliterate(
            partition,
            address,
            Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
        )
        .await
        .expect("obliterate the fragment");

    let tombstone = store
        .clone()
        .find(partition, address)
        .await
        .expect("the tombstone is in the index");
    assert_eq!(tombstone.matching, StoreMatch::MatchFull);
    assert_ne!(
        tombstone.data.flags & FragmentFlags::PayloadObliterated.bits(),
        0,
        "obliterate did not leave a tombstone to verify against"
    );

    let reported = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, false)
        .await
        .expect("verify answers");
    assert!(
        reported.verification_result.is_ok(),
        "an intentional deletion was reported as a fault"
    );

    let healed = store
        .clone()
        .verify_fragment(address, partition, StoreMatch::MatchFull, true)
        .await
        .expect("verify answers");
    assert!(
        !healed.healed,
        "healing claimed a repair on a fragment that is meant to hold no payload"
    );

    let after = store
        .clone()
        .find(partition, address)
        .await
        .expect("the tombstone is still in the index");
    assert_eq!(
        after.matching,
        StoreMatch::MatchFull,
        "healing took the tombstone with it"
    );
    assert_ne!(
        after.data.flags & FragmentFlags::PayloadObliterated.bits(),
        0,
        "healing cleared the tombstone's flag"
    );
}

/// A store that isolates partitions reports further than it reads, and this is the only
/// implementation of that split.
///
/// A sibling context in the same partition is a partition match, which `query` reports so a
/// caller can duplicate the association with a copy, and which `get` refuses so that nothing
/// crossing a wire without its level is mistaken for an association of the caller's own. The
/// battery bounds the reported level from above and cannot assert this, because a store that
/// resolves associations alone is entitled to answer `MatchNone` here instead.
#[tokio::test]
async fn an_isolating_store_reports_further_than_it_reads() {
    use lore_storage::immutable_store::ImmutableStore;

    let dir = lore_base::test_util::TempDir::new("is_scope_split_");
    let store = LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("create store");

    let partition = Partition::from([0x51u8; 16]);
    let payload = Bytes::from_static(b"one hash, two contexts, one partition");
    let stored = Address {
        hash: lore_storage::hash::hash_slice(payload.as_ref()),
        context: Context::from([0x52u8; 16]),
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    store
        .clone()
        .put(partition, stored, fragment, Some(payload), false)
        .await
        .expect("put under the storing context");

    let sibling = Address {
        hash: stored.hash,
        context: Context::from([0x53u8; 16]),
    };

    let resolved = lore_storage::immutable_store::query_one(
        &(store.clone() as Arc<dyn ImmutableStore>),
        partition,
        sibling,
    )
    .await
    .expect("query a sibling context");
    assert_eq!(
        resolved.match_made,
        StoreMatch::MatchPartition,
        "an isolating store must still report the partition match a copy would act on"
    );
    assert_eq!(resolved.partition, partition);

    assert!(
        store
            .clone()
            .get_metadata(partition, sibling)
            .await
            .expect("get_metadata answers rather than failing")
            .match_made
            == StoreMatch::MatchNone,
        "an isolating store described an association it does not hold"
    );
    assert!(
        store.clone().get(partition, sibling).await.is_err(),
        "an isolating store served a sibling context's payload"
    );
}

/// The source forms `copy` accepts: an exact association, and any association a partition
/// holds. A caller acting on a partition match only ever has the second.
mod copy_source {
    use lore_storage::immutable_store::ImmutableStore;

    use super::*;

    type Store = Arc<dyn ImmutableStore>;

    async fn store_with(entries: &[(Partition, Context)], payload: &[u8]) -> (Store, Address) {
        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let address = Address {
            hash: lore_storage::hash::hash_slice(payload),
            context: Context::default(),
        };
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        for (partition, context) in entries {
            store
                .clone()
                .put(
                    *partition,
                    Address {
                        hash: address.hash,
                        context: *context,
                    },
                    fragment,
                    Some(Bytes::copy_from_slice(payload)),
                    false,
                )
                .await
                .expect("seed association");
        }
        (store, address)
    }

    async fn readable(
        store: &Store,
        partition: Partition,
        address: Address,
        payload: &[u8],
    ) -> bool {
        lore_storage::read::read(
            store.clone(),
            partition,
            address,
            None,
            lore_storage::options::ReadOptions::default(),
            None,
        )
        .await
        .is_ok_and(|(_fragment, bytes)| bytes.as_ref() == payload)
    }

    #[tokio::test]
    async fn a_zero_context_takes_any_association_in_the_partition() {
        let payload = b"zero context names any association".as_slice();
        let partition = Partition::from([0x11u8; 16]);
        let held = Context::from([0x12u8; 16]);
        let wanted = Context::from([0x13u8; 16]);
        let (store, address) = store_with(&[(partition, held)], payload).await;

        store
            .clone()
            .copy(
                partition,
                Address::zero_context_hash(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("a partition holding the hash must answer a source naming no context");

        assert!(
            readable(
                &store,
                partition,
                Address {
                    hash: address.hash,
                    context: wanted
                },
                payload
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_zero_context_crosses_partitions() {
        let payload = b"zero context across partitions".as_slice();
        let source = Partition::from([0x21u8; 16]);
        let destination = Partition::from([0x22u8; 16]);
        let held = Context::from([0x23u8; 16]);
        let wanted = Context::from([0x24u8; 16]);
        let (store, address) = store_with(&[(source, held)], payload).await;

        store
            .clone()
            .copy(
                source,
                Address::zero_context_hash(address.hash),
                destination,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("copy from a source partition naming no context");

        assert!(
            readable(
                &store,
                destination,
                Address {
                    hash: address.hash,
                    context: wanted
                },
                payload
            )
            .await
        );
    }

    /// The partition is still the boundary: naming no context widens the search inside one
    /// partition, never across them.
    #[tokio::test]
    async fn a_zero_context_does_not_reach_another_partition() {
        let payload = b"zero context stays in its partition".as_slice();
        let held_in = Partition::from([0x31u8; 16]);
        let asked_of = Partition::from([0x32u8; 16]);
        let (store, address) = store_with(&[(held_in, Context::from([0x33u8; 16]))], payload).await;

        let err = store
            .clone()
            .copy(
                asked_of,
                Address::zero_context_hash(address.hash),
                Partition::from([0x34u8; 16]),
                Context::from([0x35u8; 16]),
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect_err("a partition holding nothing has no association to name");
        assert!(matches!(err, StoreError::AddressNotFound(_)));
    }

    /// A named context is resolved exactly. A sibling holding the same hash is not a fallback,
    /// which is the whole difference between the two forms.
    #[tokio::test]
    async fn a_named_context_does_not_widen_to_a_sibling() {
        let payload = b"an exact source is exact".as_slice();
        let partition = Partition::from([0x41u8; 16]);
        let (store, address) =
            store_with(&[(partition, Context::from([0x42u8; 16]))], payload).await;

        let err = store
            .clone()
            .copy(
                partition,
                Address {
                    hash: address.hash,
                    context: Context::from([0x43u8; 16]),
                },
                partition,
                Context::from([0x44u8; 16]),
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect_err("a context the partition does not hold must not resolve to a sibling");
        assert!(matches!(err, StoreError::AddressNotFound(_)));
    }

    /// Obliterating a fragment tree must terminate when a child shares its
    /// parent's bucket.
    ///
    /// Obliterating an address takes the write lock on the bucket that address
    /// lives in, and a child chooses its own bucket from its own hash.
    /// `tokio::sync::RwLock` is not reentrant, so a child that lands in the
    /// bucket its parent is holding used to wait on a lock the same task
    /// already owned, and the obliterate never returned. At one bucket to a
    /// group - where a client store starts - every child in the parent's group
    /// collides, which is one child in 256; a 3.4 MB file of 53 chunks hung one
    /// run in five.
    ///
    /// The collision is searched for rather than written down because both
    /// hashes are content-derived: the first byte chooses the group, and with
    /// one bucket in it the group is the bucket.
    #[tokio::test]
    async fn a_child_in_its_parent_bucket_does_not_deadlock_the_obliterate() {
        let partition = Partition::from([0x61u8; 16]);
        let context = Context::from([0x62u8; 16]);

        let (payload, leaf_hash, root_hash, references) = (0u32..)
            .find_map(|salt| {
                let payload = format!("leaf payload {salt}").into_bytes();
                let leaf_hash = lore_storage::hash::hash_slice(&payload);
                let references = vec![FragmentReference {
                    hash: leaf_hash,
                    offset_content: 0,
                }];
                let root_hash = lore_storage::hash::hash_slice(references.as_bytes());
                (root_hash.data()[0] == leaf_hash.data()[0])
                    .then_some((payload, leaf_hash, root_hash, references))
            })
            .expect("a leaf hashing into its own list's group");

        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");

        store
            .clone()
            .put(
                partition,
                Address {
                    hash: leaf_hash,
                    context,
                },
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(Bytes::copy_from_slice(&payload)),
                false,
            )
            .await
            .expect("put leaf");

        let references = Bytes::copy_from_slice(references.as_bytes());
        store
            .clone()
            .put(
                partition,
                Address {
                    hash: root_hash,
                    context,
                },
                Fragment {
                    flags: FragmentFlags::PayloadFragmented.bits(),
                    size_payload: references.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(references),
                false,
            )
            .await
            .expect("put fragment list");

        let stats = Arc::new(lore_storage::store_types::StoreObliterateStats::default());
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            store.clone().obliterate(
                partition,
                Address {
                    hash: root_hash,
                    context,
                },
                stats.clone(),
            ),
        )
        .await
        .expect("obliterating a tree whose child shares its parent's bucket must terminate")
        .expect("obliterate");

        assert_eq!(
            stats.num_fragments.load(atomic::Ordering::Relaxed),
            2,
            "both the list and the leaf it references are fragments"
        );

        let addresses = [
            Address {
                hash: root_hash,
                context,
            },
            Address {
                hash: leaf_hash,
                context,
            },
        ];
        let mut results = [StoreMatchResult::default(); 2];
        store
            .clone()
            .query(partition, &addresses, &mut results)
            .await
            .expect("query");
        for result in results {
            assert_eq!(
                result.match_made,
                StoreMatch::MatchNone,
                "an obliterated address must not resolve"
            );
        }
    }

    /// A tombstone is not a representation to adopt, so the walk passes over it and copies the
    /// live association beside it.
    #[tokio::test]
    async fn a_zero_context_skips_an_obliterated_association() {
        let payload = b"one obliterated reference, one alive".as_slice();
        let partition = Partition::from([0x51u8; 16]);
        let doomed = Context::from([0x52u8; 16]);
        let alive = Context::from([0x53u8; 16]);
        let wanted = Context::from([0x54u8; 16]);
        let (store, address) =
            store_with(&[(partition, doomed), (partition, alive)], payload).await;

        store
            .clone()
            .obliterate(
                partition,
                Address {
                    hash: address.hash,
                    context: doomed,
                },
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate one reference");

        store
            .clone()
            .copy(
                partition,
                Address::zero_context_hash(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("the surviving association is the one to copy from");

        assert!(
            readable(
                &store,
                partition,
                Address {
                    hash: address.hash,
                    context: wanted
                },
                payload
            )
            .await
        );
    }

    #[tokio::test]
    async fn an_obliterated_source_is_not_copied() {
        let payload = b"the only reference is obliterated".as_slice();
        let partition = Partition::from([0x61u8; 16]);
        let doomed = Context::from([0x62u8; 16]);
        let (store, address) = store_with(&[(partition, doomed)], payload).await;

        let source = Address {
            hash: address.hash,
            context: doomed,
        };
        store
            .clone()
            .obliterate(
                partition,
                source,
                Arc::new(lore_storage::store_types::StoreObliterateStats::default()),
            )
            .await
            .expect("obliterate the only reference");

        for named in [source, Address::zero_context_hash(address.hash)] {
            let err = store
                .clone()
                .copy(
                    partition,
                    named,
                    partition,
                    Context::from([0x63u8; 16]),
                    CopyBehavior {
                        durable: false,
                        do_not_replicate: false,
                    },
                )
                .await
                .expect_err("a tombstone is not an association to copy from");
            assert!(matches!(err, StoreError::AddressNotFound(_)));
        }
    }

    /// A hash the partition holds only the representation of. The walk records it as the
    /// fallback rather than passing over it, so the copy still registers the destination — as it
    /// does for an exact source that has no payload either.
    #[tokio::test]
    async fn a_zero_context_falls_back_to_an_association_without_its_payload() {
        let payload = b"representation held without its payload".as_slice();
        let partition = Partition::from([0x81u8; 16]);
        let held = Context::from([0x82u8; 16]);
        let wanted = Context::from([0x83u8; 16]);

        let store = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create store");
        let address = Address {
            hash: lore_storage::hash::hash_slice(payload),
            context: held,
        };
        store
            .clone()
            .put(
                partition,
                address,
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                None,
                false,
            )
            .await
            .expect("seed the representation alone");

        store
            .clone()
            .copy(
                partition,
                Address::zero_context_hash(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("the representation alone is still a source");

        let resolved = lore_storage::immutable_store::query_one(
            &store,
            partition,
            Address {
                hash: address.hash,
                context: wanted,
            },
        )
        .await
        .expect("query the destination");
        assert_eq!(resolved.match_made, StoreMatch::MatchFull);
    }

    /// A `query` naming a context hands back a source `copy` resolves exactly, which is the
    /// pairing the write path relies on to avoid the wider search.
    #[tokio::test]
    async fn a_partition_match_names_a_context_copy_resolves_exactly() {
        let payload = b"query names the association copy reads".as_slice();
        let partition = Partition::from([0x71u8; 16]);
        let held = Context::from([0x72u8; 16]);
        let wanted = Context::from([0x73u8; 16]);
        let (store, address) = store_with(&[(partition, held)], payload).await;

        let resolved = lore_storage::immutable_store::query_one(
            &store,
            partition,
            Address {
                hash: address.hash,
                context: wanted,
            },
        )
        .await
        .expect("query a sibling context");
        assert_eq!(resolved.match_made, StoreMatch::MatchPartition);
        assert_eq!(resolved.context, held);

        store
            .clone()
            .copy(
                resolved.partition,
                resolved.source_address(address.hash),
                partition,
                wanted,
                CopyBehavior {
                    durable: false,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("the source a match named must be one copy resolves");
    }
}
