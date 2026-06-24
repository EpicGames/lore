// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
mod aws_store_tests {
    use std::error::Error;
    use std::sync::Arc;

    use async_trait::async_trait;
    use bytes::Bytes;
    use lore_aws::store::immutable_store::AwsImmutableStore;
    use lore_aws::store::immutable_store::AwsImmutableStoreSettings;
    use lore_aws::store::immutable_store::DynamoDbImmutableStoreSettings;
    use lore_aws::store::immutable_store::S3StoreSettings;
    use lore_aws::store::mutable_store::AwsMutableStore;
    use lore_aws::store::mutable_store::AwsMutableStoreSettings;
    use lore_aws::store::mutable_store::DynamoDbMutableStoreSettings;
    use lore_base::error::AddressNotFound;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_base::types::Partition;
    use lore_revision::fragment;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::lore::RepositoryId;
    use lore_revision::lore::execution_context;
    use lore_revision::store::composite::CompositeStore;
    use lore_revision::store::composite::CompositeStoreBuilder;
    use lore_storage::CompressionMode;
    use lore_storage::FRAGMENT_COMPRESS_SIZE_LIMIT;
    use lore_storage::ImmutableStore;
    use lore_storage::MutableStore;
    use lore_storage::StoreError;
    use lore_storage::StoreMatch;
    use lore_storage::StoreObliterateStats;
    use lore_storage::StoreQueryResult;
    use rand::random;

    use crate::common::aws_common::FRAGMENT_METADATA_TABLE_NAME;
    use crate::common::aws_common::FRAGMENTS_TABLE_NAME;
    use crate::common::aws_common::MUTABLE_STORE_TABLE_NAME;
    use crate::common::aws_common::STORE_BUCKET_NAME;
    use crate::common::aws_common::setup;
    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    /// Apply the key type prefix to a hash, matching what the mutable store does internally.
    /// The store replaces byte 0 of the key with the key type discriminant.
    fn typed_key(mut key: Hash, key_type: KeyType) -> Hash {
        key.data_mut()[0] = key_type as u8;
        key
    }

    #[derive(Default)]
    struct LocalStore {
        local_exists_addresses: Vec<Address>,
    }

    impl LocalStore {
        fn new(local_exists_addresses: Vec<Address>) -> Self {
            Self {
                local_exists_addresses,
            }
        }
    }

    #[async_trait]
    impl ImmutableStore for LocalStore {
        async fn exist(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreMatch, StoreError> {
            Ok(StoreMatch::MatchNone)
        }

        async fn exist_batch(
            self: Arc<Self>,
            _repository: Partition,
            addresses: &[Address],
            _match_requested: StoreMatch,
        ) -> Result<Vec<StoreMatch>, StoreError> {
            let mut output = vec![];

            for address in addresses {
                if self.local_exists_addresses.contains(address) {
                    output.push(StoreMatch::MatchFull);
                } else {
                    output.push(StoreMatch::MatchNone);
                }
            }

            Ok(output)
        }

        async fn query(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreQueryResult, StoreError> {
            Ok(StoreQueryResult {
                fragment: Fragment::default(),
                match_made: StoreMatch::MatchNone,
            })
        }

        async fn get(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _match_required: StoreMatch,
        ) -> Result<(Fragment, Bytes), StoreError> {
            Err(StoreError::from(AddressNotFound::from(Address::default())))
        }

        async fn put(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _fragment: Fragment,
            _payload: Option<Bytes>,
            _force: bool,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("Store does not support operation"))
        }

        async fn obliterate(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _stats: Arc<StoreObliterateStats>,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("Store does not support operation"))
        }

        async fn evict(
            self: Arc<Self>,
            _max_capacity: usize,
            _sync_data: bool,
            _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<usize, StoreError> {
            Ok(0)
        }

        async fn compact(
            self: Arc<Self>,
            _max_size: usize,
            _at: Option<usize>,
            _sync_data: bool,
            _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            Ok(None)
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            None
        }

        async fn compact_stop(self: Arc<Self>) {}

        async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
            Ok(())
        }

        async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
            Ok(())
        }

        fn max_query_batch(&self) -> Option<usize> {
            Some(100)
        }
    }

    async fn initialize_store() -> Result<
        (
            Arc<CompositeStore>,
            Arc<AwsMutableStore>,
            Arc<ExecutionContext>,
        ),
        Box<dyn Error>,
    > {
        initialize_store_with_matches(vec![]).await
    }

    async fn initialize_store_with_matches(
        local_exists_addresses: Vec<Address>,
    ) -> Result<
        (
            Arc<CompositeStore>,
            Arc<AwsMutableStore>,
            Arc<ExecutionContext>,
        ),
        Box<dyn Error>,
    > {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (s3, dynamo_immutable, dynamo_mutable) = setup(vec![
                    MUTABLE_STORE_TABLE_NAME,
                    FRAGMENTS_TABLE_NAME,
                    FRAGMENT_METADATA_TABLE_NAME,
                ])
                .await?;

                let aws_immutable_settings = AwsImmutableStoreSettings::new(
                    S3StoreSettings::new(STORE_BUCKET_NAME.to_string()),
                    DynamoDbImmutableStoreSettings::new(
                        FRAGMENTS_TABLE_NAME.to_string(),
                        FRAGMENT_METADATA_TABLE_NAME.to_string(),
                    ),
                    false,
                );

                let mutable_settings = AwsMutableStoreSettings::new(
                    DynamoDbMutableStoreSettings::new(MUTABLE_STORE_TABLE_NAME.to_string()),
                    false,
                );

                let aws_immutable_store =
                    AwsImmutableStore::new(s3, dynamo_immutable, &aws_immutable_settings);

                let local_immutable_store = LocalStore::new(local_exists_addresses);

                let builder = CompositeStoreBuilder::default()
                    .with_durable("aws".to_string(), Arc::new(aws_immutable_store))
                    .expect("Failed to assign AWS durable immutable store")
                    .with_local("local".to_string(), Arc::new(local_immutable_store))
                    .expect("Failed to assign local immutable store");

                let immutable_store = builder.build().expect("Failed to build composite store");
                let immutable_store = Arc::new(immutable_store);

                let mutable_store = AwsMutableStore::new(
                    dynamo_mutable,
                    &mutable_settings,
                    immutable_store.clone(),
                );
                let mutable_store = Arc::new(mutable_store);

                Ok((
                    immutable_store.clone(),
                    mutable_store.clone(),
                    execution_context(),
                ))
            })
            .await
    }

    #[tokio::test]
    async fn test_exist_batch() -> TestResult {
        let repository = random::<RepositoryId>();

        let (_, address_found_local, _) = fragment::generate_random();
        let (fragment, address_found_durable, payload) = fragment::generate_random();
        let (_, address_not_found, _) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) =
            initialize_store_with_matches(vec![address_found_local])
                .await
                .expect("Failed to create store");

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .put(
                        repository,
                        address_found_durable,
                        fragment,
                        Some(payload),
                        false,
                    )
                    .await?;

                let addresses = vec![
                    address_found_local,
                    address_found_durable,
                    address_not_found,
                ];
                let result = immutable_store
                    .clone()
                    .exist_batch(repository, addresses.as_slice(), StoreMatch::MatchFull)
                    .await?;

                assert_eq!(
                    vec![
                        StoreMatch::MatchFull,
                        StoreMatch::MatchFull,
                        StoreMatch::MatchNone
                    ],
                    result
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_immutable_not_found() -> TestResult {
        let repository = random::<RepositoryId>();
        let address = random::<Address>();

        let (immutable_store, _mutable_store, execution) =
            initialize_store().await.expect("Failed to create store");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let result = immutable_store
                    .clone()
                    .query(repository, address, StoreMatch::MatchFull)
                    .await?;

                assert_eq!(
                    StoreQueryResult {
                        fragment: Fragment::default(),
                        match_made: StoreMatch::MatchNone
                    },
                    result
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_immutable_found() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) =
            initialize_store().await.expect("Failed to create store");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await?;

                let result = immutable_store
                    .clone()
                    .query(repository, address, StoreMatch::MatchFull)
                    .await
                    .unwrap();

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(
                    StoreQueryResult {
                        fragment: want_fragment,
                        match_made: StoreMatch::MatchFull
                    },
                    result
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_immutable_partial_match() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await?;

                let mut address = address;
                address.context = random::<Context>();

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(
                    StoreQueryResult {
                        fragment: want_fragment,
                        match_made: StoreMatch::MatchPartition
                    },
                    immutable_store
                        .clone()
                        .query(repository, address, StoreMatch::MatchPartition)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_lower_specificity_match() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await?;

                let mut address = address;
                address.context = random::<Context>();

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(
                    StoreQueryResult {
                        fragment: want_fragment,
                        match_made: StoreMatch::MatchHash
                    },
                    immutable_store
                        .clone()
                        .query(
                            random::<RepositoryId>(),
                            address,
                            StoreMatch::MatchPartition
                        )
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    #[ignore] // Partial puts are not currently supported
    async fn test_put_immutable_partial() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Put the fragment with an initial context.
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await?;

                let mut address = address;
                address.context = random::<Context>();

                // If we query the fragment with this new address we should get a repository match.
                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(
                    StoreQueryResult {
                        fragment: want_fragment,
                        match_made: StoreMatch::MatchPartition
                    },
                    immutable_store
                        .clone()
                        .query(repository, address, StoreMatch::MatchFull)
                        .await?
                );

                // Put the fragment again with a separate context in the same repo, but send no payload
                immutable_store
                    .clone()
                    .put(repository, address, fragment, None, false)
                    .await?;

                // Now if we query it we should get a full match.
                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(
                    StoreQueryResult {
                        fragment: want_fragment,
                        match_made: StoreMatch::MatchFull
                    },
                    immutable_store
                        .clone()
                        .query(repository, address, StoreMatch::MatchFull)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_put_immutable_partial_hash_collision() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Put the fragment with an initial context.
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                let mut invalid_fragment = fragment;
                invalid_fragment.size_content *= 2;

                assert!(
                    immutable_store
                        .clone()
                        .put(
                            repository,
                            address,
                            invalid_fragment,
                            Some(payload.clone()),
                            false
                        )
                        .await
                        .expect_err("should have returned an error")
                        .is_internal()
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_put_immutable_payload_required() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Put the fragment
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                // Try to put the same fragment without a payload to a different repository, we should be
                // prevented from doing so.
                let another_repository = random::<RepositoryId>();
                assert!(
                    immutable_store
                        .clone()
                        .put(another_repository, address, fragment, None, false)
                        .await
                        .expect_err("should have returned an error")
                        .is_internal()
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_get_immutable() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                let (got_fragment, got_buffer) = immutable_store
                    .get(repository, address, StoreMatch::MatchHash)
                    .await
                    .expect("Failed to get immutable object");

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(want_fragment, got_fragment);

                assert_eq!(payload.as_ref(), got_buffer.as_ref());

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_get_immutable_not_found() -> TestResult {
        let repository = random::<RepositoryId>();
        let address = random::<Address>();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                assert!(
                    immutable_store
                        .clone()
                        .get(repository, address, StoreMatch::MatchHash,)
                        .await
                        .expect_err("should have returned an error")
                        .is_address_not_found()
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_get_immutable_partial_match() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                let mut address = address;
                address.context = random::<Context>();

                // Getting the fragment with a different context should still return the fragment as long as we
                // specify a repository match.
                let (got_fragment, got_buffer) = immutable_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchPartition)
                    .await
                    .expect("Failed to get immutable object");

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(want_fragment, got_fragment);

                assert_eq!(payload.as_ref(), got_buffer.as_ref());

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_get_immutable_as_buffer_compressed_data() -> TestResult {
        let repository = random::<RepositoryId>();

        let mut payload = vec![];

        // In order to generate a payload that `fragment::compress` is willing to compress, just
        // repeat the data a few times to ensure there's lots of room for compression.
        let data = random::<[u8; FRAGMENT_COMPRESS_SIZE_LIMIT]>();
        for _ in 1..10 {
            payload.extend(data);
        }

        let hash = Hash::hash_buffer(payload.as_slice());

        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let (fragment, compressed_payload) = lore_storage::compress::compress(
                    fragment,
                    payload.as_slice(),
                    CompressionMode::Lz4,
                )
                .expect("Failed to compress payload");

                let address = Address {
                    hash,
                    ..Default::default()
                };

                immutable_store
                    .clone()
                    .put(
                        repository,
                        address,
                        fragment,
                        Some(compressed_payload.clone()),
                        false,
                    )
                    .await?;

                let (got_fragment, got_buffer) = immutable_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchHash)
                    .await
                    .expect("Failed to get immutable object");

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(want_fragment, got_fragment);

                assert_eq!(compressed_payload.as_ref(), got_buffer.as_ref());

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_get_immutable_as_buffer_uncompressed_data_maximum_fragment_size() -> TestResult {
        let repository = random::<RepositoryId>();
        let payload: Vec<u8> = (0..FRAGMENT_SIZE_THRESHOLD)
            .map(|_| random::<u8>())
            .collect();
        let payload = Bytes::copy_from_slice(payload.as_slice());
        let hash = Hash::hash_buffer(payload.as_ref());
        let context = random::<Context>();

        let address = Address { hash, context };
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("Failed to put immutable object");

                let (got_fragment, got_buffer) = immutable_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchHash)
                    .await
                    .expect("Failed to get immutable object");

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits();
                assert_eq!(want_fragment, got_fragment);

                assert_eq!(payload.as_ref(), got_buffer.as_ref());

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_multistore_immutable_transitions() -> TestResult {
        let (immutable_store_one, _mutable_store, execution) = initialize_store().await?;
        let (immutable_store_two, _mutable_store, _execution) = initialize_store().await?;

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository = random::<RepositoryId>();
                let (fragment, address, payload) = fragment::generate_random();

                immutable_store_one
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("Failed to put immutable object");

                let (got_fragment, got_payload) = immutable_store_two
                    .get(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("Failed to get immutable object");

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);

                assert_eq!(want_fragment, got_fragment);
                assert_eq!(payload.as_ref(), got_payload.as_ref());

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_batch_limit() -> TestResult {
        let repository = random::<RepositoryId>();

        let (immutable_store, _mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let mut address = vec![];
                address.resize_with(10000, random::<Address>);

                let mut expected = vec![];
                expected.resize(10000, StoreMatch::MatchNone);

                let result = immutable_store
                    .clone()
                    .exist_batch(repository, &address, StoreMatch::MatchFull)
                    .await
                    .expect("Failed to query exist batch");
                assert_eq!(result, expected);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_load_mutable() -> TestResult {
        let hash = random::<Hash>();
        let value = random::<Hash>();
        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                mutable_store
                    .clone()
                    .store(repository, hash, value, KeyType::BranchId)
                    .await?;

                assert_eq!(
                    value,
                    mutable_store
                        .load(repository, hash, KeyType::BranchId)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_load_mutable_not_found() -> TestResult {
        let hash = random::<Hash>();
        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                assert!(
                    mutable_store
                        .load(repository, hash, KeyType::Untyped)
                        .await
                        .expect_err("should have gotten an error")
                        .is_address_not_found()
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_store_mutable_zeroed_value() -> TestResult {
        let hash = random::<Hash>();
        let initial_value = random::<Hash>();
        let other_value = random::<Hash>();
        let value = Hash::default();
        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                mutable_store
                    .clone()
                    .store(repository, hash, initial_value, KeyType::BranchMetadata)
                    .await?;
                mutable_store
                    .clone()
                    .store(repository, hash, other_value, KeyType::Untyped)
                    .await?;
                assert_eq!(
                    initial_value,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::BranchMetadata)
                        .await?
                );
                assert_eq!(
                    other_value,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::Untyped)
                        .await?
                );

                mutable_store
                    .clone()
                    .store(repository, hash, value, KeyType::BranchMetadata)
                    .await?;

                assert!(
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::BranchMetadata)
                        .await
                        .expect_err("should have gotten an error")
                        .is_address_not_found()
                );
                assert_eq!(
                    other_value,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::Untyped)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_compare_and_swap_mutable() -> TestResult {
        let hash = random::<Hash>();
        let value = random::<Hash>();
        let expected = random::<Hash>();
        let different = random::<Hash>();

        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                mutable_store
                    .clone()
                    .store(repository, hash, expected, KeyType::Untyped)
                    .await?;
                assert_eq!(
                    expected,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::Untyped)
                        .await?
                );

                // We compare and swap expecting the value to be "different" but it's actually "expected",
                // which is what should be returned.
                assert_eq!(
                    expected,
                    mutable_store
                        .clone()
                        .compare_and_swap(repository, hash, different, value, KeyType::Untyped)
                        .await?
                );

                // Verify the value is still "expected" in the store.
                assert_eq!(
                    expected,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::Untyped)
                        .await?
                );

                // Try again, this time we actually expect the value to be "expected" which is again what's
                // returned.
                assert_eq!(
                    expected,
                    mutable_store
                        .clone()
                        .compare_and_swap(repository, hash, expected, value, KeyType::Untyped)
                        .await?
                );

                // Now we verify that the value was actually replaced with "value".
                assert_eq!(
                    value,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::Untyped)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_compare_and_swap_mutable_not_found() -> TestResult {
        let hash = random::<Hash>();
        let value = random::<Hash>();
        let expected = random::<Hash>();

        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // If we try to compare and swap a non-existent key with an expected value, we should just
                // get back an empty hash.
                assert_eq!(
                    Hash::default(),
                    mutable_store
                        .clone()
                        .compare_and_swap(repository, hash, expected, value, KeyType::Untyped)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_compare_and_swap_mutable_not_found_expected() -> TestResult {
        let hash = random::<Hash>();
        let value = random::<Hash>();
        let expected = Hash::default();

        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // If we try to compare and swap a non-existent key with an empty expected value, we should
                // perform the write and get back the written value.
                assert_eq!(
                    expected,
                    mutable_store
                        .clone()
                        .compare_and_swap(repository, hash, expected, value, KeyType::BranchId)
                        .await?
                );

                // Verify that the value was actually replaced with "value".
                assert_eq!(
                    value,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::BranchId)
                        .await?
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_list_mutable_branch_ids() -> TestResult {
        let repository = random::<RepositoryId>();
        let key1 = random::<Hash>();
        let key2 = random::<Hash>();
        let key3 = random::<Hash>();
        let value1 = random::<Hash>();
        let value2 = random::<Hash>();
        let value3 = random::<Hash>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                mutable_store
                    .clone()
                    .store(repository, key1, value1, KeyType::BranchId)
                    .await?;
                mutable_store
                    .clone()
                    .store(repository, key2, value2, KeyType::BranchId)
                    .await?;
                mutable_store
                    .clone()
                    .store(repository, key3, value3, KeyType::BranchId)
                    .await?;

                let stream = mutable_store
                    .clone()
                    .list(repository, KeyType::BranchId)
                    .await?;

                let mut channel = stream.channel();
                let mut results = Vec::new();
                while let Some(pair) = channel.recv().await {
                    results.push(pair);
                }

                assert_eq!(results.len(), 3);

                let mut expected = vec![
                    (typed_key(key1, KeyType::BranchId), value1),
                    (typed_key(key2, KeyType::BranchId), value2),
                    (typed_key(key3, KeyType::BranchId), value3),
                ];
                expected.sort_by_key(|(k, _)| *k);
                let mut actual = results;
                actual.sort_by_key(|(k, _)| *k);
                assert_eq!(actual, expected);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_list_mutable_empty() -> TestResult {
        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let stream = mutable_store
                    .clone()
                    .list(repository, KeyType::BranchId)
                    .await?;

                let mut channel = stream.channel();
                let mut results = Vec::new();
                while let Some(pair) = channel.recv().await {
                    results.push(pair);
                }

                assert!(results.is_empty());

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_list_mutable_filters_by_key_type() -> TestResult {
        let repository = random::<RepositoryId>();
        let branch_key = random::<Hash>();
        let metadata_key = random::<Hash>();
        let branch_value = random::<Hash>();
        let metadata_value = random::<Hash>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                mutable_store
                    .clone()
                    .store(repository, branch_key, branch_value, KeyType::BranchId)
                    .await?;
                mutable_store
                    .clone()
                    .store(
                        repository,
                        metadata_key,
                        metadata_value,
                        KeyType::BranchMetadata,
                    )
                    .await?;

                let mut branch_channel = mutable_store
                    .clone()
                    .list(repository, KeyType::BranchId)
                    .await?
                    .channel();
                let mut branch_results = Vec::new();
                while let Some(pair) = branch_channel.recv().await {
                    branch_results.push(pair);
                }

                assert_eq!(branch_results.len(), 1);
                assert_eq!(branch_results[0].1, branch_value);

                let mut metadata_channel = mutable_store
                    .clone()
                    .list(repository, KeyType::BranchMetadata)
                    .await?
                    .channel();
                let mut metadata_results = Vec::new();
                while let Some(pair) = metadata_channel.recv().await {
                    metadata_results.push(pair);
                }

                assert_eq!(metadata_results.len(), 1);
                assert_eq!(metadata_results[0].1, metadata_value);

                Ok(())
            })
            .await
    }

    /// Inserts enough `BranchId` entries to exceed `DynamoDB`'s 1MB page limit,
    /// forcing the streaming pagination in `list_typed` to fetch multiple pages.
    /// Each item is ~300 bytes in `DynamoDB`, so 4000 items (~1.2MB) guarantees
    /// at least two pages.
    #[tokio::test]
    async fn test_list_mutable_paginated() -> TestResult {
        let repository = random::<RepositoryId>();
        let count = 4000;

        let mut expected: Vec<(Hash, Hash)> = (0..count)
            .map(|_| (random::<Hash>(), random::<Hash>()))
            .collect();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                for (key, value) in &expected {
                    mutable_store
                        .clone()
                        .store(repository, *key, *value, KeyType::BranchId)
                        .await?;
                }

                let mut channel = mutable_store
                    .clone()
                    .list(repository, KeyType::BranchId)
                    .await?
                    .channel();
                let mut results = Vec::new();
                while let Some(pair) = channel.recv().await {
                    results.push(pair);
                }

                assert_eq!(results.len(), count);

                // Keys stored in DynamoDB have byte 0 replaced with the key type prefix
                for (key, _) in &mut expected {
                    *key = typed_key(*key, KeyType::BranchId);
                }
                expected.sort_by_key(|(k, _)| *k);
                results.sort_by_key(|(k, _)| *k);
                assert_eq!(results, expected);

                Ok(())
            })
            .await
    }
}
