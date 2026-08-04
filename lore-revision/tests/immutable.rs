// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::lore::*;
use lore_storage::*;

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;

    use bytes::Bytes;
    use lore_base::error::NoRemote;
    use lore_revision::branch::BranchLatestHistory;
    use lore_revision::immutable;
    use lore_revision::immutable::ReadFromImmutable;
    use lore_revision::immutable::read_options_from_repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::options::WriteOptions;
    use lore_transport::ProtocolError;
    use rand::Rng;
    use rand::random;

    use super::*;

    include!("helper.rs");

    /// Corrupts the packfile data at the specified location by overwriting bytes.
    fn corrupt_packfile(
        store_path: &std::path::Path,
        group_index: u8,
        pack_file: u32,
        pack_offset: u32,
    ) {
        use std::io::Read;
        use std::io::Seek;
        use std::io::SeekFrom;
        use std::io::Write;

        // Pack path is <store_path>/immutable/index/<group_hex>/pack/<pack_id>
        let pack_path = store_path.join(format!(
            "immutable/index/{group_index:02x}/pack/{pack_file}"
        ));

        // Read existing content to verify file exists and has content
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pack_path)
            .expect("Failed to open packfile for corruption");

        // Read a byte to verify we're at the right spot
        file.seek(SeekFrom::Start(pack_offset as u64))
            .expect("Failed to seek to pack_offset");
        let mut buf = [0u8; 1];
        file.read_exact(&mut buf)
            .expect("Failed to read from packfile");

        // Now write corruption bytes
        file.seek(SeekFrom::Start(pack_offset as u64))
            .expect("Failed to seek to pack_offset");
        file.write_all(&[0xFF; 16])
            .expect("Failed to write corruption bytes");
        file.sync_all().expect("Failed to sync packfile");
    }

    #[tokio::test]
    async fn write_and_read_chunked() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository = random::<RepositoryId>();
                let context = random::<Context>();

                let repository = Arc::new(RepositoryContext::new(
                    Some(dir.as_path().to_path_buf()),
                    immutable_store,
                    mutable_store,
                    repository,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                let options = immutable::read_options_from_repository(&repository);

                let not_exist_address = rand::random::<Address>();
                let failure = immutable::read(repository.clone(), not_exist_address, None, options)
                    .await
                    .expect_err("Failed reading content back from store");
                assert!(
                    failure.is_address_not_found() || failure.is_payload_not_found(),
                    "Expected a not-found error, got: {failure}"
                );

                let payload: Vec<u8> = (0..(1024 * 1024))
                    .map(|_| rng.random_range(0..=255))
                    .collect();
                let payload = Bytes::copy_from_slice(payload.as_slice());

                let (address, _fragment) = immutable::write(
                    repository.clone(),
                    context,
                    payload.clone(),
                    WriteOptions::default(),
                )
                .await
                .expect("Failed writing to store");

                let read_buffer = immutable::read(repository.clone(), address, None, options)
                    .await
                    .expect("Failed reading content back from store");

                assert_eq!(read_buffer.len(), payload.len());
                assert_eq!(read_buffer.as_ref(), payload.as_ref());

                let range = 0..payload.len();
                let read_buffer =
                    immutable::read(repository.clone(), address, Some(range.clone()), options)
                        .await
                        .expect("Failed reading content back from store");

                assert_eq!(read_buffer.len(), range.len());
                assert_eq!(read_buffer.as_ref(), &payload.as_ref()[range]);

                let range = 1000..30000;
                let read_buffer =
                    immutable::read(repository.clone(), address, Some(range.clone()), options)
                        .await
                        .expect("Failed reading content back from store");

                assert_eq!(read_buffer.len(), range.len());
                assert_eq!(read_buffer.as_ref(), &payload.as_ref()[range]);

                let range = 1000..350000;
                let read_buffer =
                    immutable::read(repository.clone(), address, Some(range.clone()), options)
                        .await
                        .expect("Failed reading content back from store");

                assert_eq!(read_buffer.len(), range.len());
                assert_eq!(read_buffer.as_ref(), &payload.as_ref()[range]);

                let range = 500000..510000;
                let read_buffer =
                    immutable::read(repository.clone(), address, Some(range.clone()), options)
                        .await
                        .expect("Failed reading content back from store");

                assert_eq!(read_buffer.len(), range.len());
                assert_eq!(read_buffer.as_ref(), &payload.as_ref()[range]);
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Create an immutable store with a path so index files are written
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository: RepositoryId = rand::random();
                let context = rand::random();

                // Write a fragment to the store
                let payload: Vec<u8> = (0..1024).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());

                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload_bytes.len() as u32,
                    size_content: payload_bytes.len() as u64,
                };

                let hash = lore_storage::hash::hash_slice(&payload_bytes);
                let address = Address { hash, context };

                // Store the fragment
                store
                    .clone()
                    .put(
                        repository,
                        address,
                        fragment,
                        Some(payload_bytes.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to store fragment");

                // Flush to ensure index files are written
                store
                    .clone()
                    .flush(true)
                    .await
                    .expect("Failed to flush store");

                // Now verify the fragment using verify_fragment
                let result = store
                    .clone()
                    .verify_fragment(
                        address,
                        repository,
                        StoreMatch::MatchHash,
                        false, /* heal */
                    )
                    .await
                    .expect("verify_fragment failed");

                assert_eq!(result.group_index, hash.data()[0] as usize);
                // bucket_index depends on the group's current bucket_count; at the client-default level 1 every entry lands in bucket 0.
                assert_eq!(
                    result.bucket_index,
                    lore_storage::local::fan_out::bucket_index_for(
                        &hash,
                        store.group[result.group_index]
                            .bucket_count
                            .load(std::sync::atomic::Ordering::Relaxed)
                    )
                );

                assert_eq!(result.entry_count, 1);

                assert_eq!(result.matches.len(), 1);

                let first_match = &result.matches[0];
                assert_eq!(first_match.address, address);
                assert_eq!(first_match.partition, repository);
                assert_eq!(first_match.data.size_payload, payload.len() as u32);

                // No corruption, so no heal should occur
                assert!(!result.healed);
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_multiple_contexts() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository: RepositoryId = rand::random();

                // Create the same payload
                let payload: Vec<u8> = (0..1024).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);

                // Store the same content with 3 different contexts
                let contexts: Vec<Context> = (0..3).map(|_| rand::random()).collect();

                for context in &contexts {
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload_bytes.len() as u32,
                        size_content: payload_bytes.len() as u64,
                    };

                    let address = Address {
                        hash,
                        context: *context,
                    };

                    store
                        .clone()
                        .put(
                            repository,
                            address,
                            fragment,
                            Some(payload_bytes.clone()),
                            false,
                        )
                        .await
                        .expect("Failed to store fragment");
                }

                store
                    .clone()
                    .flush(true)
                    .await
                    .expect("Failed to flush store");

                // Verify we can find all 3 contexts with the same hash
                let address = Address {
                    hash,
                    context: contexts[0],
                };

                let result = store
                    .clone()
                    .verify_fragment(
                        address,
                        repository,
                        StoreMatch::MatchHash,
                        false, /* heal */
                    )
                    .await
                    .expect("verify_fragment failed");

                assert_eq!(result.matches.len(), 3);

                // Verify all contexts are present in matches
                let found_contexts: std::collections::HashSet<_> =
                    result.matches.iter().map(|m| m.address.context).collect();

                for context in &contexts {
                    assert!(found_contexts.contains(context));
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_not_found() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository: RepositoryId = rand::random();
                let context = rand::random();

                // First, write a fragment to ensure the index infrastructure exists
                let payload: Vec<u8> = (0..512).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());

                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload_bytes.len() as u32,
                    size_content: payload_bytes.len() as u64,
                };

                let hash = lore_storage::hash::hash_slice(&payload_bytes);
                let address = Address { hash, context };

                store
                    .clone()
                    .put(
                        repository,
                        address,
                        fragment,
                        Some(payload_bytes.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to store fragment");

                store
                    .clone()
                    .flush(true)
                    .await
                    .expect("Failed to flush store");

                // Now try to verify a different, non-existent fragment
                // Use the same group/bucket (first two bytes) to ensure the index file exists,
                // but make the rest of the hash different
                let mut nonexistent_hash_data = *hash.data();
                nonexistent_hash_data[2] = nonexistent_hash_data[2].wrapping_add(1); // Change 3rd byte
                let nonexistent_hash = Hash::from(nonexistent_hash_data);
                let nonexistent_address = Address {
                    hash: nonexistent_hash,
                    context: rand::random(),
                };

                let result = store
                    .clone()
                    .verify_fragment(
                        nonexistent_address,
                        repository,
                        StoreMatch::MatchNone,
                        false, /* heal */
                    )
                    .await;

                assert!(result.is_err());
                assert!(result.unwrap_err().is_address_not_found());
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_not_first_in_index() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository: RepositoryId = rand::random();

                // Create 5 fragments with the first two bytes the same, but different remaining
                // bytes to ensure they land in the same group/bucket but are sorted differently.
                let group_byte = 0x0b;
                let bucket_byte = 0x4d;

                let mut fragments = Vec::new();
                for i in 0..5u8 {
                    let mut hash_data = [0u8; 32];
                    hash_data[0] = group_byte;
                    hash_data[1] = bucket_byte;
                    // Make different third bytes to create different hashes
                    hash_data[2] = i * 50; // 0, 50, 100, 150, 200
                    // Fill rest with random data based on index for uniqueness
                    for (j, byte) in hash_data.iter_mut().enumerate().skip(3) {
                        *byte = (i.wrapping_mul(j as u8)).wrapping_add(rng.random_range(0..=10));
                    }

                    let hash = Hash::from(hash_data);
                    let context = rand::random();
                    let address = Address { hash, context };

                    // Create unique payload for each fragment
                    let payload: Vec<u8> = (0..512).map(|_| rng.random_range(0..=255)).collect();
                    let payload_bytes = Bytes::copy_from_slice(payload.as_slice());

                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload_bytes.len() as u32,
                        size_content: payload_bytes.len() as u64,
                    };

                    store
                        .clone()
                        .put(
                            repository,
                            address,
                            fragment,
                            Some(payload_bytes.clone()),
                            false,
                        )
                        .await
                        .expect("Failed to store fragment");

                    fragments.push((address, hash_data[2]));
                }

                store
                    .clone()
                    .flush(true)
                    .await
                    .expect("Failed to flush store");

                // Sort fragments by their hash to determine expected order
                let mut sorted_fragments = fragments.clone();
                sorted_fragments.sort_by(|a, b| a.0.hash.data().cmp(b.0.hash.data()));

                // Look up the fragment that should be in the middle (index 2)
                let middle_fragment = sorted_fragments[2];

                let result = store
                    .clone()
                    .verify_fragment(
                        middle_fragment.0,
                        repository,
                        StoreMatch::MatchHash,
                        false, /* heal */
                    )
                    .await
                    .expect("verify_fragment failed for middle fragment");

                assert!(!result.matches.is_empty());

                // Verify we found the right fragment
                let found_match = result
                    .matches
                    .iter()
                    .find(|m| m.address.hash == middle_fragment.0.hash);
                assert!(found_match.is_some());

                let found = found_match.unwrap();
                assert_eq!(found.address, middle_fragment.0);

                assert!(found.slot > 0);
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_match_repository() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository: RepositoryId = rand::random();
                let other_repository: RepositoryId = rand::random();

                let payload: Vec<u8> = (0..1024).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);

                let contexts: Vec<Context> = (0..3).map(|_| rand::random()).collect();

                for context in &contexts {
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload_bytes.len() as u32,
                        size_content: payload_bytes.len() as u64,
                    };

                    store
                        .clone()
                        .put(
                            repository,
                            Address {
                                hash,
                                context: *context,
                            },
                            fragment,
                            Some(payload_bytes.clone()),
                            false,
                        )
                        .await
                        .expect("Failed to store fragment");
                }

                let other_context: Context = rand::random();
                store
                    .clone()
                    .put(
                        other_repository,
                        Address {
                            hash,
                            context: other_context,
                        },
                        Fragment {
                            flags: 0,
                            size_payload: payload_bytes.len() as u32,
                            size_content: payload_bytes.len() as u64,
                        },
                        Some(payload_bytes.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to store fragment");

                store
                    .clone()
                    .flush(true)
                    .await
                    .expect("Failed to flush store");

                let result = store
                    .clone()
                    .verify_fragment(
                        Address {
                            hash,
                            context: contexts[0],
                        },
                        repository,
                        StoreMatch::MatchPartition,
                        false, /* heal */
                    )
                    .await
                    .expect("verify_fragment failed");

                assert_eq!(result.matches.len(), 3);

                for m in &result.matches {
                    assert_eq!(m.partition, repository);
                    assert_eq!(m.address.hash, hash);
                }

                let found_contexts: std::collections::HashSet<_> =
                    result.matches.iter().map(|m| m.address.context).collect();

                for context in &contexts {
                    assert!(found_contexts.contains(context));
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_match_full() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository: RepositoryId = rand::random();
                let context: Context = rand::random();

                let payload: Vec<u8> = (0..1024).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);

                let address = Address { hash, context };

                store
                    .clone()
                    .put(
                        repository,
                        address,
                        Fragment {
                            flags: 0,
                            size_payload: payload_bytes.len() as u32,
                            size_content: payload_bytes.len() as u64,
                        },
                        Some(payload_bytes.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to store fragment");

                let other_context: Context = rand::random();
                store
                    .clone()
                    .put(
                        repository,
                        Address {
                            hash,
                            context: other_context,
                        },
                        Fragment {
                            flags: 0,
                            size_payload: payload_bytes.len() as u32,
                            size_content: payload_bytes.len() as u64,
                        },
                        Some(payload_bytes.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to store fragment");

                store
                    .clone()
                    .flush(true)
                    .await
                    .expect("Failed to flush store");

                let result = store
                    .clone()
                    .verify_fragment(
                        address,
                        repository,
                        StoreMatch::MatchFull,
                        false, /* heal */
                    )
                    .await
                    .expect("verify_fragment failed");

                assert_eq!(result.matches.len(), 1);

                let found = &result.matches[0];
                assert_eq!(found.address, address);
                assert_eq!(found.partition, repository);
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_corrupted_no_heal() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository: RepositoryId = rand::random();
                let context: Context = rand::random();

                let payload: Vec<u8> = (0..1024).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);

                let address = Address { hash, context };

                // Windows complains about the packfile still being locked when we go to corrupt it
                // below. In order to ensure the lock is released, we perform a scoped write and
                // verify against the store, then let it drop to ensure the locks are released
                // before we corrupt and subsequently re-verify.
                let (pack_file, pack_offset) = {
                    let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                        Some(dir.clone()),
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("Failed to create store");

                    store
                        .clone()
                        .put(
                            repository,
                            address,
                            Fragment {
                                flags: 0,
                                size_payload: payload_bytes.len() as u32,
                                size_content: payload_bytes.len() as u64,
                            },
                            Some(payload_bytes.clone()),
                            false,
                        )
                        .await
                        .expect("Failed to store fragment");

                    store
                        .clone()
                        .flush(true)
                        .await
                        .expect("Failed to flush store");

                    let result = store
                        .clone()
                        .verify_fragment(address, repository, StoreMatch::MatchFull, false)
                        .await
                        .expect("verify_fragment failed");

                    assert_eq!(result.matches.len(), 1);
                    (
                        result.matches[0].data.pack_file,
                        result.matches[0].data.pack_offset,
                    )
                };

                corrupt_packfile(&dir, hash.data()[0], pack_file, pack_offset);

                // As described above, we create a new store instance rather than reusing the
                // original to avoid holding FS locks on the packfiles.
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                // Verify with heal=false - should detect corruption but NOT evict
                let result = store
                    .clone()
                    .verify_fragment(address, repository, StoreMatch::MatchFull, false)
                    .await
                    .expect("verify_fragment failed");

                assert!(result.verification_result.is_err());

                // heal=false means no healing even with corruption
                assert!(!result.healed);

                // Verify again, since we didn't request a heal, the fragment should still be
                // present (i.e. pack_file should be non-zero)
                let result = store
                    .clone()
                    .verify_fragment(address, repository, StoreMatch::MatchFull, false)
                    .await
                    .expect("verify_fragment failed");

                assert_eq!(result.matches.len(), 1);
                assert_ne!(result.matches[0].data.pack_file, 0);
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_corrupted_heal() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository: RepositoryId = rand::random();
                let context: Context = rand::random();

                let payload: Vec<u8> = (0..1024).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);

                let address = Address { hash, context };

                // Windows complains about the packfile still being locked when we go to corrupt it
                // below. In order to ensure the lock is released, we perform a scoped write and
                // verify against the store, then let it drop to ensure the locks are released
                // before we corrupt and subsequently re-verify.
                let (pack_file, pack_offset) = {
                    let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                        Some(dir.clone()),
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("Failed to create store");

                    store
                        .clone()
                        .put(
                            repository,
                            address,
                            Fragment {
                                flags: 0,
                                size_payload: payload_bytes.len() as u32,
                                size_content: payload_bytes.len() as u64,
                            },
                            Some(payload_bytes.clone()),
                            false,
                        )
                        .await
                        .expect("Failed to store fragment");

                    store
                        .clone()
                        .flush(true)
                        .await
                        .expect("Failed to flush store");

                    let result = store
                        .clone()
                        .verify_fragment(address, repository, StoreMatch::MatchFull, false)
                        .await
                        .expect("verify_fragment failed");

                    assert_eq!(result.matches.len(), 1);
                    (
                        result.matches[0].data.pack_file,
                        result.matches[0].data.pack_offset,
                    )
                };

                corrupt_packfile(&dir, hash.data()[0], pack_file, pack_offset);

                // As described above, we create a new store instance rather than reusing the
                // original to avoid holding FS locks on the packfiles.
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                // Verify again, since we requested a heal, this should indicate a hash mismatch
                let result = store
                    .clone()
                    .verify_fragment(address, repository, StoreMatch::MatchFull, true)
                    .await
                    .expect("verify_fragment failed");

                assert!(result.verification_result.is_err());

                // heal=true with corruption should heal
                assert!(result.healed);

                // Verify once more, now the verification should succeed, but the packfile should be
                // set to 0
                let result = store
                    .clone()
                    .verify_fragment(address, repository, StoreMatch::MatchFull, false)
                    .await
                    .expect("verify_fragment failed");

                assert_eq!(result.matches.len(), 1);
                assert_eq!(result.matches[0].data.pack_file, 0);
            })
            .await;
    }

    #[tokio::test]
    async fn test_verify_fragment_match_full_wrong_context() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let mut rng = rand::rng();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
                    Some(dir.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository: RepositoryId = rand::random();
                let stored_context: Context = rand::random();

                let payload: Vec<u8> = (0..1024).map(|_| rng.random_range(0..=255)).collect();
                let payload_bytes = Bytes::copy_from_slice(payload.as_slice());
                let hash = lore_storage::hash::hash_slice(&payload_bytes);

                let stored_address = Address {
                    hash,
                    context: stored_context,
                };

                store
                    .clone()
                    .put(
                        repository,
                        stored_address,
                        Fragment {
                            flags: 0,
                            size_payload: payload_bytes.len() as u32,
                            size_content: payload_bytes.len() as u64,
                        },
                        Some(payload_bytes.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to store fragment");

                store
                    .clone()
                    .flush(true)
                    .await
                    .expect("Failed to flush store");

                // Try to verify with a different context - should fail with NotFound
                let wrong_context: Context = rand::random();
                let wrong_address = Address {
                    hash,
                    context: wrong_context,
                };

                let result = store
                    .clone()
                    .verify_fragment(
                        wrong_address,
                        repository,
                        StoreMatch::MatchFull,
                        false, /* heal */
                    )
                    .await;

                assert!(result.is_err());
                assert!(result.unwrap_err().is_address_not_found());
            })
            .await;
    }

    #[tokio::test]
    async fn test_read_zero_hash_returns_zero_initialized() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository = random::<RepositoryId>();

                let repository = Arc::new(RepositoryContext::new(
                    Some(dir.as_path().to_path_buf()),
                    immutable_store,
                    mutable_store,
                    repository,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                let zero_address = Address::zero_context_hash(Hash::default());
                let options = read_options_from_repository(&repository);

                let result = BranchLatestHistory::read_from_immutable(
                    repository.clone(),
                    zero_address,
                    options,
                )
                .await
                .expect("read_from_immutable with zero hash should succeed");

                assert!(
                    result.revision.is_zero(),
                    "revision should be zero-initialized, got {}",
                    result.revision
                );
                assert!(
                    result.previous.is_zero(),
                    "previous should be zero-initialized, got {}",
                    result.previous
                );
            })
            .await;
    }

    /// Writes a payload using small fixed-size chunks to force a 2-level
    /// fragment tree, then reads it back via `read_into_file` (which uses the
    /// streaming defragment pipeline) and verifies the content matches.
    #[tokio::test]
    async fn read_into_file_multilevel_fragmented() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();

        let mut rng = rand::rng();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository_id = random::<RepositoryId>();
                let context = random::<Context>();

                let repository = Arc::new(RepositoryContext::new(
                    Some(dir.as_path().to_path_buf()),
                    immutable_store,
                    mutable_store,
                    repository_id,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                // 2 MiB of random data with 256-byte fixed chunks creates ~8192
                // fragments. Fragment list = 8192 * 40 = 327,680 bytes > 256 KiB
                // threshold, forcing a 2-level fragment tree.
                let payload_size = 2 * 1024 * 1024;
                let payload: Vec<u8> = (0..payload_size)
                    .map(|_| rng.random_range(0..=255))
                    .collect();
                let payload = Bytes::copy_from_slice(payload.as_slice());

                let flags = WriteOptions::default().with_fixed_size_chunk(256);
                let (address, fragment) =
                    immutable::write(repository.clone(), context, payload.clone(), flags)
                        .await
                        .expect("Failed writing to store");

                // Verify the data is fragmented
                assert!(
                    fragment.flags & lore_storage::FragmentFlags::PayloadFragmented
                        == lore_storage::FragmentFlags::PayloadFragmented,
                    "Expected PayloadFragmented flag"
                );

                // Read into file using the pipeline
                let output_path = dir.join("output_file");
                let options = immutable::read_options_from_repository(&repository);
                let (read_fragment, _) = immutable::read_into_file(
                    repository.clone(),
                    address,
                    output_path.as_path(),
                    options,
                )
                .await
                .expect("read_into_file failed");

                assert_eq!(read_fragment.size_content, payload_size as u64);

                // Verify file content matches original payload
                let file_content = std::fs::read(&output_path).expect("Failed reading output file");
                assert_eq!(file_content.len(), payload.len());
                assert_eq!(file_content.as_slice(), payload.as_ref());
            })
            .await;
    }

    /// Writes a payload using small fixed-size chunks to force a 2-level
    /// fragment tree, then reads it back via `read_stream` (which uses the
    /// streaming defragment pipeline in ordered mode) and verifies the
    /// reassembled content matches.
    #[tokio::test]
    async fn read_stream_multilevel_fragmented() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();

        let mut rng = rand::rng();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository_id = random::<RepositoryId>();
                let context = random::<Context>();

                let repository = Arc::new(RepositoryContext::new(
                    Some(dir.as_path().to_path_buf()),
                    immutable_store,
                    mutable_store,
                    repository_id,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                let payload_size = 2 * 1024 * 1024;
                let payload: Vec<u8> = (0..payload_size)
                    .map(|_| rng.random_range(0..=255))
                    .collect();
                let payload = Bytes::copy_from_slice(payload.as_slice());

                let flags = WriteOptions::default().with_fixed_size_chunk(256);
                let (address, _fragment) =
                    immutable::write(repository.clone(), context, payload.clone(), flags)
                        .await
                        .expect("Failed writing to store");

                // Read via stream and collect all buffers
                let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                let options = immutable::read_options_from_repository(&repository);
                let content_length =
                    immutable::read_stream(repository.clone(), address, options, tx)
                        .await
                        .expect("read_stream failed");

                assert_eq!(content_length, payload_size as u64);

                let mut reassembled = Vec::with_capacity(payload_size);
                while let Some(chunk) = rx.recv().await {
                    let chunk = chunk.expect("stream item failed");
                    reassembled.extend_from_slice(chunk.as_ref());
                }

                assert_eq!(reassembled.len(), payload.len());
                assert_eq!(reassembled.as_slice(), payload.as_ref());

                // A cross-fragment range streams only the selected logical
                // bytes, including partial first/last leaves.
                let requested = 12_345u64..1_876_543u64;
                let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                let options = immutable::read_options_from_repository(&repository);
                let (full_length, normalized) = immutable::read_stream_range(
                    repository.clone(),
                    address,
                    Some(requested.clone()),
                    options,
                    tx,
                )
                .await
                .expect("read_stream_range failed");
                assert_eq!(full_length, payload_size as u64);
                assert_eq!(normalized, requested);

                let mut ranged = Vec::new();
                while let Some(chunk) = rx.recv().await {
                    ranged.extend_from_slice(chunk.expect("range stream item failed").as_ref());
                }
                assert_eq!(
                    ranged.as_slice(),
                    &payload[requested.start as usize..requested.end as usize]
                );
            })
            .await;
    }

    /// Writes a payload with normal chunking (1-level fragment tree) and verifies
    /// `read_into_file` works correctly with the pipeline for the common case.
    #[tokio::test]
    async fn read_into_file_single_level() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();

        let mut rng = rand::rng();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository_id = random::<RepositoryId>();
                let context = random::<Context>();

                let repository = Arc::new(RepositoryContext::new(
                    Some(dir.as_path().to_path_buf()),
                    immutable_store,
                    mutable_store,
                    repository_id,
                    lore_revision::instance::InstanceId::default(),
                    Err(ProtocolError::from(NoRemote)),
                    Arc::default(),
                    RepositoryFormat::Lore,
                ));

                // 1 MiB of random data — standard chunking produces a 1-level tree
                let payload_size = 1024 * 1024;
                let payload: Vec<u8> = (0..payload_size)
                    .map(|_| rng.random_range(0..=255))
                    .collect();
                let payload = Bytes::copy_from_slice(payload.as_slice());

                let (address, _fragment) = immutable::write(
                    repository.clone(),
                    context,
                    payload.clone(),
                    WriteOptions::default(),
                )
                .await
                .expect("Failed writing to store");

                let output_path = dir.join("output_single_level");
                let options = immutable::read_options_from_repository(&repository);
                let (read_fragment, _) = immutable::read_into_file(
                    repository.clone(),
                    address,
                    output_path.as_path(),
                    options,
                )
                .await
                .expect("read_into_file failed");

                assert_eq!(read_fragment.size_content, payload_size as u64);

                let file_content = std::fs::read(&output_path).expect("Failed reading output file");
                assert_eq!(file_content.len(), payload.len());
                assert_eq!(file_content.as_slice(), payload.as_ref());
            })
            .await;
    }
}
