// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::lore::*;
use lore_storage::*;

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use bytes::Bytes;
    use bytes::BytesMut;
    use lore_storage::ImmutableStore as ImmutableStoreTrait;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use rand::Rng;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;

    include!("helper.rs");

    fn hash_set_byte(hash: &mut Hash, index: usize, value: u8) {
        hash.data_mut()[index] = value;
    }

    #[tokio::test]
    async fn store_zero_size() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let address = Address {
                    context: repository.into(),
                    hash: Hash::default(),
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: 0,
                    size_content: 0,
                };
                let entry = store
                    .store(repository, address, fragment, None, false)
                    .await;
                assert!(entry.is_err(), "Zero sized data not rejected as expected");
            })
            .await;
    }

    #[tokio::test]
    async fn store_single_item() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(None, lore_storage::local::immutable_store::ImmutableStoreSettings::default())
                    .await
                    .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let address = Address {
                    context: repository.into(),
                    hash: Hash::default(),
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: 100,
                    size_content: 1000,
                };
                let payload = [0u8; 1000];
                let entry = store.clone()
                    .store(
                        repository,
                        address,
                        fragment,
                        Some(Bytes::copy_from_slice(&payload[..90])),
                        false,
                    )
                    .await;
                assert!(entry.is_err());
                assert!(
                    entry.is_err(),
                    "Store in empty store with mismatched payload size did not fail with expected error"
                );
                store.clone()
                    .store(
                        repository,
                        address,
                        fragment,
                        Some(Bytes::copy_from_slice(&payload[..100])),
                        false,
                    )
                    .await
                    .expect("Failed to store entry");
                let entry = store
                    .find(repository, address)
                    .await
                    .expect("Failed query after store");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchFull,
                    "Store in empty store did not match fully as expected"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn store_multiple_items() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let mut rng = rand::rng();
                let store = LocalImmutableStore::new(None, lore_storage::local::immutable_store::ImmutableStoreSettings{
                        protect_local_fragment: false,
                        ..Default::default()
                    })
                    .await
                    .expect("Failed to create store");

                let repository = random::<RepositoryId>();

                // Stress test, put 10000 items in the same bucket
                for i in 0..10000 {
                    let payload: Vec<u8> = (0..64).map(|_| rng.random_range(0..=255)).collect();
                    let other_payload: Vec<u8> = {
                        let mut cloned = payload.clone();
                        cloned.push(rand::random());
                        cloned
                    };

                    let mut hash = Hash::hash_buffer(payload.as_slice());
                    // Stress test, put everything in the same bucket
                    hash_set_byte(&mut hash, 0, 0);
                    hash_set_byte(&mut hash, 1, 0);

                    // Store a unique repo/context/hash triplet
                    let address = Address {
                        context: random::<Context>(),
                        hash,
                    };
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: (1000 + i) as u64,
                    };
                    store.clone()
                        .store(
                            repository,
                            address,
                            fragment,
                            Some(Bytes::copy_from_slice(payload.as_slice())),
                            false,
                        )
                        .await
                        .expect("Failed to store entry");

                    let entry = store
                        .find(repository, address)
                        .await
                        .expect("Failed query after store");
                    assert_eq!(
                        entry.matching,
                        StoreMatch::MatchFull,
                        "Store unique matched previous entry unexpectedly"
                    );
                    assert_eq!(
                        entry.data.size_payload, fragment.size_payload,
                        "Store unique did not return same fragment details"
                    );
                    assert_eq!(
                        entry.data.size_content, fragment.size_content,
                        "Store unique did not return same fragment details"
                    );
                    assert_eq!(
                        entry.data.flags, fragment.flags,
                        "Store unique did not return same fragment details"
                    );

                    // Store the exact same repo/context/hash triplet again
                    store.clone()
                        .store(
                            repository,
                            address,
                            fragment,
                            Some(Bytes::copy_from_slice(payload.as_slice())),
                            false,
                        )
                        .await
                        .expect("Failed to store entry");
                    let entry = store
                        .find(repository, address)
                        .await
                        .expect("Failed query after second store");
                    assert_eq!(
                        entry.matching,
                        StoreMatch::MatchFull,
                        "Store repeated did not match previous entry fully as expected"
                    );
                    assert_eq!(
                        entry.data.size_payload, fragment.size_payload,
                        "Store repeated did not return same fragment details"
                    );
                    assert_eq!(
                        entry.data.size_content, fragment.size_content,
                        "Store repeated did not return same fragment details"
                    );
                    assert_eq!(
                        entry.data.flags, fragment.flags,
                        "Store repeated did not return same fragment details"
                    );

                    // Modify the content and use same repo/context/hash triplet to generate a conflict
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: (1000 + i + 1) as u64,
                    };
                    let collision_entry = store.clone()
                        .store(
                            repository,
                            address,
                            fragment,
                            Some(Bytes::copy_from_slice(payload.as_slice())),
                            false,
                        )
                        .await;
                    assert!(collision_entry.is_err());
                    assert!(
                        collision_entry.is_err(),
                        "Store with same hash and different content size did not result in hash collision as expected"
                    );

                    // Partial deduplication test, use the same repo and hash but different context
                    let address = Address {
                        context: random::<Context>(),
                        hash,
                    };
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: (1000 + i) as u64,
                    };
                    let entry = store
                        .find(repository, address)
                        .await
                        .expect("Failed query after store");
                    assert_eq!(
                        entry.matching,
                        StoreMatch::MatchPartition,
                        "Repeated store with different content did not match hash and repository as expected"
                    );
                    assert_eq!(
                        entry.data.size_payload, fragment.size_payload,
                        "Partial deduplicate did not return identical fragment details as expected"
                    );
                    assert_eq!(
                        entry.data.size_content, fragment.size_content,
                        "Partial deduplicate did not return identical fragment details as expected"
                    );
                    assert_eq!(
                        entry.data.flags, fragment.flags,
                        "Partial deduplicate did not return identical fragment details as expected"
                    );

                    store.clone()
                        .store(
                            repository,
                            address,
                            fragment,
                            Some(Bytes::copy_from_slice(payload.as_slice())),
                            false,
                        )
                        .await
                        .expect("Failed to store deduplicated entry");

                    let other_repository = RepositoryId::from([1; 16]);
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: (1000 + i) as u64,
                    };
                    let result = store.clone()
                        .store(other_repository, address, fragment, None, false)
                        .await;
                    assert!(
                    result.is_ok(),
                    "Repeated store with different repository and no payload should succeed");

                    let entry = store
                        .find(other_repository, address)
                        .await
                        .expect("Failed query after store");
                    assert_eq!(
                    entry.matching,
                    StoreMatch::MatchFull,
                    "Repeated store with different repository and payload did not MatchFull as expected");
                    assert_eq!(
                        entry.data.size_payload, fragment.size_payload,
                        "Repository deduplication did not return identical fragment details as expected"
                    );
                    assert_eq!(
                        entry.data.size_content, fragment.size_content,
                        "Repository deduplication did not return identical fragment details as expected"
                    );
                    assert_eq!(
                        entry.data.flags, fragment.flags,
                        "Repository deduplication did not return identical fragment details as expected"
                    );

                    store.clone()
                        .store(
                            other_repository,
                            address,
                            fragment,
                            Some(Bytes::copy_from_slice(payload.as_slice())),
                            false,
                        )
                        .await
                        .expect("Failed to store entry");

                    // Generate a different payload representation with same content (size)
                    let other_repository = RepositoryId::from([2; 16]);
                    let other_fragment = Fragment {
                        flags: 0,
                        size_payload: other_payload.len() as u32,
                        size_content: (1000 + i) as u64,
                    };

                    let entry = store
                        .find(other_repository, address)
                        .await
                        .expect("Failed query after store");
                    assert_eq!(
                    entry.matching,
                    StoreMatch::MatchHash,
                    "Repeated store with different repository and different payload did not match hash only as expected");
                    assert_eq!(
                        entry.data.size_payload, fragment.size_payload,
                        "Repository deduplication with new fragment representation did not return previous fragment details as expected"
                    );
                    assert_eq!(
                        entry.data.size_content, fragment.size_content,
                        "Repository deduplication with new fragment representation did not return previous fragment details as expected"
                    );
                    assert_eq!(
                        entry.data.flags, fragment.flags,
                        "Repository deduplication with new fragment representation did not return previous fragment details as expected"
                    );

                    store.clone()
                        .store(
                            other_repository,
                            address,
                            other_fragment,
                            Some(Bytes::copy_from_slice(other_payload.as_slice())),
                            false,
                        )
                        .await
                        .expect("Failed to store entry");
                }
            })
            .await;
    }

    #[tokio::test]
    async fn store_query() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let mut rng = rand::rng();
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let payload: Vec<u8> = (0..64).map(|_| rng.random_range(0..=255)).collect();
                let mut base_hash = Hash::hash_buffer(payload.as_slice());

                // Put everything in the same bucket
                hash_set_byte(&mut base_hash, 0, 0);
                hash_set_byte(&mut base_hash, 1, 0);

                let mut hash: [Hash; 9] = [base_hash; 9];

                let repository = random::<RepositoryId>();
                let repository_other = random::<RepositoryId>();

                let mut context: [Context; 9] = [Context::from([0; 16]); 9];

                for i in 0..9 {
                    hash_set_byte(&mut hash[i], 31, i as u8);

                    let mut ctx_data: [u8; 16] = rand::random();
                    ctx_data[15] = i as u8;
                    context[i] = Context::from(ctx_data);

                    // Store a unique repo/context/hash triplet
                    let address = Address {
                        context: context[i],
                        hash: hash[i],
                    };
                    let fragment = if i == 8 {
                        // For the sake of testing query behavior on obliterated fragments
                        Fragment {
                            flags: FragmentFlags::PayloadObliterated.bits(),
                            // Not strictly valid from a data consistency perspective, but the query
                            // check only cares about the flags, so this allows us to actually store
                            // the fragment.
                            size_payload: payload.len() as u32,
                            size_content: (1000 + i) as u64,
                        }
                    } else {
                        Fragment {
                            flags: 0,
                            size_payload: payload.len() as u32,
                            size_content: (1000 + i) as u64,
                        }
                    };
                    store
                        .clone()
                        .store(
                            if i < 4 { repository } else { repository_other },
                            address,
                            fragment,
                            // Note: we store an actual payload for all fragments, including the one
                            //  with metadata indicating it's been obliterated. This is not actually
                            //  valid, but is sufficient for the purposes of this test.
                            Some(Bytes::copy_from_slice(payload.as_slice())),
                            false,
                        )
                        .await
                        .expect("Failed to store entry");
                }

                // The level is the one found, so what varies is which partition and context the
                // address is asked about rather than which level is requested.
                let address = Address {
                    context: context[0],
                    hash: rand::random::<Hash>(),
                };
                let entry = store
                    .find(repository, address)
                    .await
                    .expect("Failed to query store entry");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchNone,
                    "Query did not match expected hash"
                );

                let address = Address {
                    context: context[8],
                    hash: hash[8],
                };
                let entry = store
                    .find(repository, address)
                    .await
                    .expect("Failed to query store entry");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchHash,
                    "Query did not match expected hash"
                );
                assert_eq!(
                    entry.data.flags & FragmentFlags::PayloadObliterated.bits(),
                    FragmentFlags::PayloadObliterated.bits(),
                    "Query did not return expected flags"
                );

                let address = Address {
                    context: context[0],
                    hash: hash[0],
                };
                let entry = store
                    .find(repository, address)
                    .await
                    .expect("Failed to query store entry");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchFull,
                    "the storing partition and context is a full match"
                );

                let address = Address {
                    context: context[1],
                    hash: hash[1],
                };
                let entry = store
                    .find(repository, address)
                    .await
                    .expect("Failed to query store entry");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchFull,
                    "the storing partition and context is a full match"
                );

                let address = Address {
                    context: context[7],
                    hash: hash[7],
                };
                let entry = store
                    .find(repository_other, address)
                    .await
                    .expect("Failed to query store entry");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchFull,
                    "Query did not match expected hash"
                );

                let address = Address {
                    context: context[0],
                    hash: hash[1],
                };
                let entry = store
                    .find(repository, address)
                    .await
                    .expect("Failed to query store entry");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchPartition,
                    "a sibling context in the storing partition is a partition match"
                );

                let entry = store
                    .find(repository_other, address)
                    .await
                    .expect("Failed to query store entry");
                assert_eq!(
                    entry.matching,
                    StoreMatch::MatchHash,
                    "the same hash from another partition is a hash match"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn store_load() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let mut rng = rand::rng();
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let payload: [Vec<u8>; 8] =
                    std::array::from_fn(|_| (0..64).map(|_| rng.random_range(0..=255)).collect());
                let mut hash: [Hash; 8] =
                    std::array::from_fn(|i| Hash::hash_buffer(payload[i].as_slice()));

                // Put everything in the same bucket
                for item in hash.iter_mut() {
                    hash_set_byte(item, 0, 0);
                    hash_set_byte(item, 1, 0);
                }

                let repository = random::<RepositoryId>();

                let mut context: [Context; 8] = [Context::from([0; 16]); 8];

                for i in 0..8 {
                    context[i] = random::<Context>();

                    // Store a unique repo/context/hash triplet
                    let address = Address {
                        context: context[i],
                        hash: hash[i],
                    };
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload[i].len() as u32,
                        size_content: (1000 + i) as u64,
                    };
                    store
                        .clone()
                        .store(
                            repository,
                            address,
                            fragment,
                            Some(Bytes::copy_from_slice(payload[i].as_slice())),
                            false,
                        )
                        .await
                        .expect("Failed to store entry");
                }

                // Try to load back the data
                for i in 0..8 {
                    let address = Address {
                        context: context[i],
                        hash: hash[i],
                    };
                    let entry = store
                        .find(repository, address)
                        .await
                        .expect("Failed to query store entry");

                    let read_buffer =
                        LocalImmutableStore::load(store.packstore(entry.group), entry.data)
                            .await
                            .expect("Failed to load store entry");
                    assert_eq!(
                        read_buffer.len(),
                        payload[i].len(),
                        "Store load did not read expected size"
                    );
                    assert_eq!(
                        payload[i],
                        read_buffer.to_vec(),
                        "Loaded data not identical to stored data"
                    );

                    // Try query with partial match
                    let address = Address {
                        context: random::<Context>(),
                        hash: hash[i],
                    };
                    let repository = random::<RepositoryId>();
                    let entry = store
                        .find(repository, address)
                        .await
                        .expect("Failed to query store entry");

                    let read_buffer =
                        LocalImmutableStore::load(store.packstore(entry.group), entry.data)
                            .await
                            .expect("Failed to load store entry");
                    assert_eq!(
                        read_buffer.len(),
                        payload[i].len(),
                        "Store load did not read expected size"
                    );
                    assert_eq!(
                        payload[i],
                        read_buffer.to_vec(),
                        "Loaded data not identical to stored data"
                    );

                    // Should fail, repository don't match
                    let entry = store
                        .find(repository, address)
                        .await
                        .expect("Failed to query store entry");
                    assert_eq!(
                        entry.matching,
                        StoreMatch::MatchHash,
                        "Query stored entry with hash and repository did not match hash as expected"
                    );
                }
            })
            .await;
    }

    #[tokio::test]
    async fn store_serialize_deserialize() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let mut rng = rand::rng();
                let store = LocalImmutableStore::new(
                    Some(dir.clone()),
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let mut payload: Vec<Vec<u8>> = vec![];
                payload.resize_with(1024, || {
                    (0..64).map(|_| rng.random_range(0..=255)).collect()
                });

                for _iteration in 0..10 {
                    let mut hash: Vec<Hash> = vec![];
                    let mut i = 0;
                    hash.resize_with(payload.len(), || {
                        payload[i][0] = rand::random();
                        payload[i][1] = rand::random();
                        payload[i][2] = rand::random();
                        payload[i][3] = rand::random();
                        i += 1;
                        Hash::hash_buffer(payload[i - 1].as_slice())
                    });

                    // Put a couple of items in the same bucket
                    for i in 0..(hash.len() / 2) {
                        hash_set_byte(&mut hash[i], 0, 0);
                        hash_set_byte(&mut hash[i], 1, 0);
                    }

                    let repository = random::<RepositoryId>();

                    let mut context: Vec<Context> = vec![];
                    context.resize_with(payload.len(), random::<Context>);

                    for i in 0..payload.len() {
                        // Store a unique repo/context/hash triplet
                        let address = Address {
                            context: context[i],
                            hash: hash[i],
                        };
                        let fragment = Fragment {
                            flags: 0,
                            size_payload: payload[i].len() as u32,
                            size_content: (1000 + i) as u64,
                        };
                        store
                            .clone()
                            .store(
                                repository,
                                address,
                                fragment,
                                Some(Bytes::copy_from_slice(payload[i].as_slice())),
                                false,
                            )
                            .await
                            .expect("Failed to store entry");
                    }

                    // Load the stuff back and verify
                    for i in 0..payload.len() {
                        // Store a unique repo/context/hash triplet
                        let address = Address {
                            context: context[i],
                            hash: hash[i],
                        };
                        let entry = store
                            .find(repository, address)
                            .await
                            .expect("Failed to query store entry");
                        assert_eq!(
                            entry.matching,
                            StoreMatch::MatchFull,
                            "Query did not match expected entry"
                        );
                        let buffer =
                            LocalImmutableStore::load(store.packstore(entry.group), entry.data)
                                .await
                                .expect("Failed to load store entry");
                        assert_eq!(
                            buffer.len(),
                            entry.data.size_payload as usize,
                            "Load did not load expected size"
                        );

                        let hash = Hash::hash_buffer(buffer.as_ref());
                        let expected_hash = Hash::hash_buffer(payload[i].as_slice());
                        assert_eq!(
                            hash, expected_hash,
                            "Loaded data is not equal to stored data"
                        );
                    }
                }

                let _ = std::fs::remove_dir_all(dir.as_path());
            })
            .await;
    }

    #[tokio::test]
    async fn store_and_update() {
        let tempdir = generate_tempdir();
        let dir = tempdir.to_path_buf();
        let _ = std::fs::remove_dir_all(dir.as_path());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let mut rng = rand::rng();
                let store = LocalImmutableStore::new(
                    Some(dir.clone()),
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let mut payload: Vec<Vec<u8>> = vec![];
                payload.resize_with(1024, || {
                    (0..64).map(|_| rng.random_range(0..=255)).collect()
                });

                for _iteration in 0..10 {
                    let mut hash: Vec<Hash> = vec![];
                    let mut i = 0;
                    hash.resize_with(payload.len(), || {
                        payload[i][0] = rand::random();
                        payload[i][1] = rand::random();
                        payload[i][2] = rand::random();
                        payload[i][3] = rand::random();
                        i += 1;
                        Hash::hash_buffer(payload[i - 1].as_slice())
                    });

                    // Put a couple of items in the same bucket
                    for i in 0..(hash.len() / 2) {
                        hash_set_byte(&mut hash[i], 0, 0);
                        hash_set_byte(&mut hash[i], 1, 0);
                    }

                    let repository = random::<RepositoryId>();

                    let mut context: Vec<Context> = vec![];
                    context.resize_with(payload.len(), random::<Context>);

                    for i in 0..payload.len() {
                        // Store a unique repo/context/hash triplet without payload
                        let address = Address {
                            context: context[i],
                            hash: hash[i],
                        };
                        let fragment = Fragment {
                            flags: 0,
                            size_payload: payload[i].len() as u32,
                            size_content: (1000 + i) as u64,
                        };
                        store
                            .clone()
                            .store(repository, address, fragment, None, false)
                            .await
                            .expect("Failed to store entry");
                    }

                    // Load the stuff back and verify payload was not cached
                    for i in 0..payload.len() {
                        // The unique repo/context/hash triplet
                        let address = Address {
                            context: context[i],
                            hash: hash[i],
                        };
                        let entry = store
                            .find(repository, address)
                            .await
                            .expect("Failed to query store entry");
                        assert_eq!(
                            entry.matching,
                            StoreMatch::MatchFull,
                            "Query did not match expected entry"
                        );
                        assert_eq!(
                            entry.data.pack_file, 0,
                            "Query did not match expected zero packfile"
                        );
                    }

                    for i in 0..payload.len() {
                        // Now store the unique repo/context/hash triplet WITH payload
                        let address = Address {
                            context: context[i],
                            hash: hash[i],
                        };
                        let fragment = Fragment {
                            flags: 0,
                            size_payload: payload[i].len() as u32,
                            size_content: (1000 + i) as u64,
                        };
                        store
                            .clone()
                            .store(
                                repository,
                                address,
                                fragment,
                                Some(Bytes::copy_from_slice(payload[i].as_slice())),
                                false,
                            )
                            .await
                            .expect("Failed to store entry");
                    }

                    // Load the stuff back and verify payload WAS cached
                    for i in 0..payload.len() {
                        // The unique repo/context/hash triplet
                        let address = Address {
                            context: context[i],
                            hash: hash[i],
                        };
                        let entry = store
                            .find(repository, address)
                            .await
                            .expect("Failed to query store entry");
                        assert_eq!(
                            entry.matching,
                            StoreMatch::MatchFull,
                            "Query did not match expected entry"
                        );
                        let buffer =
                            LocalImmutableStore::load(store.packstore(entry.group), entry.data)
                                .await
                                .expect("Failed to load store entry");
                        assert_eq!(
                            buffer.len(),
                            entry.data.size_payload as usize,
                            "Load did not load expected size"
                        );

                        let hash = Hash::hash_buffer(buffer.as_ref());
                        let expected_hash = Hash::hash_buffer(payload[i].as_slice());
                        assert_eq!(
                            hash, expected_hash,
                            "Loaded data is not equal to stored data"
                        );
                    }
                }

                let _ = std::fs::remove_dir_all(dir.as_path());
            })
            .await;
    }

    #[tokio::test]
    async fn store_new_payload() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings {
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to create store");

                // Insert a fragment without payload
                let repository = RepositoryId::from([0; 16]);
                let address = Address {
                    context: repository.into(),
                    hash: rand::random(),
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: 100,
                    size_content: 1000,
                };
                store
                    .clone()
                    .store(repository, address, fragment, None, false)
                    .await
                    .expect("Failed to store partial fragment");
                store
                    .clone()
                    .store(
                        repository,
                        Address {
                            context: rand::random(),
                            hash: address.hash,
                        },
                        fragment,
                        None,
                        false,
                    )
                    .await
                    .expect("Failed to store partial fragment");

                // Insert a fragment with same hash but with a payload
                let full_repository = random::<RepositoryId>();
                let full_address = Address {
                    context: repository.into(),
                    hash: address.hash,
                };

                let mut payload = BytesMut::new();
                payload.resize(100, 0u8);
                let payload = payload.freeze();
                store
                    .clone()
                    .store(
                        full_repository,
                        full_address,
                        fragment,
                        Some(payload.clone()),
                        false,
                    )
                    .await
                    .expect("Failed to store full fragment");

                // Lookup using the previous address with full match and ensure we got the full thing
                let (_fragment_found, payload_found) =
                    ImmutableStoreTrait::get(store.clone(), repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
                        .expect("Failed to get back first fragment on full match");

                assert_eq!(payload_found.as_bytes(), payload.as_bytes());

                // Lookup using the previous address with hash match and ensure we got the full thing
                let (_fragment_found, payload_found) =
                    ImmutableStoreTrait::get(store.clone(), repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
                        .expect("Failed to get back first fragment on hash match");

                assert_eq!(payload_found.as_bytes(), payload.as_bytes());
            })
            .await;
    }

    /// Put a fragment in repo A, copy to repo B, verify it is retrievable from B with `get()`,
    /// verify the source in repo A is unchanged, and verify a second copy returns Ok(()).
    #[tokio::test]
    async fn copy_fragment_between_repositories() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repo_a = RepositoryId::from([1u8; 16]);
                let repo_b = RepositoryId::from([2u8; 16]);
                let context = Context::from([3u8; 16]);

                let payload_data = b"hello copy world";
                let payload = Bytes::copy_from_slice(payload_data);
                let hash = lore_storage::hash::hash_slice(&payload);
                let address = Address { hash, context };

                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };

                // Store in repo A
                store
                    .clone()
                    .put(repo_a, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("Failed to store fragment in repo A");

                // Copy to repo B (preserve source context as the destination context — the
                // legacy cross-partition behavior).
                store
                    .clone()
                    .copy(repo_a, address, repo_b, address.context, false)
                    .await
                    .expect("Failed to copy fragment from repo A to repo B");

                // Fragment must be retrievable from repo B
                let (retrieved_fragment, retrieved_payload) = store
                    .clone()
                    .get(repo_b, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
                    .expect("Failed to get fragment from repo B after copy");

                assert_eq!(
                    retrieved_payload.as_ref(),
                    payload.as_ref(),
                    "Payload in repo B does not match original"
                );
                assert_eq!(
                    retrieved_fragment.size_content, fragment.size_content,
                    "Fragment size_content mismatch in repo B"
                );

                // Source in repo A must still be retrievable
                let (_, src_payload) = store
                    .clone()
                    .get(repo_a, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
                    .expect("Fragment in repo A should still be accessible after copy");
                assert_eq!(
                    src_payload.as_ref(),
                    payload.as_ref(),
                    "Payload in repo A changed after copy"
                );

                // Second copy call must be idempotent (Ok)
                store
                    .clone()
                    .copy(repo_a, address, repo_b, address.context, false)
                    .await
                    .expect("Second copy call should return Ok(()) (idempotent)");
            })
            .await;
    }

    /// Copying a non-existent fragment (hash not in store) returns `StoreError::NotFound`.
    #[tokio::test]
    async fn copy_nonexistent_fragment_returns_not_found() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repo_a = RepositoryId::from([1u8; 16]);
                let repo_b = RepositoryId::from([2u8; 16]);
                let context = Context::from([3u8; 16]);

                // Make an address that was never stored
                let hash = Hash::from([0xABu8; 32]);
                let address = Address { hash, context };

                let result = store
                    .clone()
                    .copy(repo_a, address, repo_b, address.context, false)
                    .await;
                assert!(
                    result.as_ref().is_err_and(|e| e.is_address_not_found()),
                    "Expected NotFound when copying a non-existent fragment, got {result:?}"
                );
            })
            .await;
    }

    /// Copying a fragment that exists in `repo_a` but using `repo_b` as source returns `StoreError::NotFound`.
    ///
    /// The fragment is present by hash but not associated with the requested source repository,
    /// so `exist` returns `MatchHash` and the copy must be rejected.
    #[tokio::test]
    async fn copy_wrong_source_repository_returns_not_found() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repo_a = RepositoryId::from([1u8; 16]);
                let repo_b = RepositoryId::from([2u8; 16]);
                let repo_c = RepositoryId::from([3u8; 16]);
                let context = Context::from([4u8; 16]);

                let payload_data = b"only in repo_a";
                let payload = Bytes::copy_from_slice(payload_data);
                let hash = lore_storage::hash::hash_slice(&payload);
                let address = Address { hash, context };

                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };

                // Store fragment only in repo_a.
                store
                    .clone()
                    .put(repo_a, address, fragment, Some(payload), false)
                    .await
                    .expect("Failed to store fragment in repo_a");

                // Copy using repo_b as source should fail — fragment is not in repo_b.
                let result = store
                    .clone()
                    .copy(repo_b, address, repo_c, address.context, false)
                    .await;
                assert!(
                    result.as_ref().is_err_and(|e| e.is_address_not_found()),
                    "Expected NotFound when source repository does not own the fragment, got {result:?}"
                );
            })
            .await;
    }

    // ------------------------------------------------------------------
    // Size-bound enforcement tests for the local immutable store
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn put_rejects_oversized_size_payload() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let mut hash = Hash::default();
                hash_set_byte(&mut hash, 0, 0xAA);
                let address = Address {
                    context: repository.into(),
                    hash,
                };
                let oversize = (FRAGMENT_SIZE_THRESHOLD + 1) as u32;
                let fragment = Fragment {
                    flags: 0,
                    size_payload: oversize,
                    size_content: oversize as u64,
                };
                // With a None payload and oversize declared — put must reject.
                let result = store
                    .clone()
                    .put(repository, address, fragment, None, false)
                    .await;
                assert!(
                    matches!(result, Err(StoreError::Oversized(_))),
                    "put with size_payload > threshold should return Oversized, got {result:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn put_rejects_oversized_payload() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let mut hash = Hash::default();
                hash_set_byte(&mut hash, 0, 0xBB);
                let address = Address {
                    context: repository.into(),
                    hash,
                };
                let oversize = FRAGMENT_SIZE_THRESHOLD + 1;
                let fragment = Fragment {
                    flags: 0,
                    size_payload: oversize as u32,
                    size_content: oversize as u64,
                };
                let payload = Bytes::from(vec![0u8; oversize]);
                let result = store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await;
                assert!(
                    matches!(result, Err(StoreError::Oversized(_))),
                    "put with oversized payload should return Oversized, got {result:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn put_rejects_payload_length_mismatch() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let mut hash = Hash::default();
                hash_set_byte(&mut hash, 0, 0xCC);
                let address = Address {
                    context: repository.into(),
                    hash,
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: 100,
                    size_content: 100,
                };
                // Payload is 50 bytes but size_payload declares 100 — must reject.
                let payload = Bytes::from(vec![0u8; 50]);
                let result = store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await;
                assert!(
                    matches!(result, Err(StoreError::Internal(_))),
                    "put with mismatched payload length should fail, got {result:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn store_rejects_oversized_size_payload() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let mut hash = Hash::default();
                hash_set_byte(&mut hash, 0, 0xDD);
                let address = Address {
                    context: repository.into(),
                    hash,
                };
                let oversize = FRAGMENT_SIZE_THRESHOLD + 1;
                let fragment = Fragment {
                    flags: 0,
                    size_payload: oversize as u32,
                    size_content: oversize as u64,
                };
                let payload = Bytes::from(vec![0u8; oversize]);
                let result = store
                    .clone()
                    .store(repository, address, fragment, Some(payload), false)
                    .await;
                assert!(result.is_err(), "store() must reject oversized fragment");
            })
            .await;
    }

    #[tokio::test]
    async fn store_fragment_rejects_oversized() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let mut hash = Hash::default();
                hash_set_byte(&mut hash, 0, 0xEE);
                let address = Address {
                    context: repository.into(),
                    hash,
                };
                let oversize = FRAGMENT_SIZE_THRESHOLD + 1;
                let fragment = Fragment {
                    flags: 0,
                    size_payload: oversize as u32,
                    size_content: oversize as u64,
                };
                let payload = Bytes::from(vec![0u8; oversize]);
                // store_fragment is the direct-put entry point used by store_raw
                // callers; it must also reject oversized fragments.
                let result = lore_storage::store_fragment(
                    store.clone(),
                    repository,
                    address,
                    fragment,
                    payload,
                    false,
                    None,
                    lore_storage::WriteContext::none(),
                    None,
                )
                .await;
                match &result {
                    Err(StorageError::Oversized(_)) => {}
                    _ => panic!("store_fragment with oversized payload should return Oversized"),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn store_fragment_rejects_length_mismatch() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repository = RepositoryId::from([0; 16]);
                let mut hash = Hash::default();
                hash_set_byte(&mut hash, 0, 0xFE);
                let address = Address {
                    context: repository.into(),
                    hash,
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: 128,
                    size_content: 128,
                };
                // Payload is 64 bytes but size_payload declares 128.
                let payload = Bytes::from(vec![0u8; 64]);
                let result = lore_storage::store_fragment(
                    store.clone(),
                    repository,
                    address,
                    fragment,
                    payload,
                    false,
                    None,
                    lore_storage::WriteContext::none(),
                    None,
                )
                .await;
                assert!(
                    result.is_err(),
                    "store_fragment with mismatched buffer length must fail"
                );
            })
            .await;
    }
}
