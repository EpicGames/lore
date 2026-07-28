// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests covering the lazy progressive fan-out behavior of `LocalImmutableStore` and
//! `LocalMutableStore`. Verifies the headline file-count win for client stores: cloning a 100k-file
//! repository keeps every group at level 1, so the on-disk artifact count is bounded by 256
//! per-group bucket files + 256 marker files (mutable) or +256 packfiles (immutable).

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use lore_storage::ImmutableStoreSettings;
    use lore_storage::MutableStoreSettings;
    use lore_storage::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn temp_dir() -> PathBuf {
        let name = format!("store_fan_out_test_{}", rand::random::<u64>());
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("Failed to create temp dir");
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    /// Generate a deterministic 32-byte hash from a seeded RNG, evenly distributed across the hash
    /// space (so that with N items and K buckets the distribution approximates uniform).
    fn make_hash(rng: &mut StdRng) -> lore_storage::Hash {
        let mut data = [0u8; 32];
        rand::Rng::fill(rng, &mut data[..]);
        let mut hash = lore_storage::Hash::default();
        hash.data_mut().copy_from_slice(&data);
        hash
    }

    fn count_files_recursive(dir: &Path) -> usize {
        let mut total = 0;
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += count_files_recursive(&path);
            } else {
                total += 1;
            }
        }
        total
    }

    /// REQ-F-1 acceptance: a fresh client `LocalMutableStore` at level 1, after writing 100,000
    /// entries (well under the per-group threshold trip), keeps every group at `bucket_count = 1`
    /// and writes ≤512 on-disk files (one bucket file + one marker file per group).
    #[tokio::test]
    async fn mutable_store_at_level_one_writes_at_most_512_files_for_100k_entries() {
        let store_path = temp_dir();
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("Failed to create in-memory immutable store");

        let mutable = lore_storage::LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: 1,
                fan_out_threshold: FAN_OUT_THRESHOLD_DEFAULT,
                ..Default::default()
            },
            immutable,
        )
        .await
        .expect("Failed to create mutable store");

        let mutable = Arc::new(mutable);
        let store: Arc<dyn lore_storage::MutableStore> = mutable.clone();

        let partition = lore_storage::Partition::default();
        let mut rng = StdRng::seed_from_u64(0xDEADBEEFCAFEBABE);
        let n = 100_000;
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            let key = make_hash(&mut rng);
            let value = make_hash(&mut rng);
            keys.push((key, value));
            store
                .clone()
                .store(partition, key, value, lore_storage::KeyType::BranchMetadata)
                .await
                .expect("Failed to store");
        }

        store
            .clone()
            .flush(true)
            .await
            .expect("Failed to flush mutable store");

        // Spot-check: lookups for ALL written keys succeed.
        for (key, value) in &keys {
            let loaded = store
                .clone()
                .load(partition, *key, lore_storage::KeyType::BranchMetadata)
                .await
                .expect("Lookup failed for written key");
            assert_eq!(loaded, *value, "Loaded value did not match stored value");
        }

        // Every group received writes (uniform distribution across 256 groups, 100k entries → ~390 per group, well below the 1000 threshold), so every group must remain at bucket_count = 1.
        for (idx, group) in mutable.group.iter().enumerate() {
            let count = group.bucket_count.load(Ordering::Relaxed);
            assert_eq!(
                count, 1,
                "Group {idx} expected bucket_count=1 (workload below threshold), got {count}"
            );
        }

        let mutable_path = store_path.join("mutable").join("index");
        let total_files = count_files_recursive(&mutable_path);
        assert!(
            total_files <= 512,
            "expected ≤512 on-disk files for level-1 mutable store, got {total_files} (every group at level 1 should write at most 1 bucket file + 1 marker file)"
        );

        cleanup(&store_path);
    }

    /// REQ-F-1 acceptance: a fresh client `LocalImmutableStore` at level 1, after writing 100,000
    /// fragments (well under the per-group threshold trip), keeps every group at `bucket_count = 1`
    /// and writes ≤768 on-disk files (1 bucket file + 1 marker file + 1 packfile per group).
    #[tokio::test]
    async fn immutable_store_at_level_one_writes_at_most_768_files_for_100k_entries() {
        let store_path = temp_dir();
        let store = lore_storage::LocalImmutableStore::new(
            Some(store_path.clone()),
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                fan_out_threshold: FAN_OUT_THRESHOLD_DEFAULT,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to create immutable store");

        let partition = lore_storage::Partition::default();
        let mut rng = StdRng::seed_from_u64(0xDEADBEEFCAFEBABE);
        let payload = bytes::Bytes::from_static(b"test fragment payload data 30!");
        let n = 100_000;
        let mut addresses = Vec::with_capacity(n);

        for _ in 0..n {
            let hash = make_hash(&mut rng);
            let address = lore_storage::Address {
                hash,
                context: lore_storage::Context::default(),
            };
            addresses.push(address);
            let fragment = lore_storage::Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            };
            store
                .clone()
                .store(partition, address, fragment, Some(payload.clone()), false)
                .await
                .expect("Failed to store fragment");
        }

        let store_dyn: Arc<dyn lore_storage::ImmutableStore> = store.clone();
        store_dyn
            .flush(true)
            .await
            .expect("Failed to flush immutable store");

        // Every group received writes (uniform distribution across 256 groups, 100k entries → ~390 per group, well below the 1000 threshold), so every group must remain at bucket_count = 1.
        for (idx, group) in store.group.iter().enumerate() {
            let count = group.bucket_count.load(Ordering::Relaxed);
            assert_eq!(
                count, 1,
                "Group {idx} expected bucket_count=1 (workload below threshold), got {count}"
            );
        }

        // All 100k addresses must be findable.
        for address in &addresses {
            let result = store
                .find(partition, *address, lore_storage::StoreMatch::MatchFull)
                .await
                .expect("find returned an error");
            assert_eq!(
                result.matching,
                lore_storage::StoreMatch::MatchFull,
                "Lookup failed for written address (expected MatchFull, got {:?})",
                result.matching
            );
        }

        let immutable_index = store_path.join("immutable").join("index");
        let total_files = count_files_recursive(&immutable_index);
        assert!(
            total_files <= 768,
            "expected ≤768 on-disk files for level-1 immutable store, got {total_files} (every group at level 1 should write at most 1 bucket + 1 marker + 1 packfile)"
        );

        cleanup(&store_path);
    }

    /// Per-group `bucket_count` must remain at the configured initial level when the workload
    /// stays below threshold. This is observable via the on-disk marker file content.
    #[tokio::test]
    async fn mutable_store_marker_records_initial_level_after_first_flush() {
        let store_path = temp_dir();
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("Failed to create in-memory immutable store");

        let mutable = lore_storage::LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            immutable,
        )
        .await
        .expect("Failed to create mutable store");

        let store: Arc<dyn lore_storage::MutableStore> = Arc::new(mutable);
        let partition = lore_storage::Partition::default();
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..10 {
            let key = make_hash(&mut rng);
            let value = make_hash(&mut rng);
            store
                .clone()
                .store(partition, key, value, lore_storage::KeyType::BranchMetadata)
                .await
                .expect("Failed to store");
        }
        store.clone().flush(true).await.expect("Failed to flush");

        let mutable_index = store_path.join("mutable").join("index");
        // Find at least one group dir with a marker file.
        let mut found_marker = false;
        for entry in std::fs::read_dir(&mutable_index)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.is_dir()
                && let Ok(Some(level)) = lore_storage::local::fan_out::read_level_marker(&path)
            {
                assert_eq!(level, 1, "Marker should record initial level 1");
                found_marker = true;
                break;
            }
        }
        assert!(found_marker, "Expected at least one group marker file");

        cleanup(&store_path);
    }

    /// Server-mode default (`initial_fan_out_level` = 256) on a FRESH disk goes to fan-out-aware
    /// mode (writes markers, bumps version) per the user's "always fan-out-aware for fresh stores"
    /// rule. The marker records 256.
    #[tokio::test]
    async fn fresh_server_mode_store_writes_marker_at_level_256() {
        let store_path = temp_dir();
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("Failed to create in-memory immutable store");

        let mutable = lore_storage::LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
                ..Default::default()
            },
            immutable,
        )
        .await
        .expect("Failed to create mutable store");

        let store: Arc<dyn lore_storage::MutableStore> = Arc::new(mutable);
        let partition = lore_storage::Partition::default();
        let mut rng = StdRng::seed_from_u64(13);
        for _ in 0..10 {
            let key = make_hash(&mut rng);
            let value = make_hash(&mut rng);
            store
                .clone()
                .store(partition, key, value, lore_storage::KeyType::BranchMetadata)
                .await
                .expect("Failed to store");
        }
        store.clone().flush(true).await.expect("Failed to flush");

        let mutable_index = store_path.join("mutable").join("index");
        let mut found_marker_at_256 = false;
        for entry in std::fs::read_dir(&mutable_index)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.is_dir()
                && let Ok(Some(level)) = lore_storage::local::fan_out::read_level_marker(&path)
            {
                assert_eq!(level, 256, "Marker should record level 256");
                found_marker_at_256 = true;
                break;
            }
        }
        assert!(
            found_marker_at_256,
            "Expected marker file recording level 256 in at least one group"
        );

        cleanup(&store_path);
    }

    /// REQ-F-5 acceptance: every maintenance function iterates per-group bucket arrays at the
    /// actual `bucket_count` (not a hard-coded `BUCKET_COUNT`) and leaves the store consistent.
    /// We force three groups to levels 1, 32, and 256 by setting `bucket_count` directly, populate
    /// each, then exercise (a) `flush`, (b) `evict_oldest`, and (c) `compact` — the last
    /// dispatches to `compact_packfiles` → `compact_group_packfiles` → `evict_group_sized`,
    /// covering the four maintenance ops the plan named.
    #[tokio::test]
    async fn mixed_level_maintenance_is_consistent() {
        let store_path = temp_dir();
        let store = lore_storage::LocalImmutableStore::new(
            Some(store_path.clone()),
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Force specific groups to specific levels by setting bucket_count directly.
        store.group[0].bucket_count.store(1, Ordering::Relaxed);
        store.group[1].bucket_count.store(32, Ordering::Relaxed);
        store.group[100].bucket_count.store(256, Ordering::Relaxed);

        // Populate each forced group with a few synthetic addresses that would route to bucket 0
        // under that group's level. This validates `bucket_index_for` consistency.
        let mut rng = StdRng::seed_from_u64(0xCAFE);
        for &group_idx in &[0usize, 1usize, 100usize] {
            for _ in 0..5 {
                let mut hash = make_hash(&mut rng);
                hash.data_mut()[0] = group_idx as u8;
                // Force key.data()[1] = 0 so it routes to bucket 0 under any level.
                hash.data_mut()[1] = 0;
                let address = lore_storage::Address {
                    hash,
                    context: lore_storage::Context::default(),
                };
                let payload = bytes::Bytes::from_static(b"test payload data");
                let fragment = lore_storage::Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                let _ = store
                    .clone()
                    .store(
                        lore_storage::Partition::default(),
                        address,
                        fragment,
                        Some(payload),
                        false,
                    )
                    .await;
            }
        }

        // Flush — exercises the per-group serialize loop with mixed bucket counts.
        let store_dyn: Arc<dyn lore_storage::ImmutableStore> = store.clone();
        store_dyn.clone().flush(true).await.unwrap();

        // evict_oldest with generous capacity: no actual eviction, but the bucket-iteration path runs at every level.
        let _evicted = store.evict_oldest(1_000_000_000, None).await;

        // compact with small max_size: forces compact_packfiles → compact_group_packfiles → evict_group_sized to iterate per-group bucket arrays at the actual bucket_count. We don't care whether it evicts anything; we only care that all three paths complete without error or panic at every group level.
        let _compact_step = store_dyn
            .clone()
            .compact(1, None, false, None)
            .await
            .expect("compact must complete without error at mixed bucket levels");

        // Maintenance ops never alter bucket_count (only fan-out does). All three forced levels survive.
        assert_eq!(store.group[0].bucket_count.load(Ordering::Relaxed), 1);
        assert_eq!(store.group[1].bucket_count.load(Ordering::Relaxed), 32);
        assert_eq!(store.group[100].bucket_count.load(Ordering::Relaxed), 256);

        cleanup(&store_path);
    }

    /// REQ-F-3 / REQ-NF-3 acceptance: a v2-format mutable store synthesised inline (mimicking what
    /// current `main` produces) loads cleanly, all entries readable, and read-only access performs
    /// zero file writes. The post-write+flush behaviour is covered separately by
    /// `legacy_v2_store_stays_legacy_after_write_and_flush`.
    #[tokio::test]
    async fn legacy_v2_store_loads_and_stays_legacy_until_modified() {
        // Build a synthetic v2 mutable store on disk: write a bucket file at TypedItems version
        // (which is what current `main` writes) and a "version" file at TypedItems. No markers.
        let store_path = temp_dir();
        let mutable_index = store_path.join("mutable").join("index").join("00");
        std::fs::create_dir_all(&mutable_index).unwrap();

        // Write the store-level "version" file with TypedItems = 2.
        let version_file = store_path.join("mutable").join("version");
        std::fs::write(&version_file, 2u32.to_le_bytes()).unwrap();

        // Write a single bucket file with version = 2 (TypedItems) and zero entries (count = 0).
        // Layout: header (16 bytes: version, _unused, count, _unused), then sorted_index, then entries.
        let mut bucket_bytes = Vec::new();
        bucket_bytes.extend_from_slice(&2u32.to_le_bytes()); // version = TypedItems
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // count = 0
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused_two
        // No sorted_index, no entries, since count = 0. The deserialize path treats count=0 as empty.
        // BUT: the deserialize path computes expected_count from file_size, and if file_size == header_size
        // it skips the header read entirely and returns empty. So just a 16-byte file works for "empty bucket".
        std::fs::write(mutable_index.join("index_00"), &bucket_bytes).unwrap();

        // Snapshot DATA files before opening (skip the FSLock infrastructure file). REQ-NF-3
        // covers entry data and config metadata, not lock files.
        let snapshot_before: std::collections::BTreeMap<PathBuf, Vec<u8>> =
            walk_data_files(&store_path);

        // Open with new code. No writes should happen during construction.
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = lore_storage::LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings::default(),
            immutable,
        )
        .await
        .unwrap();

        // Confirm legacy mode detected: serialize_version is TypedItems on every group.
        for group in &mutable.group {
            assert_eq!(
                group.serialize_version.load(Ordering::Relaxed),
                2,
                "Expected legacy serialize_version (TypedItems = 2), found {}",
                group.serialize_version.load(Ordering::Relaxed)
            );
        }

        // Read-only access: drop the store without any explicit writes.
        drop(mutable);

        let snapshot_after: std::collections::BTreeMap<PathBuf, Vec<u8>> =
            walk_data_files(&store_path);
        assert_eq!(
            snapshot_before, snapshot_after,
            "On-disk DATA state must be byte-identical after a read-only session (lock files excluded)"
        );

        cleanup(&store_path);
    }

    /// REQ-F-3 / REQ-NF-3 acceptance under Decision 8: a v2-format mutable store opened for
    /// read-write must STAY at v2. Writing a new entry and flushing must NOT add any marker files
    /// and must NOT bump bucket files to v3. This preserves compatibility with old binaries that
    /// don't understand the lazy-fan-out version.
    #[tokio::test]
    async fn legacy_v2_store_stays_legacy_after_write_and_flush() {
        let store_path = temp_dir();
        let mutable_dir = store_path.join("mutable");
        let mutable_index_root = mutable_dir.join("index");
        let mutable_index_00 = mutable_index_root.join("00");
        std::fs::create_dir_all(&mutable_index_00).unwrap();

        std::fs::write(mutable_dir.join("version"), 2u32.to_le_bytes()).unwrap();

        // Synthetic v2 bucket file in group 00 (16-byte header, count = 0).
        let mut bucket_bytes = Vec::new();
        bucket_bytes.extend_from_slice(&2u32.to_le_bytes()); // version = TypedItems
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // count = 0
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused_two
        std::fs::write(mutable_index_00.join("index_00"), &bucket_bytes).unwrap();

        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = Arc::new(
            lore_storage::LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings::default(),
                immutable,
            )
            .await
            .unwrap(),
        );
        let store: Arc<dyn lore_storage::MutableStore> = mutable.clone();

        // Decision 8: v2 store with no markers → serialize_version stays at TypedItems (=2) for every group.
        for group in &mutable.group {
            assert_eq!(
                group.serialize_version.load(Ordering::Relaxed),
                2,
                "Expected legacy serialize_version = 2"
            );
        }

        // Write a new entry to a previously-untouched group (0x42).
        let mut rng = StdRng::seed_from_u64(0x1337);
        let mut key = make_hash(&mut rng);
        key.data_mut()[0] = 0x42;
        let value = make_hash(&mut rng);
        store
            .clone()
            .store(
                lore_storage::Partition::default(),
                key,
                value,
                lore_storage::KeyType::BranchMetadata,
            )
            .await
            .unwrap();

        store.clone().flush(true).await.unwrap();

        // Decision 8 invariant 1: NO marker files appear anywhere in the index tree after flush.
        for entry in std::fs::read_dir(&mutable_index_root).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                let marker = path.join("level");
                assert!(
                    !marker.exists(),
                    "Marker file appeared in legacy v2 store at {}",
                    marker.display()
                );
            }
        }

        // Decision 8 invariant 2: any bucket file written for the dirty group must stay at v2.
        let group_42_dir = mutable_index_root.join("42");
        assert!(
            group_42_dir.exists(),
            "Group 0x42 dir should exist after flush"
        );
        let mut found_bucket = false;
        for file_entry in std::fs::read_dir(&group_42_dir).unwrap().flatten() {
            let path = file_entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("index_") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            if bytes.len() < 4 {
                continue;
            }
            let version = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            assert_eq!(
                version,
                2,
                "Bucket file {} should stay at v2 (TypedItems), got {version}",
                path.display()
            );
            found_bucket = true;
        }
        assert!(
            found_bucket,
            "Bucket file for group 0x42 not created on flush"
        );

        // Sanity: the entry remains readable after flush.
        let loaded = store
            .clone()
            .load(
                lore_storage::Partition::default(),
                key,
                lore_storage::KeyType::BranchMetadata,
            )
            .await
            .expect("Lookup failed for written key");
        assert_eq!(loaded, value);

        cleanup(&store_path);
    }

    /// Walks `dir` recursively, returning a map of every file `PATH→CONTENT`. Skips infrastructure
    /// files like the `FSLock` (`/lock` at the store root); those are expected side-effects of any
    /// store open and don't represent data writes.
    fn walk_data_files(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                out.extend(walk_data_files(&path));
            } else if name != "lock"
                && let Ok(bytes) = std::fs::read(&path)
            {
                out.insert(path, bytes);
            }
        }
        out
    }

    /// REQ-NF-5 acceptance: a fan-out-aware client store at level 1 writes bucket files at
    /// `LazyFanOut` version. An old binary capping its accepted version at `TypedItems` would
    /// refuse such a file with `IncompatibleStoreVersion` rather than silently misinterpreting
    /// the bucket layout. We simulate the old binary by manually parsing the bucket file header
    /// and asserting the version field is the bumped value.
    #[tokio::test]
    async fn fresh_client_store_writes_bucket_files_at_lazy_fan_out_version() {
        let store_path = temp_dir();
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = lore_storage::LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
            immutable,
        )
        .await
        .unwrap();
        let store: Arc<dyn lore_storage::MutableStore> = Arc::new(mutable);
        let mut rng = StdRng::seed_from_u64(99);
        for _ in 0..20 {
            let key = make_hash(&mut rng);
            let value = make_hash(&mut rng);
            store
                .clone()
                .store(
                    lore_storage::Partition::default(),
                    key,
                    value,
                    lore_storage::KeyType::BranchMetadata,
                )
                .await
                .unwrap();
        }
        store.clone().flush(true).await.unwrap();

        // Find a bucket file and read its first 4 bytes (the version field).
        let mutable_index = store_path.join("mutable").join("index");
        let mut found_v3 = false;
        for group_entry in std::fs::read_dir(&mutable_index)
            .into_iter()
            .flatten()
            .flatten()
        {
            for file_entry in std::fs::read_dir(group_entry.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = file_entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.starts_with("index_") {
                    continue;
                }
                let bytes = std::fs::read(&path).unwrap();
                if bytes.len() < 4 {
                    continue;
                }
                let version = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                assert_eq!(
                    version, 3,
                    "Expected LazyFanOut (v3) bucket file header, got {version}"
                );
                found_v3 = true;
            }
        }
        assert!(found_v3, "No bucket files found to inspect");
        cleanup(&store_path);
    }

    /// REQ-F-6 / T9(a) acceptance for `LocalMutableStore`: 5,000 entries to a single group at level
    /// 1 with the default threshold (1000) trigger a direct-jump fan-out to level 32 on flush —
    /// `level_for(1, 5000, 1000)` lands at the smallest ladder value above `5000/1000`, which is
    /// 32. After flush `bucket_count` is exactly 32; the on-disk marker records the same; every
    /// key remains findable.
    #[tokio::test]
    async fn mutable_5000_entries_in_one_group_fans_out_to_level_32() {
        let store_path = temp_dir();
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = lore_storage::LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: 1,
                fan_out_threshold: FAN_OUT_THRESHOLD_DEFAULT,
                ..Default::default()
            },
            immutable,
        )
        .await
        .unwrap();
        let mutable = Arc::new(mutable);
        let store: Arc<dyn lore_storage::MutableStore> = mutable.clone();

        // Pin every write to group 0x10 by fixing the first hash byte.
        let group_index: u8 = 0x10;
        let n = 5000;
        let mut rng = StdRng::seed_from_u64(42);
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            let mut key = make_hash(&mut rng);
            key.data_mut()[0] = group_index;
            let value = make_hash(&mut rng);
            keys.push((key, value));
            store
                .clone()
                .store(
                    lore_storage::Partition::default(),
                    key,
                    value,
                    lore_storage::KeyType::BranchMetadata,
                )
                .await
                .unwrap();
        }
        store.clone().flush(true).await.unwrap();

        // Direct-jump fan-out lands at exactly level 32 for these parameters.
        let final_level = mutable.group[group_index as usize]
            .bucket_count
            .load(Ordering::Relaxed);
        assert_eq!(
            final_level, 32,
            "Expected direct-jump to level 32, got {final_level}"
        );

        let marker_path = store_path
            .join("mutable")
            .join("index")
            .join(format!("{group_index:02x}"));
        let marker_level = lore_storage::local::fan_out::read_level_marker(&marker_path)
            .unwrap()
            .expect("Marker should exist after fan-out");
        assert_eq!(marker_level, 32, "Marker level mismatches in-memory level");

        for (key, value) in &keys {
            let loaded = store
                .clone()
                .load(
                    lore_storage::Partition::default(),
                    *key,
                    lore_storage::KeyType::BranchMetadata,
                )
                .await
                .expect("Lookup failed after fan-out");
            assert_eq!(loaded, *value);
        }

        cleanup(&store_path);
    }

    /// REQ-F-6 / T9(a) acceptance for `LocalImmutableStore`: same direct-jump fan-out semantics as
    /// the mutable side. 5,000 fragments to a single group at level 1 with the default threshold
    /// land at exactly level 32 on flush; every fragment remains findable.
    #[tokio::test]
    async fn immutable_5000_entries_in_one_group_fans_out_to_level_32() {
        let store_path = temp_dir();
        let store = lore_storage::LocalImmutableStore::new(
            Some(store_path.clone()),
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                fan_out_threshold: FAN_OUT_THRESHOLD_DEFAULT,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let group_index: u8 = 0x10;
        let n = 5000;
        let payload = bytes::Bytes::from_static(b"test fragment payload data 30!");
        let partition = lore_storage::Partition::default();
        let mut rng = StdRng::seed_from_u64(42);
        let mut addresses = Vec::with_capacity(n);
        for _ in 0..n {
            let mut hash = make_hash(&mut rng);
            hash.data_mut()[0] = group_index;
            let address = lore_storage::Address {
                hash,
                context: lore_storage::Context::default(),
            };
            addresses.push(address);
            let fragment = lore_storage::Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            };
            store
                .clone()
                .store(partition, address, fragment, Some(payload.clone()), false)
                .await
                .unwrap();
        }
        let store_dyn: Arc<dyn lore_storage::ImmutableStore> = store.clone();
        store_dyn.flush(true).await.unwrap();

        let final_level = store.group[group_index as usize]
            .bucket_count
            .load(Ordering::Relaxed);
        assert_eq!(
            final_level, 32,
            "Expected direct-jump to level 32, got {final_level}"
        );

        let marker_path = store_path
            .join("immutable")
            .join("index")
            .join(format!("{group_index:02x}"));
        let marker_level = lore_storage::local::fan_out::read_level_marker(&marker_path)
            .unwrap()
            .expect("Marker should exist after fan-out");
        assert_eq!(marker_level, 32, "Marker level mismatches in-memory level");

        for address in &addresses {
            let result = store
                .find(partition, *address, lore_storage::StoreMatch::MatchFull)
                .await
                .unwrap();
            assert_eq!(
                result.matching,
                lore_storage::StoreMatch::MatchFull,
                "Address not findable after fan-out"
            );
        }

        cleanup(&store_path);
    }

    /// T9(b) acceptance: load an existing on-disk store with entries spread across all level-32
    /// buckets but with no buckets deserialized in memory; trigger fan-out via flush; verify the
    /// fan-out path force-deserializes [0..N] before redistribute. If force-deserialize were
    /// broken, the on-disk-only buckets (which hold the bulk of phase 1's entries) would be
    /// overwritten as empty during the level-N→M serialize and their entries would be lost.
    #[tokio::test]
    async fn reload_with_partial_deserialization_force_deserializes_during_fan_out() {
        let store_path = temp_dir();
        let group_index: u8 = 0x10;
        let n_phase1 = 30_000;
        let n_phase3 = 200;
        let mut all_keys = Vec::with_capacity(n_phase1 + n_phase3);
        let mut rng = StdRng::seed_from_u64(0xBEEF);

        // Phase 1: fresh store at level 1, 30k entries to one group → fan-out to level 32 on flush.
        {
            let immutable: Arc<dyn lore_storage::ImmutableStore> =
                lore_storage::local::immutable_store::create(
                    None::<&str>,
                    ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .unwrap();
            let mutable = Arc::new(
                lore_storage::LocalMutableStore::new(
                    Some(&store_path),
                    MutableStoreSettings {
                        initial_fan_out_level: 1,
                        fan_out_threshold: FAN_OUT_THRESHOLD_DEFAULT,
                        ..Default::default()
                    },
                    immutable,
                )
                .await
                .unwrap(),
            );
            let store: Arc<dyn lore_storage::MutableStore> = mutable.clone();

            for _ in 0..n_phase1 {
                let mut key = make_hash(&mut rng);
                key.data_mut()[0] = group_index;
                let value = make_hash(&mut rng);
                all_keys.push((key, value));
                store
                    .clone()
                    .store(
                        lore_storage::Partition::default(),
                        key,
                        value,
                        lore_storage::KeyType::BranchMetadata,
                    )
                    .await
                    .unwrap();
            }
            store.clone().flush(true).await.unwrap();
            assert_eq!(
                mutable.group[group_index as usize]
                    .bucket_count
                    .load(Ordering::Relaxed),
                32,
                "Phase 1 should fan-out to level 32"
            );
        }

        // Phase 2: reopen. bucket_count=32 loaded from marker; no buckets deserialized yet.
        {
            let immutable: Arc<dyn lore_storage::ImmutableStore> =
                lore_storage::local::immutable_store::create(
                    None::<&str>,
                    ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .unwrap();
            let mutable = Arc::new(
                lore_storage::LocalMutableStore::new(
                    Some(&store_path),
                    MutableStoreSettings {
                        initial_fan_out_level: 1,
                        fan_out_threshold: FAN_OUT_THRESHOLD_DEFAULT,
                        ..Default::default()
                    },
                    immutable,
                )
                .await
                .unwrap(),
            );
            let store: Arc<dyn lore_storage::MutableStore> = mutable.clone();
            assert_eq!(
                mutable.group[group_index as usize]
                    .bucket_count
                    .load(Ordering::Relaxed),
                32,
                "bucket_count must restore from marker"
            );

            // Phase 3: write entries pinned to level-32 bucket 0 (top 5 bits of key.data()[1] = 0, i.e., values [0..7]). Total in bucket 0 climbs from ~937 (n_phase1/32) to ~1137 (>1000 threshold).
            for i in 0..n_phase3 {
                let mut key = make_hash(&mut rng);
                key.data_mut()[0] = group_index;
                key.data_mut()[1] = (i % 8) as u8;
                let value = make_hash(&mut rng);
                all_keys.push((key, value));
                store
                    .clone()
                    .store(
                        lore_storage::Partition::default(),
                        key,
                        value,
                        lore_storage::KeyType::BranchMetadata,
                    )
                    .await
                    .unwrap();
            }

            // Phase 4: flush. Level-32 bucket 0 exceeds threshold → fan-out to level 64. The fan-out path MUST force-deserialize all level-32 buckets before redistribute or phase-1 entries in non-deserialized buckets will be lost.
            store.clone().flush(true).await.unwrap();
            let new_level = mutable.group[group_index as usize]
                .bucket_count
                .load(Ordering::Relaxed);
            assert!(
                new_level > 32,
                "Expected second fan-out (level > 32), got {new_level}"
            );

            // Phase 5: every key from phase 1 + phase 3 must remain findable. A broken force-deserialize would drop ~29063 phase-1 entries from non-deserialized buckets.
            for (key, value) in &all_keys {
                let loaded = store
                    .clone()
                    .load(
                        lore_storage::Partition::default(),
                        *key,
                        lore_storage::KeyType::BranchMetadata,
                    )
                    .await
                    .expect("Lookup failed after reload+fan-out");
                assert_eq!(
                    loaded, *value,
                    "Value mismatch for key after reload+fan-out (force-deserialize regression)"
                );
            }
        }

        cleanup(&store_path);
    }

    /// T9(c)+(d) acceptance: 100 concurrent writer tasks against a single group while flushes run
    /// in parallel and trigger fan-out mid-flight. CAS-retry must keep all writes coherent: every
    /// written key remains findable post-flush, fan-out actually fires, no panics, no lost entries.
    #[tokio::test]
    async fn concurrent_writers_with_fan_out_preserve_all_keys() {
        let store_path = temp_dir();
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = Arc::new(
            lore_storage::LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings {
                    initial_fan_out_level: 1,
                    fan_out_threshold: FAN_OUT_THRESHOLD_DEFAULT,
                    ..Default::default()
                },
                immutable,
            )
            .await
            .unwrap(),
        );
        let store: Arc<dyn lore_storage::MutableStore> = mutable.clone();
        let group_index: u8 = 0x10;
        let partition = lore_storage::Partition::default();

        // Pre-populate near (but under) threshold so the fan-out trigger lands during the concurrent writer phase rather than only on the final flush.
        let mut pre_keys = Vec::with_capacity(900);
        let mut pre_rng = StdRng::seed_from_u64(0xFEED);
        for _ in 0..900 {
            let mut key = make_hash(&mut pre_rng);
            key.data_mut()[0] = group_index;
            let value = make_hash(&mut pre_rng);
            pre_keys.push((key, value));
            store
                .clone()
                .store(partition, key, value, lore_storage::KeyType::BranchMetadata)
                .await
                .unwrap();
        }

        // Spawn 100 writer tasks, each writing 50 entries to the same group. Total writes: 5000.
        let writers_count = 100usize;
        let entries_per_writer = 50usize;
        let mut writer_handles = Vec::with_capacity(writers_count);
        for w in 0..writers_count {
            let store = store.clone();
            let handle = lore_base::lore_spawn!(async move {
                let mut rng = StdRng::seed_from_u64(0xC0DE_0000 + w as u64);
                let mut local_keys = Vec::with_capacity(entries_per_writer);
                for _ in 0..entries_per_writer {
                    let mut key = make_hash(&mut rng);
                    key.data_mut()[0] = group_index;
                    let value = make_hash(&mut rng);
                    store
                        .clone()
                        .store(partition, key, value, lore_storage::KeyType::BranchMetadata)
                        .await
                        .unwrap();
                    local_keys.push((key, value));
                }
                local_keys
            });
            writer_handles.push(handle);
        }

        // Run a parallel flush loop. Multiple iterations maximize the chance of catching writers mid-flight, exercising the CAS-retry path during a real fan-out.
        let flush_store = store.clone();
        let flush_handle = lore_base::lore_spawn!(async move {
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                let _ = flush_store.clone().flush(true).await;
            }
        });

        let mut all_keys = pre_keys;
        for handle in writer_handles {
            let writer_keys = handle.await.expect("Writer task panicked");
            all_keys.extend(writer_keys);
        }
        flush_handle.await.expect("Flush task panicked");

        // Final settle flush after all writers are done.
        store.clone().flush(true).await.unwrap();

        let final_level = mutable.group[group_index as usize]
            .bucket_count
            .load(Ordering::Relaxed);
        assert!(
            final_level > 1,
            "Expected fan-out to fire (level > 1), got {final_level}"
        );

        for (key, value) in &all_keys {
            let loaded = store
                .clone()
                .load(partition, *key, lore_storage::KeyType::BranchMetadata)
                .await
                .expect("Lookup failed for concurrently-written key");
            assert_eq!(
                loaded, *value,
                "Value mismatch for concurrently-written key (CAS-retry regression)"
            );
        }

        cleanup(&store_path);
    }

    /// REQ-F-2 / REQ-NF-2: smoke-grade latency check at level 256. Pre-populates a server-mode
    /// store with 100k entries and times 10k random lookups. Records the median, p99, and total
    /// time so a reviewer can compare against `main` before merging. Not a regression gate by
    /// itself — Criterion-style harness deferred to a follow-up; this is a visibility data point.
    #[tokio::test]
    async fn level_256_lookup_latency_visibility() {
        let store_path = temp_dir();
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = lore_storage::LocalMutableStore::new(
            Some(&store_path),
            MutableStoreSettings {
                initial_fan_out_level: lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
                ..Default::default()
            },
            immutable,
        )
        .await
        .unwrap();
        let store: Arc<dyn lore_storage::MutableStore> = Arc::new(mutable);
        let partition = lore_storage::Partition::default();
        let mut rng = StdRng::seed_from_u64(123);
        let n = 100_000;
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            let key = make_hash(&mut rng);
            let value = make_hash(&mut rng);
            keys.push((key, value));
            store
                .clone()
                .store(partition, key, value, lore_storage::KeyType::BranchMetadata)
                .await
                .unwrap();
        }

        // Measure 10k lookups against the populated store.
        let probe_count = 10_000;
        let mut samples = Vec::with_capacity(probe_count);
        let probe_indices: Vec<usize> = (0..probe_count)
            .map(|_| rand::Rng::random_range(&mut rng, 0..n))
            .collect();
        let total_start = std::time::Instant::now();
        for &idx in &probe_indices {
            let (key, _) = keys[idx];
            let t = std::time::Instant::now();
            let _ = store
                .clone()
                .load(partition, key, lore_storage::KeyType::BranchMetadata)
                .await
                .unwrap();
            samples.push(t.elapsed().as_nanos() as u64);
        }
        let total = total_start.elapsed();
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        let p99 = samples[(samples.len() * 99) / 100];
        eprintln!(
            "level-256 lookup latency: median={median}ns, p99={p99}ns, total={total:?} for {probe_count} probes"
        );
        // Sanity gate: median should be well under 1ms even on slow CI hardware. Real regression
        // analysis happens manually against `main`.
        assert!(
            median < 1_000_000,
            "median lookup latency {median}ns is implausibly slow (>1ms)"
        );
        cleanup(&store_path);
    }

    /// Reload a fan-out-aware store written by an earlier session and confirm `bucket_count` is
    /// restored from the on-disk marker.
    #[tokio::test]
    async fn reload_restores_bucket_count_from_marker() {
        let store_path = temp_dir();

        // Write phase: fresh client store, level 1.
        {
            let immutable: Arc<dyn lore_storage::ImmutableStore> =
                lore_storage::local::immutable_store::create(
                    None::<&str>,
                    ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .unwrap();
            let mutable = lore_storage::LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings {
                    initial_fan_out_level: 1,
                    ..Default::default()
                },
                immutable,
            )
            .await
            .unwrap();
            let store: Arc<dyn lore_storage::MutableStore> = Arc::new(mutable);
            let mut rng = StdRng::seed_from_u64(42);
            for _ in 0..50 {
                let key = make_hash(&mut rng);
                let value = make_hash(&mut rng);
                store
                    .clone()
                    .store(
                        lore_storage::Partition::default(),
                        key,
                        value,
                        lore_storage::KeyType::BranchMetadata,
                    )
                    .await
                    .unwrap();
            }
            store.clone().flush(true).await.unwrap();
        }

        // Reload phase: reopen the same path, verify groups loaded at bucket_count = 1.
        {
            let immutable: Arc<dyn lore_storage::ImmutableStore> =
                lore_storage::local::immutable_store::create(
                    None::<&str>,
                    ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .unwrap();
            let reopened = lore_storage::LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings::default(),
                immutable,
            )
            .await
            .unwrap();
            // Only groups that received a write during the previous session have markers; the
            // others stay at the legacy default (256) since they hold no data and their level is
            // irrelevant. Confirm at least one group was restored to bucket_count = 1.
            let level_one_groups = reopened
                .group
                .iter()
                .filter(|g| g.bucket_count.load(Ordering::Relaxed) == 1)
                .count();
            assert!(
                level_one_groups > 0,
                "Expected at least one group restored to bucket_count = 1 from marker"
            );
        }

        cleanup(&store_path);
    }

    /// Helper: create a mutable store at `initial_fan_out_level=1`, write enough entries pinned
    /// to one group to trigger fan-out to level 32, flush, and return:
    /// * the store path,
    /// * the group dir path (`<store>/mutable/index/<gg>/`),
    /// * the list of (key, value) pairs written.
    ///
    /// On return the store has been dropped, so the on-disk state reflects a clean post-fan-out
    /// state at level 32 with marker file in place.
    async fn setup_post_fan_out_to_level_32(
        seed: u64,
        threshold: usize,
    ) -> (
        PathBuf,
        PathBuf,
        Vec<(lore_storage::Hash, lore_storage::Hash)>,
    ) {
        let store_path = temp_dir();
        let group_index: u8 = 0x10;
        let n = threshold + threshold / 5; // 20% over threshold, guaranteed to trip fan-out
        let mut keys = Vec::with_capacity(n);

        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = Arc::new(
            lore_storage::LocalMutableStore::new(
                Some(&store_path),
                MutableStoreSettings {
                    initial_fan_out_level: 1,
                    fan_out_threshold: threshold,
                    ..Default::default()
                },
                immutable,
            )
            .await
            .unwrap(),
        );
        let store: Arc<dyn lore_storage::MutableStore> = mutable.clone();
        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..n {
            let mut key = make_hash(&mut rng);
            key.data_mut()[0] = group_index;
            let value = make_hash(&mut rng);
            keys.push((key, value));
            store
                .clone()
                .store(
                    lore_storage::Partition::default(),
                    key,
                    value,
                    lore_storage::KeyType::BranchMetadata,
                )
                .await
                .unwrap();
        }
        store.clone().flush(true).await.unwrap();
        assert_eq!(
            mutable.group[group_index as usize]
                .bucket_count
                .load(Ordering::Relaxed),
            32,
            "Setup helper expected fan-out to level 32"
        );
        let group_dir = store_path
            .join("mutable")
            .join("index")
            .join(format!("{group_index:02x}"));
        drop(store);
        drop(mutable);
        (store_path, group_dir, keys)
    }

    /// Helper: open the store at `store_path` (recovery runs at construction) and verify every
    /// `(key, value)` in `expected_keys` is findable, then drop the store.
    async fn verify_all_keys_findable_and_drop(
        store_path: &Path,
        expected_keys: &[(lore_storage::Hash, lore_storage::Hash)],
    ) {
        let immutable: Arc<dyn lore_storage::ImmutableStore> =
            lore_storage::local::immutable_store::create(
                None::<&str>,
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .unwrap();
        let mutable = lore_storage::LocalMutableStore::new(
            Some(store_path),
            MutableStoreSettings::default(),
            immutable,
        )
        .await
        .unwrap();
        let store: Arc<dyn lore_storage::MutableStore> = Arc::new(mutable);
        for (key, value) in expected_keys {
            let loaded = store
                .clone()
                .load(
                    lore_storage::Partition::default(),
                    *key,
                    lore_storage::KeyType::BranchMetadata,
                )
                .await
                .expect("Lookup failed after recovery");
            assert_eq!(loaded, *value);
        }
    }

    /// T10 crash point 1a/1b: dangling `.new` files with no `level.pending`. Recovery must be a
    /// no-op — the dangling files are harmless leftovers from a fan-out that never reached the
    /// commit point. Subsequent reopens still see the previous level's marker and bucket files.
    #[tokio::test]
    async fn t10_recovery_dangling_new_files_without_pending_is_noop() {
        let (store_path, group_dir, keys) = setup_post_fan_out_to_level_32(0xAA01, 200).await;

        // Construct: copy each level-32 final file to .new (dangling .new, no pending).
        for entry in std::fs::read_dir(&group_dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("index_") && !name.ends_with(".new") {
                let new_path = path.with_file_name(format!("{name}.new"));
                std::fs::copy(&path, &new_path).unwrap();
            }
        }
        assert!(
            !group_dir
                .join(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME)
                .exists(),
            "Setup must NOT write level.pending"
        );

        verify_all_keys_findable_and_drop(&store_path, &keys).await;

        cleanup(&store_path);
    }

    /// T10 crash point 2: `level.pending` written, all `.new` files in place, no renames done
    /// yet. Recovery must roll forward — rename every `.new` to its final name, write marker,
    /// delete pending. Final state: no `.new`, no pending, marker matches target.
    #[tokio::test]
    async fn t10_recovery_pending_with_all_new_files_rolls_forward() {
        let (store_path, group_dir, keys) = setup_post_fan_out_to_level_32(0xAA02, 200).await;

        // Construct: copy each level-32 final file to .new, write pending=32. (Final files stay in place — recovery's renames overwrite them with identical content, which is fine.)
        for entry in std::fs::read_dir(&group_dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("index_") && !name.ends_with(".new") {
                let new_path = path.with_file_name(format!("{name}.new"));
                std::fs::copy(&path, &new_path).unwrap();
            }
        }
        lore_storage::local::fan_out::write_level_pending(&group_dir, 32, false).unwrap();

        verify_all_keys_findable_and_drop(&store_path, &keys).await;

        // Recovery cleaned up: no .new files, no pending, marker still says 32.
        for entry in std::fs::read_dir(&group_dir).unwrap().flatten() {
            let name = entry.file_name();
            let name_str = name.to_str().unwrap_or("");
            assert!(
                !name_str.ends_with(".new"),
                ".new file leftover after recovery: {name_str}"
            );
            assert_ne!(
                name_str,
                lore_storage::local::fan_out::LEVEL_PENDING_FILENAME,
                "level.pending leftover after recovery"
            );
        }
        assert_eq!(
            lore_storage::local::fan_out::read_level_marker(&group_dir).unwrap(),
            Some(32)
        );

        cleanup(&store_path);
    }

    /// T10 crash point 3a: mid-renames. Some `.new` already renamed to final, some still as
    /// `.new`, pending exists. Recovery must rename only the remaining `.new` files (the
    /// already-renamed ones are silently skipped because the source `.new` no longer exists).
    #[tokio::test]
    async fn t10_recovery_pending_with_partial_renames_completes_them() {
        let (store_path, group_dir, keys) = setup_post_fan_out_to_level_32(0xAA03, 200).await;

        // Construct: half the bucket files have .new (still pending rename), half don't (already renamed). Use index parity to pick the half.
        for entry in std::fs::read_dir(&group_dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("index_") && !name.ends_with(".new") {
                // Parse the bucket index from the filename (4 hex chars after `index_`).
                if let Some(hex) = name.strip_prefix("index_")
                    && let Ok(bb) = u8::from_str_radix(hex, 16)
                    && bb % 2 == 0
                {
                    let new_path = path.with_file_name(format!("{name}.new"));
                    std::fs::copy(&path, &new_path).unwrap();
                }
            }
        }
        lore_storage::local::fan_out::write_level_pending(&group_dir, 32, false).unwrap();

        verify_all_keys_findable_and_drop(&store_path, &keys).await;

        // No .new files leftover, no pending, marker correct.
        for entry in std::fs::read_dir(&group_dir).unwrap().flatten() {
            let name = entry.file_name();
            let name_str = name.to_str().unwrap_or("");
            assert!(
                !name_str.ends_with(".new"),
                ".new file leftover after recovery: {name_str}"
            );
        }
        assert!(
            !group_dir
                .join(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME)
                .exists()
        );
        assert_eq!(
            lore_storage::local::fan_out::read_level_marker(&group_dir).unwrap(),
            Some(32)
        );

        cleanup(&store_path);
    }

    /// T10 crash points 4 and 5: all renames done, marker may or may not be written, pending
    /// still present. Recovery is effectively idempotent — the rename loop is a no-op (no `.new`
    /// files to rename), the marker write is a no-op (already at the target), and pending gets
    /// deleted.
    #[tokio::test]
    async fn t10_recovery_pending_with_no_new_files_is_idempotent() {
        let (store_path, group_dir, keys) = setup_post_fan_out_to_level_32(0xAA04, 200).await;

        // Construct: write pending=32, leave everything else alone (final files at level-32, marker=32).
        lore_storage::local::fan_out::write_level_pending(&group_dir, 32, false).unwrap();

        verify_all_keys_findable_and_drop(&store_path, &keys).await;

        // No leftovers.
        assert!(
            !group_dir
                .join(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME)
                .exists()
        );
        assert_eq!(
            lore_storage::local::fan_out::read_level_marker(&group_dir).unwrap(),
            Some(32)
        );

        cleanup(&store_path);
    }

    /// REQ-NF-5 / Decision 8: an existing immutable store at the current pre-fan-out version
    /// (v4 `LastAccessInEntry`) with no markers stays at v4 — `serialize_version` is set to
    /// `LastAccessInEntry`, not `LazyFanOut`. Old binaries can still read this store.
    #[tokio::test]
    async fn current_immutable_v4_store_stays_at_v4() {
        let store_path = temp_dir();
        let group_dir = store_path.join("immutable").join("index").join("00");
        std::fs::create_dir_all(&group_dir).unwrap();
        // Synthetic v4 (LastAccessInEntry) bucket file: 16-byte header, count = 0.
        let mut bucket_bytes = Vec::new();
        bucket_bytes.extend_from_slice(&4u32.to_le_bytes()); // version = LastAccessInEntry
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // count
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused_two
        std::fs::write(group_dir.join("index_00"), &bucket_bytes).unwrap();

        let store = lore_storage::LocalImmutableStore::new(
            Some(store_path.clone()),
            ImmutableStoreSettings::default(),
        )
        .await
        .unwrap();

        let v = store.group[0].serialize_version.load(Ordering::Relaxed);
        assert_eq!(
            v,
            lore_storage::local::immutable_store::ImmutableStoreVersion::LastAccessInEntry as u32,
            "Current v4 store should stay at LastAccessInEntry, got {v}"
        );

        cleanup(&store_path);
    }

    /// REQ-NF-5 / Decision 8: an existing immutable store at an older version (v1-v3) gets
    /// upgraded all the way to `LazyFanOut` (v5). Detected by sampling bucket file headers at
    /// construction; any version < `LastAccessInEntry` triggers the full upgrade so the next
    /// flush writes markers and bumps re-serialized buckets to the forward-compat sentinel.
    #[tokio::test]
    async fn legacy_immutable_v3_store_upgrades_to_lazy_fan_out() {
        let store_path = temp_dir();
        let group_dir = store_path.join("immutable").join("index").join("00");
        std::fs::create_dir_all(&group_dir).unwrap();
        // Synthetic v3 (PackfilePerGroup) bucket file: 16-byte header, count = 0.
        let mut bucket_bytes = Vec::new();
        bucket_bytes.extend_from_slice(&3u32.to_le_bytes()); // version = PackfilePerGroup
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // count
        bucket_bytes.extend_from_slice(&0u32.to_le_bytes()); // _unused_two
        std::fs::write(group_dir.join("index_00"), &bucket_bytes).unwrap();

        let store = lore_storage::LocalImmutableStore::new(
            Some(store_path.clone()),
            ImmutableStoreSettings::default(),
        )
        .await
        .unwrap();

        let v = store.group[0].serialize_version.load(Ordering::Relaxed);
        assert_eq!(
            v,
            lore_storage::local::immutable_store::ImmutableStoreVersion::LazyFanOut as u32,
            "Older v3 store should upgrade to LazyFanOut (v5), got {v}"
        );

        cleanup(&store_path);
    }

    /// T10 idempotency: running recovery twice (open → drop → open) leaves the store in the
    /// same consistent state both times. The second open finds no pending, performs no recovery,
    /// and entries remain findable.
    #[tokio::test]
    async fn t10_recovery_is_idempotent_across_reopens() {
        let (store_path, group_dir, keys) = setup_post_fan_out_to_level_32(0xAA05, 200).await;

        // Construct: full crash-point-2 state (pending + .new files).
        for entry in std::fs::read_dir(&group_dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("index_") && !name.ends_with(".new") {
                let new_path = path.with_file_name(format!("{name}.new"));
                std::fs::copy(&path, &new_path).unwrap();
            }
        }
        lore_storage::local::fan_out::write_level_pending(&group_dir, 32, false).unwrap();

        // First open: recovery runs.
        verify_all_keys_findable_and_drop(&store_path, &keys).await;
        // Second open: recovery is a no-op (no pending), keys still findable.
        verify_all_keys_findable_and_drop(&store_path, &keys).await;

        cleanup(&store_path);
    }
}
