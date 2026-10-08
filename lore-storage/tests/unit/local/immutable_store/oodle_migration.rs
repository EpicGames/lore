// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_storage::CompressionMode;
use lore_storage::LocalImmutableStore;
use lore_storage::LocalImmutableStoreError;
use lore_storage::hash;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreBucket;
use lore_storage::local::immutable_store::ImmutableStoreGroup;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::oodle_migration::*;
use lore_storage::store_types::StoreMatchResult;
use tokio::sync::OwnedRwLockWriteGuard;

fn partition() -> Partition {
    Partition::from([7u8; 16])
}

/// Client-shaped: fragments are not durable until something says otherwise, which is what
/// makes the durable branch reachable on purpose rather than by default.
fn client_settings() -> ImmutableStoreSettings {
    ImmutableStoreSettings {
        protect_local_fragment: true,
        implicit_durable_stored: false,
        ..Default::default()
    }
}

/// Bytes every codec beats comfortably, so a fixture never lands on the incompressible path
/// by accident.
fn compressible(seed: u8, len: usize) -> Bytes {
    Bytes::from((0..len).map(|i| seed ^ (i / 64) as u8).collect::<Vec<u8>>())
}

fn address_of(payload: &Bytes, context: u8) -> Address {
    Address {
        hash: hash::hash_slice(payload.as_ref()),
        context: Context::from([context; 16]),
    }
}

async fn open_store(prefix: &str) -> (lore_base::test_util::TempDir, Arc<LocalImmutableStore>) {
    let dir = lore_base::test_util::TempDir::new(prefix);
    let store = reopen(dir.path()).await;
    (dir, store)
}

async fn reopen(path: &std::path::Path) -> Arc<LocalImmutableStore> {
    LocalImmutableStore::new(Some(path.to_path_buf()), client_settings())
        .await
        .expect("store opens")
}

/// The bucket `hash` belongs to, locked for writing. Mirrors how the store itself resolves a
/// bucket, re-reading the level in case a fan-out moved it while the lock was being taken.
async fn lock_bucket_for_hash(
    group: &Arc<ImmutableStoreGroup>,
    hash: &Hash,
) -> (usize, OwnedRwLockWriteGuard<ImmutableStoreBucket>) {
    loop {
        let level = group.bucket_count.load(Ordering::Relaxed);
        let index = lore_storage::local::fan_out::bucket_index_for(hash, level);
        let lock = group.bucket(index).clone().write_owned().await;
        if group.bucket_count.load(Ordering::Relaxed) == level {
            break (index, lock);
        }
    }
}

fn oodle_encode(payload: &Bytes) -> (Fragment, Bytes) {
    let raw = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    lore_storage::compress::compress_without_deprecation_checks(
        raw,
        payload.as_ref(),
        CompressionMode::Oodle,
    )
    .expect("the fixture payload is Oodle-compressible")
}

/// Write `payload` through the store, then re-encode what it stored as Oodle.
///
/// The store places the entry, so its group, bucket, insert slot, flags and dedup are whatever
/// production produces. Only the codec is substituted, because `compress` refuses to emit new
/// Oodle data - a store holding it, which is the only state this migration exists for, cannot
/// be reached through the public API at all.
async fn put_as_oodle(store: &Arc<LocalImmutableStore>, address: Address, payload: &Bytes) {
    put(store, address, payload, true).await;

    let (_, encoded) = oodle_encode(payload);
    let group = &store.group[address.hash.data()[0] as usize];
    let pack_ref = group
        .packstore
        .store(encoded.clone())
        .await
        .expect("the Oodle payload writes to the packstore");

    let (_, mut bucket) = lock_bucket_for_hash(group, &address.hash).await;
    for entry in bucket.entry.iter_mut() {
        if entry.address.hash != address.hash {
            continue;
        }
        entry.data.flags = (entry.data.flags & !FragmentFlags::PayloadCompressed.bits())
            | FragmentFlags::PayloadCompressedOodle2.bits();
        entry.data.size_payload = encoded.len() as u32;
        entry.data.pack_file = pack_ref.id;
        entry.data.pack_offset = pack_ref.offset;
    }
}

/// Persist everything outstanding, so a later dirty flag is the migration's doing and not the
/// fixture's.
async fn flush(store: &Arc<LocalImmutableStore>) {
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    dyn_store.flush(true).await.expect("store flushes");
}

/// Put `payload` at `address`. With `with_bytes` false the entry is metadata only: it declares
/// the content it refers to and holds none of it, which is how an entry comes to have no local
/// payload.
async fn put(
    store: &Arc<LocalImmutableStore>,
    address: Address,
    payload: &Bytes,
    with_bytes: bool,
) {
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    dyn_store
        .put(
            partition(),
            address,
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            with_bytes.then(|| payload.clone()),
            false,
        )
        .await
        .expect("put succeeds");
}

/// What the store reports about `address` through its own interface: how the stored bytes are
/// represented, and where it says they are held.
async fn observed(
    store: &Arc<LocalImmutableStore>,
    address: Address,
) -> (Fragment, StoreMatchResult) {
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    let metadata = dyn_store
        .clone()
        .get_metadata(partition(), address)
        .await
        .expect("metadata reads");
    let mut matched = [StoreMatchResult::default()];
    dyn_store
        .query(partition(), &[address], &mut matched)
        .await
        .expect("query succeeds");
    (metadata.fragment, matched[0])
}

/// The content behind `address`: the bytes the store serves, put through the read path's own
/// decode-and-verify rather than decoded by hand. The stored bytes, the recorded codec, the
/// recorded sizes and the address all have to agree for this to return.
async fn content(store: &Arc<LocalImmutableStore>, address: Address) -> Bytes {
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    let stored = dyn_store
        .get(partition(), address)
        .await
        .expect("the stored bytes read");
    let (_, content) = lore_storage::read::decompress_and_verify(
        stored.fragment,
        stored.payload.expect("a stored payload is served"),
        address,
        lore_storage::ReadOptions {
            decompress: true,
            verify: true,
            ..Default::default()
        },
    )
    .expect("the stored bytes decode to the content the address names");
    content
}

/// Migrate the bucket holding `address`, reporting what the pass did.
async fn migrate_bucket_holding(
    store: &Arc<LocalImmutableStore>,
    address: Address,
) -> Result<MigrationCounts, LocalImmutableStoreError> {
    let group_index = group_of(&address);
    let group = store.group[group_index].clone();
    let path = store.path.clone().expect("store has a path");
    let (bucket_index, guard) = lock_bucket_for_hash(&group, &address.hash).await;

    migrate_bucket(
        bucket_index,
        guard,
        group,
        path,
        group_index,
        store.gc_counters.clone(),
    )
    .await
}

/// Point the entry at a packfile the store does not have, the shape a store takes when one is
/// lost from under it.
async fn lose_the_packfile(store: &Arc<LocalImmutableStore>, address: &Address) {
    let group = &store.group[group_of(address)];
    let (_, mut bucket) = lock_bucket_for_hash(group, &address.hash).await;
    bucket
        .entry
        .iter_mut()
        .find(|entry| entry.address == *address)
        .expect("the entry was written")
        .data
        .pack_file = u32::MAX;
}

fn group_of(address: &Address) -> usize {
    address.hash.data()[0] as usize
}

/// Flag the entry as Oodle without giving it a payload: a metadata-only entry from when Oodle
/// was still a codec.
async fn flag_as_oodle(store: &Arc<LocalImmutableStore>, address: Address) {
    let group = &store.group[group_of(&address)];
    let (_, mut bucket) = lock_bucket_for_hash(group, &address.hash).await;
    bucket
        .entry
        .iter_mut()
        .filter(|entry| entry.address == address)
        .for_each(|entry| entry.data.flags |= FragmentFlags::PayloadCompressedOodle2.bits());
}

/// Repoint the entry at an Oodle payload holding content it does not describe, the shape a
/// packfile takes when a write lands at the wrong offset.
async fn point_at_foreign_content(
    store: &Arc<LocalImmutableStore>,
    address: Address,
    impostor: &Bytes,
) {
    let (_, encoded) = oodle_encode(impostor);
    let group = &store.group[group_of(&address)];
    let pack_ref = group
        .packstore
        .store(encoded.clone())
        .await
        .expect("the impostor payload writes");
    let (_, mut bucket) = lock_bucket_for_hash(group, &address.hash).await;
    let entry = bucket
        .entry
        .iter_mut()
        .find(|entry| entry.address == address)
        .expect("the entry was written");
    entry.data.pack_file = pack_ref.id;
    entry.data.pack_offset = pack_ref.offset;
    entry.data.size_payload = encoded.len() as u32;
}

mod migrate_bucket {
    use super::*;

    /// The codec a remote holds a payload under is not knowable from here, so an entry with
    /// nothing stored locally is left as it is.
    #[tokio::test]
    async fn an_entry_with_no_local_payload_is_left_alone() {
        let (_dir, store) = open_store("om_no_payload_").await;
        let payload = compressible(1, 4096);
        let address = address_of(&payload, 0);
        put(&store, address, &payload, false).await;
        flag_as_oodle(&store, address).await;
        flush(&store).await;
        let (before, _) = observed(&store, address).await;

        let counts = migrate_bucket_holding(&store, address)
            .await
            .expect("migration succeeds");

        let (after, matched) = observed(&store, address).await;
        assert_eq!(counts.migrated, 0);
        assert_eq!(counts.durably_left, 0);
        assert_eq!(counts.invalid, 0);
        assert_eq!(after.flags, before.flags);
        assert!(!matched.stored_local);
    }

    #[tokio::test]
    async fn an_entry_that_is_not_oodle_is_left_alone() {
        let (_dir, store) = open_store("om_not_oodle_").await;
        let payload = compressible(2, 4096);
        let address = address_of(&payload, 0);
        put(&store, address, &payload, true).await;
        flush(&store).await;
        let (before, _) = observed(&store, address).await;

        let counts = migrate_bucket_holding(&store, address)
            .await
            .expect("migration succeeds");

        let (after, matched) = observed(&store, address).await;
        assert_eq!(counts.migrated, 0);
        assert_eq!(counts.durably_left, 0);
        assert_eq!(counts.invalid, 0);
        assert_eq!(after.flags, before.flags);
        assert_eq!(after.size_payload, before.size_payload);
        assert!(matched.stored_local);
        assert_eq!(content(&store, address).await, payload);
    }

    /// A durable payload is held upstream, so the local copy is left exactly as it is, and a
    /// bucket holding nothing else stays clean and costs its group no rewrite.
    #[tokio::test]
    async fn a_durable_entry_is_left_alone_and_its_bucket_clean() {
        let (_dir, store) = open_store("om_durable_").await;
        let payload = compressible(3, 4096);
        let address = address_of(&payload, 0);
        put_as_oodle(&store, address, &payload).await;
        store.mark_all_as_durably_stored().await;
        flush(&store).await;
        let (before, _) = observed(&store, address).await;

        let counts = migrate_bucket_holding(&store, address)
            .await
            .expect("migration succeeds");

        let (after, matched) = observed(&store, address).await;
        assert_eq!(counts.durably_left, 1);
        assert_eq!(counts.migrated, 0);
        assert_eq!(after.flags, before.flags);
        assert_eq!(after.size_payload, before.size_payload);
        assert!(matched.stored_local);
        assert!(matched.stored_durable);
        assert_eq!(content(&store, address).await, payload);

        let group = &store.group[group_of(&address)];
        let (bucket_index, _bucket) = lock_bucket_for_hash(group, &address.hash).await;
        assert!(!group.dirty[bucket_index].load(Ordering::Relaxed));
    }

    /// The bucket a pass finds on a reopened store has not been deserialized yet, which is the
    /// shape every real migration starts from.
    #[tokio::test]
    async fn a_local_only_entry_is_re_encoded_as_zstd() {
        let dir = lore_base::test_util::TempDir::new("om_reencode_");
        let payload = compressible(4, 4096);
        let address = address_of(&payload, 0);

        let before = {
            let store = reopen(dir.path()).await;
            put_as_oodle(&store, address, &payload).await;
            flush(&store).await;
            observed(&store, address).await.0
        };
        assert_ne!(
            before.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0
        );

        let store = reopen(dir.path()).await;
        let counts = migrate_bucket_holding(&store, address)
            .await
            .expect("migration succeeds");

        let (after, matched) = observed(&store, address).await;
        assert_eq!(counts.migrated, 1);
        assert_eq!(
            after.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0
        );
        assert_ne!(after.flags & FragmentFlags::PayloadCompressedZstd.bits(), 0);
        assert_eq!(
            after.flags & !FragmentFlags::PayloadCompressed.bits(),
            before.flags & !FragmentFlags::PayloadCompressed.bits(),
            "re-encoding changes the codec and nothing else"
        );
        assert_eq!(after.size_content, before.size_content);
        assert!(matched.stored_local);
        assert_eq!(content(&store, address).await, payload);
    }

    /// Entries sharing a hash share one stored payload. The run has to leave them sharing one,
    /// or the store holds the same content twice and the entries disagree about where it is.
    #[tokio::test]
    async fn entries_sharing_a_hash_end_up_sharing_one_payload() {
        let (_dir, store) = open_store("om_dedup_").await;
        let payload = compressible(5, 4096);
        let addresses: Vec<Address> = (0..3).map(|c| address_of(&payload, c)).collect();
        for address in &addresses {
            put(&store, *address, &payload, true).await;
        }
        put_as_oodle(&store, addresses[0], &payload).await;

        let counts = migrate_bucket_holding(&store, addresses[0])
            .await
            .expect("migration succeeds");
        assert_eq!(
            counts.migrated, 3,
            "every entry in the run is accounted for"
        );

        // Where a payload is held is the one thing the store's interface does not report - it
        // describes what an entry holds, not the bytes it shares - so this reads the bucket.
        let entries = {
            let group = &store.group[group_of(&addresses[0])];
            let (_, guard) = lock_bucket_for_hash(group, &addresses[0].hash).await;
            guard.entry.to_vec()
        };
        let location_of = |address: &Address| {
            let data = entries
                .iter()
                .find(|entry| entry.address == *address)
                .expect("the entry is in the bucket it was written to")
                .data;
            (data.pack_file, data.pack_offset)
        };
        let location = location_of(&addresses[0]);
        for address in &addresses {
            assert_eq!(
                location_of(address),
                location,
                "the run has to converge on the payload the first entry wrote"
            );
            let (fragment, _) = observed(&store, *address).await;
            assert_eq!(
                fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
                0
            );
            assert_eq!(content(&store, *address).await, payload);
        }
    }

    /// Durability is a fact about one (partition, address) pair, not about the payload a run
    /// shares, so an adopting sibling must not inherit the leader's. A durable sibling still
    /// adopts the payload a local-only leader was re-encoded to.
    #[tokio::test]
    async fn an_adopting_sibling_keeps_its_own_durability() {
        let (_dir, store) = open_store("om_dedup_durable_").await;
        let payload = compressible(6, 4096);
        let addresses: Vec<Address> = (0..2).map(|c| address_of(&payload, c)).collect();
        for address in &addresses {
            put(&store, *address, &payload, true).await;
        }
        put_as_oodle(&store, addresses[0], &payload).await;
        store.mark_all_as_durably_stored().await;
        store
            .mark_as_not_durably_stored(partition(), addresses[0])
            .await;

        migrate_bucket_holding(&store, addresses[0])
            .await
            .expect("migration succeeds");

        let (_, leader) = observed(&store, addresses[0]).await;
        let (sibling_fragment, sibling) = observed(&store, addresses[1]).await;
        assert!(!leader.stored_durable);
        assert!(sibling.stored_durable);
        assert!(leader.stored_local && sibling.stored_local);
        assert_eq!(
            sibling_fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0,
            "the durable sibling kept its Oodle payload rather than adopting the leader's"
        );
        assert_ne!(
            sibling_fragment.flags & FragmentFlags::PayloadCompressedZstd.bits(),
            0
        );
        assert_eq!(content(&store, addresses[1]).await, payload);
    }

    /// Stored bytes that decode to content the address does not describe cannot be served or
    /// uploaded whatever happens here, so the pass counts them and moves on.
    #[tokio::test]
    async fn a_payload_that_decodes_to_the_wrong_content_is_skipped() {
        let (_dir, store) = open_store("om_bad_hash_").await;
        let payload = compressible(7, 4096);
        let address = address_of(&payload, 0);
        put_as_oodle(&store, address, &payload).await;

        point_at_foreign_content(&store, address, &compressible(8, 4096)).await;
        flush(&store).await;
        let (before, _) = observed(&store, address).await;

        let counts = migrate_bucket_holding(&store, address)
            .await
            .expect("an unusable entry does not fail the bucket");

        let (after, _) = observed(&store, address).await;
        assert_eq!(counts.invalid, 1);
        assert_eq!(counts.migrated, 0);
        assert_eq!(
            after.flags, before.flags,
            "an unusable entry is left as it is"
        );
        assert_eq!(after.size_payload, before.size_payload);
        assert_ne!(
            after.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0
        );
    }

    /// A payload that cannot be read says nothing about whether the content still exists, and
    /// a local-only payload is the only copy, so the pass stops rather than recording a loss.
    #[tokio::test]
    async fn a_payload_that_cannot_be_read_ends_the_pass() {
        let (_dir, store) = open_store("om_unreadable_").await;
        let payload = compressible(9, 4096);
        let address = address_of(&payload, 0);
        put_as_oodle(&store, address, &payload).await;

        lose_the_packfile(&store, &address).await;

        let err = migrate_bucket_holding(&store, address)
            .await
            .expect_err("an unreadable payload is not skipped silently");
        assert!(err.is_internal());
    }

    #[tokio::test]
    async fn a_bucket_with_nothing_to_migrate_does_nothing() {
        let (_dir, store) = open_store("om_clean_").await;
        let payload = compressible(10, 4096);
        let address = address_of(&payload, 0);

        let counts = migrate_bucket_holding(&store, address)
            .await
            .expect("migration succeeds");

        assert_eq!(counts.migrated, 0);
        assert_eq!(counts.invalid, 0);
        assert_eq!(counts.durably_left, 0);
    }
}

mod migrate_group {
    use super::*;

    /// Re-encoding only counts once it is on disk: the bucket file and the group's level marker
    /// have to be published together, or a reopen reads the group back at the level it had
    /// before and the entries above it are lost.
    #[tokio::test]
    async fn a_migrated_group_survives_a_reopen() {
        let dir = lore_base::test_util::TempDir::new("om_group_persist_");
        let payload = compressible(11, 4096);
        let address = address_of(&payload, 0);

        let before = {
            let store = reopen(dir.path()).await;
            put_as_oodle(&store, address, &payload).await;
            flush(&store).await;
            let before = observed(&store, address).await.0;

            let path = store.path.clone().expect("store has a path");
            migrate_group(&store, &path, group_of(&address))
                .await
                .expect("group migrates");
            before
        };
        assert_ne!(
            before.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0
        );

        let store = reopen(dir.path()).await;
        let (after, matched) = observed(&store, address).await;
        assert_eq!(
            after.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0,
            "the entry reopened still claiming Oodle"
        );
        assert_ne!(after.flags & FragmentFlags::PayloadCompressedZstd.bits(), 0);
        assert!(matched.stored_local);
        assert_eq!(content(&store, address).await, payload);
    }

    #[tokio::test]
    async fn a_bucket_failure_fails_the_group() {
        let (_dir, store) = open_store("om_group_fail_").await;
        let payload = compressible(12, 4096);
        let address = address_of(&payload, 0);
        put_as_oodle(&store, address, &payload).await;
        lose_the_packfile(&store, &address).await;

        let path = store.path.clone().expect("store has a path");
        let err = migrate_group(&store, &path, group_of(&address))
            .await
            .expect_err("a bucket that fails takes the group with it");
        assert!(err.is_internal());
    }
}

mod migrate_groups {
    use super::*;

    #[tokio::test]
    async fn a_complete_pass_records_nothing_left_to_migrate() {
        let (_dir, store) = open_store("om_groups_done_").await;
        let payload = compressible(13, 4096);
        let address = address_of(&payload, 0);
        put_as_oodle(&store, address, &payload).await;
        flush(&store).await;
        let path = store.path.clone().expect("store has a path");

        let completed = migrate_groups(store.clone(), &path, group_of(&address) as i32)
            .await
            .expect("the pass runs");

        assert!(completed);
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1,
            "a completed pass leaves nothing for the next one"
        );
        let (fragment, _) = observed(&store, address).await;
        assert_eq!(
            fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0
        );
        assert_eq!(content(&store, address).await, payload);
    }

    /// A failed group leaves the recorded index alone, so the next pass retries from there
    /// rather than stepping over a group it never migrated.
    #[tokio::test]
    async fn a_failing_group_ends_the_pass_without_recording_progress() {
        let (_dir, store) = open_store("om_groups_fail_").await;
        let payload = compressible(14, 4096);
        let address = address_of(&payload, 0);
        let group = group_of(&address) as i32;
        put_as_oodle(&store, address, &payload).await;
        flush(&store).await;
        lose_the_packfile(&store, &address).await;
        store
            .update_store_info(|info| info.next_group_index_to_migrate_oodle = group)
            .await
            .expect("info records where to resume");

        let path = store.path.clone().expect("store has a path");
        let completed = migrate_groups(store.clone(), &path, group)
            .await
            .expect("a failed group is not an error");

        assert!(!completed);
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            group,
            "the failed group has to be retried, not skipped"
        );
    }

    /// A real store is a mix of kinds. Each has to be treated by its own rule, and the presence
    /// of one must not change what happens to another - in particular one unusable fragment
    /// must not stop the rest of the pass.
    #[tokio::test]
    async fn a_store_of_mixed_fragments_migrates_each_by_its_kind() {
        let (_dir, store) = open_store("om_mixed_").await;

        // Durable first: `mark_all_as_durably_stored` reaches everything written so far, so
        // everything added after it stays local-only.
        let durable = compressible(20, 4096);
        let durable_address = address_of(&durable, 0);
        put_as_oodle(&store, durable_address, &durable).await;
        store.mark_all_as_durably_stored().await;

        let local = compressible(21, 4096);
        let local_address = address_of(&local, 0);
        put_as_oodle(&store, local_address, &local).await;

        let zstd = compressible(22, 4096);
        let zstd_address = address_of(&zstd, 0);
        put(&store, zstd_address, &zstd, true).await;

        let absent = compressible(23, 4096);
        let absent_address = address_of(&absent, 0);
        put(&store, absent_address, &absent, false).await;
        flag_as_oodle(&store, absent_address).await;

        let unusable = compressible(24, 4096);
        let unusable_address = address_of(&unusable, 0);
        put_as_oodle(&store, unusable_address, &unusable).await;
        point_at_foreign_content(&store, unusable_address, &compressible(25, 4096)).await;

        let shared = compressible(26, 4096);
        let shared_addresses: Vec<Address> = (0..3).map(|c| address_of(&shared, c)).collect();
        for address in &shared_addresses {
            put(&store, *address, &shared, true).await;
        }
        put_as_oodle(&store, shared_addresses[0], &shared).await;

        flush(&store).await;
        let path = store.path.clone().expect("store has a path");

        let completed = migrate_groups(store.clone(), &path, 255)
            .await
            .expect("the pass runs");

        assert!(completed);
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1
        );

        let (fragment, matched) = observed(&store, durable_address).await;
        assert_ne!(
            fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0,
            "a durable payload is left as it is, not re-encoded"
        );
        assert!(matched.stored_local);
        assert!(matched.stored_durable);

        let (fragment, matched) = observed(&store, local_address).await;
        assert_eq!(
            fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0
        );
        assert_ne!(
            fragment.flags & FragmentFlags::PayloadCompressedZstd.bits(),
            0
        );
        assert!(matched.stored_local);
        assert_eq!(content(&store, local_address).await, local);

        let (fragment, _) = observed(&store, zstd_address).await;
        assert_eq!(
            fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0
        );
        assert_eq!(content(&store, zstd_address).await, zstd);

        let (fragment, matched) = observed(&store, absent_address).await;
        assert!(
            !matched.stored_local,
            "an entry with no local payload gains one from nowhere"
        );
        assert_ne!(
            fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0,
            "there was nothing local to re-encode, so the codec stands"
        );

        let (fragment, _) = observed(&store, unusable_address).await;
        assert_ne!(
            fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0,
            "an entry whose bytes decode to other content is left as it is"
        );

        for address in &shared_addresses {
            let (fragment, matched) = observed(&store, *address).await;
            assert_eq!(
                fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
                0
            );
            assert!(matched.stored_local);
            assert_eq!(content(&store, *address).await, shared);
        }

        let entries = {
            let group = &store.group[group_of(&shared_addresses[0])];
            let (_, guard) = lock_bucket_for_hash(group, &shared_addresses[0].hash).await;
            guard.entry.to_vec()
        };
        let locations: Vec<(u32, u32)> = shared_addresses
            .iter()
            .map(|address| {
                let data = entries
                    .iter()
                    .find(|entry| entry.address == *address)
                    .expect("the entry is in the bucket it was written to")
                    .data;
                (data.pack_file, data.pack_offset)
            })
            .collect();
        assert!(
            locations.windows(2).all(|pair| pair[0] == pair[1]),
            "the run still shares one payload: {locations:?}"
        );
    }

    /// A group is materialised by the hash byte that selects it, so the range the walk covers
    /// is mostly groups that were never written. Touching one would create its directory and
    /// an fsynced level marker for content that does not exist.
    #[tokio::test]
    async fn a_group_that_was_never_written_is_left_absent() {
        let (_dir, store) = open_store("om_groups_absent_").await;
        let payload = compressible(16, 4096);
        let address = address_of(&payload, 0);
        put_as_oodle(&store, address, &payload).await;
        flush(&store).await;
        let path = store.path.clone().expect("store has a path");
        let before = group_dirs(&path);
        assert_eq!(before, vec![format!("{:02x}", group_of(&address))]);

        let completed = migrate_groups(store.clone(), &path, 255)
            .await
            .expect("the pass runs");

        assert!(completed);
        assert_eq!(
            group_dirs(&path),
            before,
            "a pass must not materialise the groups it walks past"
        );
    }

    /// Directory names under a store's index, sorted.
    fn group_dirs(path: &Arc<PathBuf>) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(path.join("index")) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Progress is recorded on disk, not just in memory, or a pass would start from the top
    /// again on the next open.
    #[tokio::test]
    async fn progress_is_recorded_on_disk() {
        let (_dir, store) = open_store("om_groups_progress_").await;
        let path = store.path.clone().expect("store has a path");

        migrate_groups(store.clone(), &path, 1)
            .await
            .expect("the pass runs");

        let on_disk = lore_storage::local::immutable_store::info::read_info_file(
            &lore_storage::local::immutable_store::info::info_path_for_store_root(&path),
        )
        .await
        .expect("info file reads")
        .expect("info file is present");
        assert_eq!(on_disk.next_group_index_to_migrate_oodle, -1);
    }
}

mod run_migrations {
    use super::*;

    /// A store written before the info file existed is seeded from the groups on disk, and a
    /// pass re-encodes what that seed points at.
    #[tokio::test]
    async fn a_store_predating_the_info_file_is_migrated() {
        let dir = lore_base::test_util::TempDir::new("om_open_");
        let payload = compressible(15, 4096);
        let address = address_of(&payload, 0);

        {
            let store = reopen(dir.path()).await;
            put_as_oodle(&store, address, &payload).await;
            flush(&store).await;
        }
        std::fs::remove_file(
            lore_storage::local::immutable_store::info::info_path_for_store_root(
                &dir.path().join("immutable"),
            ),
        )
        .expect("the info file a newer binary wrote is removed");

        let store = reopen(dir.path()).await;
        store.run_migrations().await.expect("migrations run");

        let (fragment, _) = observed(&store, address).await;
        assert_eq!(
            fragment.flags & FragmentFlags::PayloadCompressedOodle2.bits(),
            0,
            "a store that predates the info file has to be migrated"
        );
        assert_eq!(content(&store, address).await, payload);
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1
        );
    }

    /// A store with no written group has nothing to migrate, so a pass does not sweep all 256.
    #[tokio::test]
    async fn a_fresh_store_has_nothing_to_migrate() {
        let (_dir, store) = open_store("om_open_fresh_").await;
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1
        );

        store.run_migrations().await.expect("migrations run");

        assert!(
            !std::fs::read_dir(dir_index(&store)).is_ok_and(|mut d| d.next().is_some()),
            "a pass with nothing to do must not materialise every group"
        );
    }

    fn dir_index(store: &Arc<LocalImmutableStore>) -> PathBuf {
        store
            .path
            .clone()
            .expect("store has a path")
            .as_path()
            .join("index")
    }
}
