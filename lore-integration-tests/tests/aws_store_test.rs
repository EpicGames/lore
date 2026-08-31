// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
mod aws_store_tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::sync::Arc;

    use async_trait::async_trait;
    use aws_sdk_dynamodb::primitives::Blob;
    use aws_sdk_dynamodb::types::AttributeValue;
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
    use lore_storage::StoreGetData;
    use lore_storage::StoreMatch;
    use lore_storage::StoreMatchResult;
    use lore_storage::StoreObliterateStats;
    use lore_storage::immutable_store::query_one;
    use rand::random;

    use crate::common::aws_common::FRAGMENT_METADATA_TABLE_NAME;
    use crate::common::aws_common::FRAGMENT_STATE_TABLE_NAME;
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
        async fn get_metadata(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
        ) -> Result<StoreGetData, StoreError> {
            Ok(StoreGetData::default())
        }

        async fn query(
            self: Arc<Self>,
            repository: Partition,
            addresses: &[Address],
            results: &mut [StoreMatchResult],
        ) -> Result<(), StoreError> {
            for (address, result) in addresses.iter().zip(results.iter_mut()) {
                *result = if self.local_exists_addresses.contains(address) {
                    StoreMatchResult {
                        match_made: StoreMatch::MatchFull,
                        partition: repository,
                        context: address.context,
                        stored_local: true,
                        stored_durable: false,
                    }
                } else {
                    StoreMatchResult::default()
                };
            }

            Ok(())
        }

        async fn get(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
        ) -> Result<StoreGetData, StoreError> {
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

        async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
            Ok(())
        }

        async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
            Ok(())
        }

        async fn copy(
            self: Arc<Self>,
            _source_partition: Partition,
            _source_address: Address,
            _destination_partition: Partition,
            _destination_context: Context,
            _durable: bool,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("Copy not supported by this store"))
        }

        fn max_query_batch(&self) -> Option<usize> {
            Some(100)
        }
    }

    /// An object stored before the fragment moved onto it: bare bytes in S3, the fragment in a row
    /// of the fragment metadata table. Reading one must still work, since the cut-over does not
    /// rewrite the existing population — this is the whole of the migration's read path.
    #[tokio::test]
    async fn test_get_immutable_falls_back_to_the_fragment_metadata_table() -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (s3, dynamo, _) = setup(vec![
                    MUTABLE_STORE_TABLE_NAME,
                    FRAGMENTS_TABLE_NAME,
                    FRAGMENT_METADATA_TABLE_NAME,
                ])
                .await?;

                let settings = AwsImmutableStoreSettings::new(
                    S3StoreSettings::new(STORE_BUCKET_NAME.to_string()),
                    DynamoDbImmutableStoreSettings::new(
                        FRAGMENTS_TABLE_NAME.to_string(),
                        FRAGMENT_METADATA_TABLE_NAME.to_string(),
                    )
                    .with_fragment_metadata_table(FRAGMENT_METADATA_TABLE_NAME.to_string()),
                    false,
                );
                let store = Arc::new(AwsImmutableStore::new(s3, dynamo, &settings));

                // Store it the current way first, so the association and the keys are real.
                store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                let (raw_s3, raw_dynamo, _) = setup(vec![]).await?;

                // Then put it back the way that era stored it: bare bytes, no object metadata.
                let mut dst = [0u8; 64];
                let key = lore_revision::util::to_hex_str(address.hash.data(), &mut dst);
                raw_s3
                    .put_object(STORE_BUCKET_NAME, key, payload.clone(), None)
                    .await
                    .expect("rewriting the object without metadata should succeed");

                // And the row that era wrote: the whole fragment flattened, with no state attribute.
                let row = HashMap::from([
                    (
                        "hash".to_string(),
                        AttributeValue::B(Blob::new(address.hash.data().to_vec())),
                    ),
                    (
                        "flags".to_string(),
                        AttributeValue::N(fragment.flags.to_string()),
                    ),
                    (
                        "size_payload".to_string(),
                        AttributeValue::N(fragment.size_payload.to_string()),
                    ),
                    (
                        "size_content".to_string(),
                        AttributeValue::N(fragment.size_content.to_string()),
                    ),
                ]);
                raw_dynamo
                    .put_item(&Arc::<str>::from(FRAGMENT_METADATA_TABLE_NAME), row)
                    .await
                    .expect("writing the pre-cut-over row should succeed");

                let (got_fragment, got_payload) = store
                    .get(repository, address)
                    .await
                    .and_then(StoreGetData::into_payload)?;

                assert_eq!(got_payload, payload, "the payload must read back intact");
                assert_eq!(got_fragment.size_payload, fragment.size_payload);
                assert_eq!(got_fragment.size_content, fragment.size_content);
                assert_eq!(
                    got_fragment.flags & FragmentFlags::PayloadStoredDurable,
                    FragmentFlags::PayloadStoredDurable,
                    "durability is derived, not read from the row"
                );

                Ok(())
            })
            .await
    }

    /// The same object on a deployment that never stored one that way. Leaving the fragment
    /// metadata table unconfigured declares no such object exists, so this one is damaged rather
    /// than old — and must be reported, not described from a row that cannot be about it.
    #[tokio::test]
    async fn test_get_immutable_without_a_fragment_metadata_table_reports_the_object_as_damaged()
    -> TestResult {
        let repository = random::<RepositoryId>();
        let (fragment, address, payload) = fragment::generate_random();

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (s3, dynamo, _) = setup(vec![
                    MUTABLE_STORE_TABLE_NAME,
                    FRAGMENTS_TABLE_NAME,
                    FRAGMENT_METADATA_TABLE_NAME,
                ])
                .await?;

                let settings = AwsImmutableStoreSettings::new(
                    S3StoreSettings::new(STORE_BUCKET_NAME.to_string()),
                    DynamoDbImmutableStoreSettings::new(
                        FRAGMENTS_TABLE_NAME.to_string(),
                        FRAGMENT_METADATA_TABLE_NAME.to_string(),
                    ),
                    false,
                );
                let store = Arc::new(AwsImmutableStore::new(s3, dynamo, &settings));

                store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await?;

                let (raw_s3, _, _) = setup(vec![]).await?;
                let mut dst = [0u8; 64];
                let key = lore_revision::util::to_hex_str(address.hash.data(), &mut dst);
                raw_s3
                    .put_object(STORE_BUCKET_NAME, key, payload.clone(), None)
                    .await
                    .expect("rewriting the object without metadata should succeed");

                let error = store
                    .get(repository, address)
                    .await
                    .expect_err("an object with no metadata and nowhere to look is damaged");

                assert!(
                    error.is_internal(),
                    "must be reported as an error, not as a miss that a client would retry into"
                );
                assert!(
                    format!("{error:?}").contains("carries no fragment metadata"),
                    "the error should say what is wrong with the object, got: {error:?}"
                );

                Ok(())
            })
            .await
    }

    /// Wraps the durable store to record which write verb it was asked for, so a test can say the
    /// payload never reached S3 a second time rather than only that the read works.
    struct CountingDurable {
        inner: Arc<dyn ImmutableStore>,
        puts: std::sync::atomic::AtomicUsize,
        copies: std::sync::atomic::AtomicUsize,
    }

    impl CountingDurable {
        fn new(inner: Arc<dyn ImmutableStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                puts: std::sync::atomic::AtomicUsize::new(0),
                copies: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn puts(&self) -> usize {
            self.puts.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn copies(&self) -> usize {
            self.copies.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ImmutableStore for CountingDurable {
        fn is_local(&self) -> bool {
            self.inner.is_local()
        }

        fn isolates_partitions(&self) -> bool {
            self.inner.isolates_partitions()
        }

        fn read_scope(&self) -> StoreMatch {
            self.inner.read_scope()
        }

        fn query_scope(&self) -> StoreMatch {
            self.inner.query_scope()
        }

        async fn get(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get(partition, address).await
        }

        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        async fn query(
            self: Arc<Self>,
            partition: Partition,
            addresses: &[Address],
            results: &mut [StoreMatchResult],
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn put(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            fragment: Fragment,
            payload: Option<Bytes>,
            force: bool,
        ) -> Result<(), StoreError> {
            self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await
        }

        async fn copy(
            self: Arc<Self>,
            source_partition: Partition,
            source_address: Address,
            destination_partition: Partition,
            destination_context: Context,
            durable: bool,
        ) -> Result<(), StoreError> {
            self.copies
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner
                .clone()
                .copy(
                    source_partition,
                    source_address,
                    destination_partition,
                    destination_context,
                    durable,
                )
                .await
        }

        async fn obliterate(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            stats: Arc<StoreObliterateStats>,
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .obliterate(partition, address, stats)
                .await
        }

        async fn evict(
            self: Arc<Self>,
            max_capacity: usize,
            sync_data: bool,
            sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<usize, StoreError> {
            self.inner
                .clone()
                .evict(max_capacity, sync_data, sink)
                .await
        }

        async fn compact(
            self: Arc<Self>,
            max_size: usize,
            at: Option<usize>,
            sync_data: bool,
            sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            self.inner
                .clone()
                .compact(max_size, at, sync_data, sink)
                .await
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            self.inner.clone().compact_resume_at().await
        }

        fn max_query_batch(&self) -> Option<usize> {
            self.inner.max_query_batch()
        }

        async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
            self.inner.clone().flush(sync_data).await
        }

        async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
            self.inner.clone().verify(heal).await
        }
    }

    /// A composite shaped like a server's: a real in-memory local cache in front of the AWS store,
    /// which is what lets a second put see the first one's association and duplicate it.
    async fn initialize_store_with_local_cache() -> Result<
        (
            Arc<CompositeStore>,
            Arc<CountingDurable>,
            Arc<ExecutionContext>,
        ),
        Box<dyn Error>,
    > {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (s3, dynamo_immutable, _dynamo_mutable) = setup(vec![
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

                let durable = CountingDurable::new(Arc::new(AwsImmutableStore::new(
                    s3,
                    dynamo_immutable,
                    &aws_immutable_settings,
                )));

                let local = lore_storage::local::immutable_store::create(
                    None::<&str>,
                    lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
                    false,
                    lore_storage::local::immutable_store::ImmutableStoreSettings {
                        isolate_partitions: true,
                        ..Default::default()
                    },
                )
                .await?;

                let immutable_store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("aws".to_string(), durable.clone())
                        .expect("Failed to assign AWS durable immutable store")
                        .with_local("local".to_string(), local)
                        .expect("Failed to assign local immutable store")
                        .build()
                        .expect("Failed to build composite store"),
                );

                Ok((immutable_store, durable, execution_context()))
            })
            .await
    }

    /// A put whose content the durable store already holds under another context is registered with
    /// a copy, and the target address reads back the deduplicated payload once the put returns.
    #[tokio::test]
    async fn put_duplicating_a_durable_association_reads_back_from_the_target() -> TestResult {
        let partition = random::<RepositoryId>();
        let (fragment, first, payload) = fragment::generate_random();
        let second = Address {
            hash: first.hash,
            context: random::<Context>(),
        };

        let (store, durable, execution) = initialize_store_with_local_cache()
            .await
            .expect("Failed to create store");

        LORE_CONTEXT
            .scope(execution.clone(), async move {
                store
                    .clone()
                    .put(partition, first, fragment, Some(payload.clone()), false)
                    .await
                    .expect("first put should store the payload");
                assert_eq!(durable.puts(), 1, "the first put has to upload the payload");
                assert_eq!(durable.copies(), 0);

                // The composite caches into the local store detached, and it is that cached
                // association the second put names as its source.
                for _ in 0..50 {
                    let cached = query_one(&store.local(), partition, first)
                        .await
                        .expect("query the local cache");
                    if cached.match_made == StoreMatch::MatchFull && cached.stored_durable {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }

                store
                    .clone()
                    .put(partition, second, fragment, Some(payload.clone()), false)
                    .await
                    .expect("second put should duplicate the association");

                assert_eq!(
                    durable.copies(),
                    1,
                    "the durable store already holds these bytes, so it must be asked to copy"
                );
                assert_eq!(
                    durable.puts(),
                    1,
                    "no second upload: the payload must not reach S3 again"
                );

                let (read_fragment, read_payload) = store
                    .clone()
                    .get(partition, second)
                    .await
                    .and_then(StoreGetData::into_payload)
                    .expect("the target address must read back after the put");
                assert_eq!(read_payload, payload, "the deduplicated bytes must match");
                assert_eq!(read_fragment.size_content, fragment.size_content);

                // And it is the durable store that holds it, not merely the cache in front: the
                // association is in DynamoDB and S3 serves the payload for the target address.
                let on_durable = query_one(
                    &(durable.clone() as Arc<dyn ImmutableStore>),
                    partition,
                    second,
                )
                .await
                .expect("query the durable store");
                assert_eq!(
                    on_durable.match_made,
                    StoreMatch::MatchFull,
                    "the durable store must hold the target association"
                );

                let (_, from_durable) = durable
                    .clone()
                    .get(partition, second)
                    .await
                    .and_then(StoreGetData::into_payload)
                    .expect("the durable store must serve the target address");
                assert_eq!(
                    from_durable, payload,
                    "the durable store must serve the deduplicated bytes for the target address"
                );

                Ok(())
            })
            .await
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
                let mut result = [StoreMatchResult::default(); 3];
                immutable_store
                    .clone()
                    .query(repository, addresses.as_slice(), &mut result)
                    .await?;

                assert_eq!(
                    result.map(|entry| entry.match_made),
                    [
                        StoreMatch::MatchFull,
                        StoreMatch::MatchFull,
                        StoreMatch::MatchNone
                    ],
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
                let result = query_one(
                    &(immutable_store as Arc<dyn ImmutableStore>),
                    repository,
                    address,
                )
                .await?;

                assert_eq!(StoreMatchResult::default(), result);

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
                    .get_metadata(repository, address)
                    .await
                    .unwrap();

                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                assert_eq!(result.fragment, want_fragment);
                assert_eq!(result.match_made, StoreMatch::MatchFull);
                assert_eq!(result.partition, repository);

                Ok(())
            })
            .await
    }

    /// The same hash under a sibling context, on a store that isolates partitions. Nothing answers,
    /// for two separate reasons: the read reaches no further than the exact association, and the
    /// existence path resolves associations alone, so it reports less than the truth rather than
    /// claiming the partition holds no such hash.
    #[tokio::test]
    async fn test_a_sibling_context_resolves_to_nothing() -> TestResult {
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

                let result = immutable_store
                    .clone()
                    .get_metadata(repository, address)
                    .await?;
                assert_eq!(
                    result.match_made,
                    StoreMatch::MatchNone,
                    "an isolating store described an association it does not hold"
                );

                let store: Arc<dyn ImmutableStore> = immutable_store.clone();
                let resolved = query_one(&store, repository, address).await?;
                assert_eq!(
                    resolved.match_made,
                    StoreMatch::MatchNone,
                    "this store resolves associations alone, so it has nothing to report here"
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    /// The store isolates partitions, so a hash held only by another one is not its to report -
    /// where this once answered with a hash match, absence is now the whole answer.
    async fn test_query_does_not_match_across_partitions() -> TestResult {
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

                let result = query_one(
                    &(immutable_store as Arc<dyn ImmutableStore>),
                    random::<RepositoryId>(),
                    address,
                )
                .await?;
                assert_eq!(result, StoreMatchResult::default());

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
                let result = immutable_store
                    .clone()
                    .get_metadata(repository, address)
                    .await?;
                assert_eq!(result.fragment, want_fragment);
                assert_eq!(result.match_made, StoreMatch::MatchPartition);

                // Put the fragment again with a separate context in the same repo, but send no payload
                immutable_store
                    .clone()
                    .put(repository, address, fragment, None, false)
                    .await?;

                // Now if we query it we should get a full match.
                let mut want_fragment = fragment;
                want_fragment.flags = FragmentFlags::PayloadStoredDurable.bits()
                    | (fragment.flags & FragmentFlags::PayloadCompressed);
                let result = immutable_store
                    .clone()
                    .get_metadata(repository, address)
                    .await?;
                assert_eq!(result.fragment, want_fragment);
                assert_eq!(result.match_made, StoreMatch::MatchFull);

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
                    .get(repository, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
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
                        .get(repository, address)
                        .await
                        .expect_err("should have returned an error")
                        .is_address_not_found()
                );

                Ok(())
            })
            .await
    }

    /// A read names an association, and this store holds none for a sibling context. It isolates
    /// partitions, and nothing carries the level alongside a payload it serves, so a caller on the
    /// far side of a wire would read anything handed over here as an association of its own.
    #[tokio::test]
    async fn test_get_immutable_refuses_a_sibling_context() -> TestResult {
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

                assert!(
                    immutable_store
                        .clone()
                        .get(repository, address)
                        .await
                        .expect_err("an isolating store served a sibling context's payload")
                        .is_address_not_found()
                );

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
                    .get(repository, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
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
                    .get(repository, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
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
                    .get(repository, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
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

                let mut result = vec![StoreMatchResult::default(); address.len()];
                immutable_store
                    .clone()
                    .query(repository, &address, &mut result)
                    .await
                    .expect("Failed to query batch");
                assert_eq!(
                    result
                        .into_iter()
                        .map(|entry| entry.match_made)
                        .collect::<Vec<_>>(),
                    expected
                );

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

    /// Regression test for a silent push failure on newly created repositories.
    ///
    /// `branch::create` for a default branch (no commits yet) calls
    /// `store_latest(prev=0, latest=0)`, which goes through
    /// `compare_and_swap(expected=0, value=0)` and writes a `{value=0000…}` row
    /// to DynamoDB. Previously the zero-expected CAS condition was
    /// `attribute_not_exists(pk) AND attribute_not_exists(sk)` — an item-existence
    /// check that fails when the row already exists, even with a zero value. The
    /// handler for the resulting `ConditionalCheckFailedException` returned the
    /// stored zero as the "previous" value, which the caller mistook for a
    /// successful swap. The branch pointer was never advanced.
    ///
    /// The fix changes the condition to check the value attribute directly:
    /// `attribute_not_exists(#v) OR #v = :expected`, which matches both an absent
    /// row and a row whose value is zero.
    #[tokio::test]
    async fn test_compare_and_swap_zero_expected_succeeds_when_row_holds_zero_value() -> TestResult
    {
        let hash = random::<Hash>();
        let new_value = random::<Hash>();
        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Replicate what branch::create does for a default branch: CAS from zero to
                // zero, which writes {value=0000…} into DynamoDB without the caller noticing
                // the value is the same as "no entry".
                assert_eq!(
                    Hash::default(),
                    mutable_store
                        .clone()
                        .compare_and_swap(
                            repository,
                            hash,
                            Hash::default(),
                            Hash::default(),
                            KeyType::BranchLatestPointer,
                        )
                        .await?,
                    "initialisation CAS must succeed"
                );

                // The row now exists in DynamoDB with value=0000… . A zero value is treated
                // as absent by both the local and AWS stores, so load returns AddressNotFound.
                assert!(
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::BranchLatestPointer)
                        .await
                        .is_err(),
                    "a zero-valued row must look absent to load"
                );

                // Now replicate the first real push: CAS from zero to a real revision hash.
                // Before the fix this silently no-oped because the item-existence condition
                // failed and the handler returned the stored zero as the previous value,
                // making the caller think the swap had succeeded.
                assert_eq!(
                    Hash::default(),
                    mutable_store
                        .clone()
                        .compare_and_swap(
                            repository,
                            hash,
                            Hash::default(),
                            new_value,
                            KeyType::BranchLatestPointer,
                        )
                        .await?,
                    "push CAS must succeed against the zero-valued row"
                );

                // The write must have actually landed — a clone would no longer be empty.
                assert_eq!(
                    new_value,
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::BranchLatestPointer)
                        .await?,
                    "branch pointer must reflect the pushed revision"
                );

                Ok(())
            })
            .await
    }

    /// A zero-valued row must block a CAS that expects a non-zero value.
    ///
    /// The stored zero does not equal the non-zero expected hash, so the swap
    /// must not take effect. The returned previous value is zero (the actual
    /// stored value), which differs from the non-zero expected, signalling
    /// failure to the caller.
    #[tokio::test]
    async fn test_compare_and_swap_zero_value_row_blocks_nonzero_expected() -> TestResult {
        let hash = random::<Hash>();
        let new_value = random::<Hash>();
        let wrong_expected = random::<Hash>(); // non-zero, does not match stored zero
        let repository = random::<RepositoryId>();

        let (_immutable_store, mutable_store, execution) = initialize_store().await?;
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                // Write the zero-valued row (simulating branch::create for an empty branch).
                mutable_store
                    .clone()
                    .compare_and_swap(
                        repository,
                        hash,
                        Hash::default(),
                        Hash::default(),
                        KeyType::BranchLatestPointer,
                    )
                    .await?;

                // CAS with a non-zero expected must not swap — the stored value is zero.
                let previous = mutable_store
                    .clone()
                    .compare_and_swap(
                        repository,
                        hash,
                        wrong_expected,
                        new_value,
                        KeyType::BranchLatestPointer,
                    )
                    .await?;

                // previous != wrong_expected signals failure to the caller.
                assert_ne!(
                    previous, wrong_expected,
                    "previous must not equal wrong_expected — the swap must not have taken effect"
                );

                // The row must be unchanged — still holds zero, which loads as absent.
                assert!(
                    mutable_store
                        .clone()
                        .load(repository, hash, KeyType::BranchLatestPointer)
                        .await
                        .is_err(),
                    "row must still hold zero after a failed CAS"
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

    /// The contract every `ImmutableStore` owes its callers, checked against the AWS store
    /// backed by real S3 and DynamoDB. This is the authoritative conformance check for the AWS
    /// implementation: unit tests can only validate logic against the in-process model of what AWS
    /// services do, whereas this test runs the full battery against the actual services and will
    /// catch any divergence between that model and reality.
    #[tokio::test]
    async fn aws_immutable_store_satisfies_the_conformance_contract() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (s3, dynamo, _) =
                    setup(vec![FRAGMENTS_TABLE_NAME, FRAGMENT_STATE_TABLE_NAME]).await?;

                let settings = AwsImmutableStoreSettings::new(
                    S3StoreSettings::new(STORE_BUCKET_NAME.to_string()),
                    DynamoDbImmutableStoreSettings::new(
                        FRAGMENTS_TABLE_NAME.to_string(),
                        FRAGMENT_STATE_TABLE_NAME.to_string(),
                    ),
                    false,
                );
                let store = Arc::new(AwsImmutableStore::new(s3, dynamo, &settings));

                lore_storage::conformance::verify_immutable_store(
                    store,
                    lore_storage::conformance::Capabilities::new("AwsImmutableStore/integration"),
                )
                .await;

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn aws_mutable_store_satisfies_conformance_battery() -> TestResult {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (_, dynamo, _) = setup(vec![MUTABLE_STORE_TABLE_NAME]).await?;

                let settings = AwsMutableStoreSettings::new(
                    DynamoDbMutableStoreSettings::new(MUTABLE_STORE_TABLE_NAME.to_string()),
                    false,
                );

                let store: Arc<dyn MutableStore> = Arc::new(AwsMutableStore::new(
                    dynamo,
                    &settings,
                    Arc::new(LocalStore::default()),
                ));

                lore_storage::mutable_conformance::verify_mutable_store(
                    store,
                    lore_storage::mutable_conformance::Capabilities::new("AwsMutableStore")
                        .known_violations(&[
                            // DynamoDB queries are scoped to a single partition key value.
                            // Partition::default() converts to a zero Context and queries only
                            // entries stored under that specific partition — not a cross-partition
                            // scan.
                            lore_storage::mutable_conformance::Check::ListNullPartitionMatchesAll,
                        ]),
                )
                .await;

                Ok(())
            })
            .await
    }
}
