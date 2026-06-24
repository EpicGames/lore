// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::RwLock;

    use async_trait::async_trait;
    use bytes::Bytes;
    use lore_base::error::AddressNotFound;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_base::types::Partition;
    use lore_revision::fragment::generate_random;
    use lore_revision::lore::RepositoryId;
    use lore_revision::store::composite::CompositeStoreBuilder;
    use lore_storage::ImmutableStore;
    use lore_storage::KeyValueStream;
    use lore_storage::StoreError;
    use lore_storage::StoreMatch;
    use lore_storage::StoreObliterateStats;
    use lore_storage::StoreQueryResult;
    use lore_storage::local::immutable_store as immutable;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use rand::random;

    include!("helper.rs");

    #[derive(Default)]
    struct TestStore<'a> {
        succeed: bool,
        match_result: Option<StoreMatch>,
        invocations: RwLock<HashMap<&'a str, u32>>,
        compare_and_swap_result: Option<Hash>,
        get_immutable_result: Option<(Fragment, Bytes)>,
        max_query_batch: Option<usize>,
    }

    impl TestStore<'_> {
        fn succeeding() -> Self {
            Self {
                succeed: true,
                ..Default::default()
            }
        }

        fn succeeding_limited(limit: usize) -> Self {
            Self {
                succeed: true,
                max_query_batch: Some(limit),
                ..Default::default()
            }
        }

        fn failing() -> Self {
            Self::default()
        }

        fn with_mock_get_immutable(mut self, fragment: &Fragment, payload: &Bytes) -> Self {
            self.get_immutable_result = Some((*fragment, payload.clone()));
            self
        }

        fn with_mock_match(mut self, match_result: StoreMatch) -> Self {
            self.match_result = Some(match_result);
            self
        }

        fn track_invocation(&self, name: &'static str) {
            let mut invocations = self.invocations.write().unwrap();
            invocations.entry(name).and_modify(|v| *v += 1).or_insert(1);
        }
    }

    #[async_trait]
    impl lore_storage::MutableStore for TestStore<'_> {
        async fn load(
            self: Arc<Self>,
            _repository: Partition,
            _key: Hash,
            _key_type: KeyType,
        ) -> Result<Hash, StoreError> {
            self.track_invocation("load");

            if self.succeed {
                Ok(random::<Hash>())
            } else {
                Err(StoreError::from(AddressNotFound::from(Address::default())))
            }
        }

        async fn store(
            self: Arc<Self>,
            _repository: Partition,
            _key: Hash,
            _value: Hash,
            _key_type: KeyType,
        ) -> Result<(), StoreError> {
            self.track_invocation("store");

            if self.succeed {
                Ok(())
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
        }

        async fn compare_and_swap(
            self: Arc<Self>,
            _repository: Partition,
            _key: Hash,
            _expected: Hash,
            _value: Hash,
            _key_type: KeyType,
        ) -> Result<Hash, StoreError> {
            self.track_invocation("compare_and_swap");

            let value = self.compare_and_swap_result.unwrap_or(random::<Hash>());

            if self.succeed {
                Ok(value)
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
        }

        async fn list(
            self: Arc<Self>,
            _repository: Partition,
            _key_type: KeyType,
        ) -> Result<KeyValueStream, StoreError> {
            self.track_invocation("list");

            if self.succeed {
                let (stream, sender) = KeyValueStream::new();

                sender
                    .send((random::<Hash>(), random::<Hash>()))
                    .map_err(|_err| StoreError::internal("send failed"))?;
                sender
                    .send((random::<Hash>(), random::<Hash>()))
                    .map_err(|_err| StoreError::internal("send failed"))?;

                Ok(stream)
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
        }

        async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
            Ok(())
        }
    }

    #[async_trait]
    impl lore_storage::ImmutableStore for TestStore<'static> {
        async fn exist(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreMatch, StoreError> {
            self.track_invocation("exist");

            if self.succeed {
                if let Some(match_result) = self.match_result {
                    Ok(match_result)
                } else {
                    Ok(StoreMatch::MatchFull)
                }
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
        }

        async fn exist_batch(
            self: Arc<Self>,
            repository: Partition,
            addresses: &[Address],
            match_requested: StoreMatch,
        ) -> Result<Vec<StoreMatch>, StoreError> {
            let mut result = vec![];
            for address in addresses {
                result.push(
                    self.clone()
                        .exist(repository, *address, match_requested)
                        .await?,
                );
            }
            Ok(result)
        }

        async fn query(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreQueryResult, StoreError> {
            self.track_invocation("query");

            if self.succeed {
                Ok(StoreQueryResult {
                    fragment: Fragment::default(),
                    match_made: StoreMatch::MatchFull,
                })
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
        }

        async fn get(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _match_required: StoreMatch,
        ) -> Result<(Fragment, Bytes), StoreError> {
            self.track_invocation("get");

            if self.succeed {
                Ok(self.get_immutable_result.clone().unwrap())
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
        }

        async fn put(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            _fragment: Fragment,
            _payload: Option<Bytes>,
            _force: bool,
        ) -> Result<(), StoreError> {
            self.track_invocation("put");

            if self.succeed {
                Ok(())
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
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
            // Not needed for tests
            Ok(0)
        }

        async fn compact(
            self: Arc<Self>,
            _max_size: usize,
            _at: Option<usize>,
            _sync_data: bool,
            _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            // Not needed for tests
            Ok(None)
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            None
        }

        async fn compact_stop(self: Arc<Self>) {}

        fn max_query_batch(&self) -> Option<usize> {
            self.max_query_batch
        }

        async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
            Ok(())
        }

        async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_composite_store_builder() {
        let store1 = Arc::new(TestStore::failing());
        let store2 = Arc::new(TestStore::succeeding());
        let store3 = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                assert!(
                    CompositeStoreBuilder::default()
                        .with_local("failing".to_string(), store1.clone())
                        .expect("Failed add local")
                        .with_replica(
                            "successful, non-durable".to_string(),
                            store2.clone(),
                            true,
                            true
                        )
                        .with_durable("successful, durable".to_string(), store3.clone())
                        .expect("Failed add durable")
                        .build()
                        .is_ok()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_composite_store_builder_no_durable_stores() {
        let store1 = Arc::new(TestStore::failing());
        let store2 = Arc::new(TestStore::succeeding());
        let store3 = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                assert!(
                    CompositeStoreBuilder::default()
                        .with_local("failing".to_string(), store1.clone())
                        .expect("Failed add local")
                        .with_replica(
                            "successful, non-durable".to_string(),
                            store2.clone(),
                            true,
                            true
                        )
                        .with_replica(
                            "successful, durable".to_string(),
                            store3.clone(),
                            true,
                            true
                        )
                        .build()
                        .is_err()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_composite_store_builder_too_many_local_stores() {
        let store1 = Arc::new(TestStore::failing());
        let store2 = Arc::new(TestStore::succeeding());
        let store3 = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                assert!(
                    CompositeStoreBuilder::default()
                        .with_local("failing, local".to_string(), store1.clone())
                        .expect("Failed add local")
                        .with_durable("successful, durable".to_string(), store2.clone())
                        .expect("Failed add durable")
                        .with_local("successful, local".to_string(), store3.clone())
                        .is_err()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_composite_store_builder_too_many_durable_stores() {
        let store1 = Arc::new(TestStore::failing());
        let store2 = Arc::new(TestStore::succeeding());
        let store3 = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                assert!(
                    CompositeStoreBuilder::default()
                        .with_local("failing, local".to_string(), store1.clone())
                        .expect("Failed add local")
                        .with_durable("successful, durable".to_string(), store2.clone())
                        .expect("Failed add durable")
                        .with_durable("successful, durable".to_string(), store3.clone())
                        .is_err()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_non_durable_read() {
        let store1 = Arc::new(TestStore::failing());
        let store2 = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository: Partition = random::<RepositoryId>();
                let address = Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                };

                let store = CompositeStoreBuilder::default()
                    .with_local("failing, local".to_string(), store1.clone())
                    .expect("Failed add local")
                    .with_durable("successful, durable".to_string(), store2.clone())
                    .expect("Failed add durable")
                    .build()
                    .expect("Failed store build");
                let store = Arc::new(store);

                // The result is just hard coded in the TestStore impl, so we don't really care what it is,
                // just whether it was successful.
                store
                    .query(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("Store query failed");

                assert_eq!(*store1.invocations.read().unwrap().get("query").unwrap(), 1);
                assert_eq!(*store2.invocations.read().unwrap().get("query").unwrap(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn test_non_durable_read_short_circuits() {
        let store1 = Arc::new(TestStore::succeeding());
        let store2 = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = CompositeStoreBuilder::default()
                    .with_local("successful, local".to_string(), store1.clone())
                    .expect("Failed add local")
                    .with_durable("successful, durable".to_string(), store2.clone())
                    .expect("Failed add durable")
                    .build()
                    .expect("Failed store build");
                let store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let address = Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                };

                store
                    .clone()
                    .query(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("Store query failed");

                assert_eq!(*store1.invocations.read().unwrap().get("query").unwrap(), 1);

                assert!(store2.invocations.read().unwrap().get("query").is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn test_read_through_cache() {
        let mut fragment = Fragment::default();
        let payload = random::<[u8; 32]>();
        fragment.size_payload = payload.len() as u32;
        let buffer = Bytes::copy_from_slice(payload.as_slice());

        let store1 = Arc::new(TestStore::failing());
        let store2 = Arc::new(TestStore::succeeding().with_mock_get_immutable(&fragment, &buffer));

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = CompositeStoreBuilder::default()
                    .with_local("failing, local".to_string(), store1.clone())
                    .expect("Failed add local")
                    .with_durable("successful, durable".to_string(), store2.clone())
                    .expect("Failed add local")
                    .build()
                    .expect("Failed build store");
                let store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let address = Address {
                    hash: Hash::hash_buffer(&payload),
                    context: random::<Context>(),
                };

                assert_eq!(
                    fragment,
                    store
                        .get(repository, address, StoreMatch::MatchFull)
                        .await
                        .expect("Get immutable failed")
                        .0
                );

                // We should invoke the failing store first...
                assert_eq!(
                    *store1
                        .invocations
                        .read()
                        .unwrap()
                        .get("get")
                        .expect("Local get immutable not called"),
                    1
                );

                // Then the succeeding store...
                assert_eq!(
                    *store2
                        .invocations
                        .read()
                        .unwrap()
                        .get("get")
                        .expect("Durable get immutable not called"),
                    1
                );

                // Arbitrary sleep to force single threaded tokio test runtime to
                // execute the detached local put cache operation
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

                // And finally we should have invoked `put` on the failing store to cache the value.
                assert_eq!(
                    *store1
                        .invocations
                        .read()
                        .unwrap()
                        .get("put")
                        .expect("Local put immutable not invoked"),
                    1
                );

                // We shouldn't ever invoke `put` on the succeeding store.
                assert!(store2.invocations.read().unwrap().get("put").is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn test_exist_batch_local_partial() {
        let store1 = Arc::new(TestStore::succeeding().with_mock_match(StoreMatch::MatchHash));
        let store2 = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = CompositeStoreBuilder::default()
                    .with_local("successful, no match, local".to_string(), store1.clone())
                    .expect("Failed add local")
                    .with_durable(
                        "successful, full match, durable".to_string(),
                        store2.clone(),
                    )
                    .expect("Failed add durable")
                    .build()
                    .expect("Failed build store");
                let store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let addresses = [random::<Address>(), random::<Address>()];

                let result = store
                    .exist_batch(repository, &addresses, StoreMatch::MatchFull)
                    .await
                    .expect("Exist batch failed");

                assert_eq!(result.len(), addresses.len());
                assert_eq!(result[0], StoreMatch::MatchFull);
                assert_eq!(result[1], StoreMatch::MatchFull);
            })
            .await;
    }

    #[test]
    fn test_max_query_batch() {
        let store1 = Arc::new(TestStore::failing());
        let store2 = Arc::new(TestStore::succeeding_limited(100));
        let store3 = Arc::new(TestStore::succeeding_limited(500));

        let store = CompositeStoreBuilder::default()
            .with_local("local".to_string(), store1.clone())
            .expect("Failed add local")
            .with_durable("durable".to_string(), store2.clone())
            .expect("Failed add durable")
            .with_replica("replica".to_string(), store3.clone(), true, true)
            .build()
            .expect("Failed build store");

        assert!(store.max_query_batch().is_some());
        assert_eq!(store.max_query_batch().unwrap(), 100);
    }

    #[tokio::test]
    async fn match_full_query_results_are_cached_locally() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let durable_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("durable should have been created");
                let local_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("local should have been created");

                let store = CompositeStoreBuilder::default()
                    .with_cache_query_results(true)
                    .with_durable("test-durable".to_string(), durable_store.clone())
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store.clone())
                    .expect("local should have worked")
                    .build()
                    .expect("build should have worked");
                let composite_store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();

                // confirm we don't find the address via composite store
                let result = composite_store
                    .clone()
                    .query(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("Initial query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchNone));

                // write to the durable store without going through composite, so we recreate
                // the scenario where a remote store has data that our composite's local does not
                durable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put to durable failed");

                // confirm local store doesn't know about this address before going via composite
                let result = local_store
                    .clone()
                    .query(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("local confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchNone));

                // now composite query should find the address
                let result = composite_store
                    .clone()
                    .query(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("post-put query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // and the local store should have the cache because composite store will
                // populate it out of band
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                let result = local_store
                    .clone()
                    .query(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("local confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // but local 'get' still fails as it does not have the payload
                let result = local_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect_err("local get didn't fail");
                assert!(result.is_payload_not_found());
            })
            .await;
    }

    #[tokio::test]
    async fn local_metadata_only_strips_payload_on_get_cache() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let durable_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("durable should have been created");
                let local_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("local should have been created");

                let store = CompositeStoreBuilder::default()
                    .with_local_metadata_only(true)
                    .with_durable("test-durable".to_string(), durable_store.clone())
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store.clone())
                    .expect("local should have worked")
                    .build()
                    .expect("build should have worked");
                let composite_store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();

                // Write directly to durable (simulates remote data)
                durable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("Put to durable failed");

                // Get through composite — should fetch from durable
                let (got_fragment, got_payload) = composite_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("Composite get failed");
                assert_eq!(got_fragment.size_payload, fragment.size_payload);
                assert_eq!(got_payload, payload);

                // Wait for detached local cache task
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Local store should have the fragment (exist works)
                let match_result = local_store
                    .clone()
                    .exist(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("local exist failed");
                assert_eq!(match_result, StoreMatch::MatchFull);

                // But local get should fail with PayloadNotFound (no payload cached)
                let result = local_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect_err("local get should have failed — payload not cached");
                assert!(result.is_payload_not_found());
            })
            .await;
    }

    #[tokio::test]
    async fn local_metadata_only_strips_payload_on_put_cache() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let durable_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("durable should have been created");
                let local_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("local should have been created");

                let store = CompositeStoreBuilder::default()
                    .with_local_metadata_only(true)
                    .with_durable("test-durable".to_string(), durable_store.clone())
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store.clone())
                    .expect("local should have worked")
                    .build()
                    .expect("build should have worked");
                let composite_store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();

                // Put through composite — durable gets payload, local should not
                composite_store
                    .clone()
                    .put(repository, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("Composite put failed");

                // Wait for detached local cache task
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Local store should have the fragment metadata
                let match_result = local_store
                    .clone()
                    .exist(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("local exist failed");
                assert_eq!(match_result, StoreMatch::MatchFull);

                // But local get should fail — payload was stripped
                let result = local_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect_err("local get should have failed — payload not cached");
                assert!(result.is_payload_not_found());

                // Durable should have the full payload
                let (_, durable_payload) = durable_store
                    .clone()
                    .get(repository, address, StoreMatch::MatchFull)
                    .await
                    .expect("Durable get failed");
                assert_eq!(durable_payload, payload);
            })
            .await;
    }

    #[tokio::test]
    async fn put_with_do_not_replicate_skips_write_replicas() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let local_store: Arc<TestStore<'static>> =
                    Arc::new(TestStore::succeeding().with_mock_match(StoreMatch::MatchNone));
                let durable_store: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                let write_replica: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());

                let store = CompositeStoreBuilder::default()
                    .with_local("local".to_string(), local_store.clone())
                    .expect("Failed add local")
                    .with_durable("durable".to_string(), durable_store.clone())
                    .expect("Failed add durable")
                    .with_replica("replica".to_string(), write_replica.clone(), false, true)
                    .build()
                    .expect("Failed store build");
                let store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let address = Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                };
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadDoNotReplicate.into(),
                    size_payload: 128,
                    size_content: 128,
                };
                let payload = Bytes::from(vec![0u8; 128]);

                store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put failed");

                // Allow detached tasks to run
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                // Durable store should have received the put
                assert_eq!(
                    *durable_store
                        .invocations
                        .read()
                        .unwrap()
                        .get("put")
                        .expect("Durable put not called"),
                    1
                );

                // Write replica should NOT have received a put because do_not_replicate was set
                assert!(
                    write_replica
                        .invocations
                        .read()
                        .unwrap()
                        .get("put")
                        .is_none(),
                    "Write replica should not have been called when PayloadDoNotReplicate is set"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn put_without_do_not_replicate_sends_to_write_replicas() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let local_store: Arc<TestStore<'static>> =
                    Arc::new(TestStore::succeeding().with_mock_match(StoreMatch::MatchNone));
                let durable_store: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                let write_replica: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());

                let store = CompositeStoreBuilder::default()
                    .with_local("local".to_string(), local_store.clone())
                    .expect("Failed add local")
                    .with_durable("durable".to_string(), durable_store.clone())
                    .expect("Failed add durable")
                    .with_replica("replica".to_string(), write_replica.clone(), false, true)
                    .build()
                    .expect("Failed store build");
                let store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let address = Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                };
                let fragment = Fragment {
                    flags: 0,
                    size_payload: 128,
                    size_content: 128,
                };
                let payload = Bytes::from(vec![0u8; 128]);

                store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put failed");

                // Allow detached tasks to run
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                // Durable store should have received the put
                assert_eq!(
                    *durable_store
                        .invocations
                        .read()
                        .unwrap()
                        .get("put")
                        .expect("Durable put not called"),
                    1
                );

                // Write replica SHOULD have received a put
                assert_eq!(
                    *write_replica
                        .invocations
                        .read()
                        .unwrap()
                        .get("put")
                        .expect("Replica put not called"),
                    1,
                    "Write replica should have been called when PayloadDoNotReplicate is not set"
                );
            })
            .await;
    }

    mod inflight_dedup {
        use std::path::Path;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU32;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        use async_trait::async_trait;
        use bytes::Bytes;
        use lore_base::lore_spawn;
        use lore_base::runtime::LORE_CONTEXT;
        use lore_base::types::Address;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::fragment::generate_random;
        use lore_revision::lore::RepositoryId;
        use lore_revision::store::composite::CompositeStoreBuilder;
        use lore_storage::ImmutableStore;
        use lore_storage::StoreError;
        use lore_storage::StoreMatch;
        use lore_storage::StoreObliterateStats;
        use lore_storage::StoreQueryResult;
        use lore_storage::local::immutable_store as immutable;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;
        use rand::random;
        use tokio::sync::RwLock;

        use crate::tests::setup_test_execution;

        struct DelayStore {
            get_delay: Duration,
            get_result: RwLock<Result<(Fragment, Bytes), StoreError>>,
            get_count: AtomicU32,
        }

        impl DelayStore {
            fn succeeding(fragment: Fragment, payload: Bytes, delay: Duration) -> Self {
                Self {
                    get_delay: delay,
                    get_result: RwLock::new(Ok((fragment, payload))),
                    get_count: AtomicU32::new(0),
                }
            }

            fn failing(error: StoreError, delay: Duration) -> Self {
                Self {
                    get_delay: delay,
                    get_result: RwLock::new(Err(error)),
                    get_count: AtomicU32::new(0),
                }
            }

            fn get_count(&self) -> u32 {
                self.get_count.load(Ordering::SeqCst)
            }
        }

        impl std::fmt::Debug for DelayStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "DelayStore")
            }
        }

        #[async_trait]
        impl ImmutableStore for DelayStore {
            async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
                true
            }

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
                Ok(vec![StoreMatch::MatchNone; addresses.len()])
            }

            async fn query(
                self: Arc<Self>,
                _repository: Partition,
                _address: Address,
                _match_requested: StoreMatch,
            ) -> Result<StoreQueryResult, StoreError> {
                Ok(StoreQueryResult::default())
            }

            async fn get(
                self: Arc<Self>,
                _repository: Partition,
                _address: Address,
                _match_required: StoreMatch,
            ) -> Result<(Fragment, Bytes), StoreError> {
                self.get_count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.get_delay).await;
                self.get_result.read().await.clone()
            }

            async fn put(
                self: Arc<Self>,
                _repository: Partition,
                _address: Address,
                _fragment: Fragment,
                _payload: Option<Bytes>,
                _force: bool,
            ) -> Result<(), StoreError> {
                Ok(())
            }

            async fn obliterate(
                self: Arc<Self>,
                _repository: Partition,
                _address: Address,
                _stats: Arc<StoreObliterateStats>,
            ) -> Result<(), StoreError> {
                Ok(())
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

            fn max_query_batch(&self) -> Option<usize> {
                None
            }

            async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
                Ok(())
            }

            async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
                Ok(())
            }
        }

        fn create_empty_local()
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Arc<dyn ImmutableStore>> + Send>>
        {
            Box::pin(async {
                immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("should create local")
            })
        }

        #[tokio::test]
        async fn success_propagated_to_listeners() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let (fragment, address, payload) = generate_random();
                    let repository: Partition = random::<RepositoryId>();

                    let durable = Arc::new(DelayStore::succeeding(
                        fragment,
                        payload.clone(),
                        Duration::from_millis(200),
                    ));

                    let composite = Arc::new(
                        CompositeStoreBuilder::default()
                            .with_local("local".to_string(), create_empty_local().await)
                            .expect("local should work")
                            .with_durable("durable".to_string(), durable.clone())
                            .expect("durable should work")
                            .build()
                            .expect("build should work"),
                    );

                    let num_concurrent = 5;
                    let mut handles = Vec::with_capacity(num_concurrent);
                    for _ in 0..num_concurrent {
                        let store = composite.clone();
                        handles.push(lore_spawn!(async move {
                            store.get(repository, address, StoreMatch::MatchFull).await
                        }));
                    }

                    for handle in handles {
                        let (got_fragment, got_payload) =
                            handle.await.expect("task panicked").expect("get failed");
                        assert_eq!(got_fragment.size_payload, fragment.size_payload);
                        assert_eq!(got_payload, payload);
                    }

                    assert_eq!(
                        durable.get_count(),
                        1,
                        "inflight dedup should collapse concurrent gets into a single durable get"
                    );
                })
                .await;
        }

        #[tokio::test]
        async fn failure_propagated_to_listeners() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let repository: Partition = random::<RepositoryId>();
                    let address = random::<Address>();

                    let durable = Arc::new(DelayStore::failing(
                        StoreError::from(lore_storage::AddressNotFound::from(address)),
                        Duration::from_millis(200),
                    ));

                    let composite = Arc::new(
                        CompositeStoreBuilder::default()
                            .with_local("local".to_string(), create_empty_local().await)
                            .expect("local should work")
                            .with_durable("durable".to_string(), durable.clone())
                            .expect("durable should work")
                            .build()
                            .expect("build should work"),
                    );

                    let num_concurrent = 5;
                    let mut handles = Vec::with_capacity(num_concurrent);
                    for _ in 0..num_concurrent {
                        let store = composite.clone();
                        handles.push(lore_spawn!(async move {
                            store.get(repository, address, StoreMatch::MatchFull).await
                        }));
                    }

                    for handle in handles {
                        let result = handle.await.expect("task panicked");
                        assert!(result.is_err(), "get should have failed");
                        assert!(
                            result.unwrap_err().is_address_not_found(),
                            "should be AddressNotFound"
                        );
                    }

                    assert_eq!(
                        durable.get_count(),
                        1,
                        "inflight dedup should collapse concurrent gets into a single durable get even on failure"
                    );
                })
                .await;
        }
    }

    mod topology {
        use std::collections::HashSet;
        use std::error::Error;
        use std::path::Path;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        use async_trait::async_trait;
        use lore_base::runtime::LORE_CONTEXT;
        use lore_revision::cluster::peer::Locality;
        use lore_revision::cluster::peer::PeerInfo;
        use lore_revision::cluster::topology::RefreshLoopError;
        use lore_revision::cluster::topology::Topology;
        use lore_revision::store::composite::CompositeStore;
        use lore_revision::store::composite::CompositeStoreBuilder;
        use lore_revision::store::composite::ReplicationTarget;
        use lore_revision::store::composite::replica_factory::ReplicaFactory;
        use lore_revision::store::composite::replica_factory::ReplicaTargets;
        use lore_storage::StoreError;
        use lore_storage::local::immutable_store as immutable;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;
        use tokio::sync::broadcast::Receiver;
        use tokio::sync::broadcast::Sender;

        use crate::tests::setup_test_execution;

        #[derive(Debug)]
        struct DummyTopology {
            broadcaster: Sender<HashSet<PeerInfo>>,
        }

        impl Default for DummyTopology {
            fn default() -> Self {
                Self {
                    broadcaster: Sender::new(1),
                }
            }
        }

        #[async_trait]
        impl Topology for DummyTopology {
            async fn refresh_loop(self: Arc<Self>) -> Result<(), RefreshLoopError> {
                Ok(())
            }

            fn subscribe_to_peer_refreshes(self: Arc<Self>) -> Receiver<HashSet<PeerInfo>> {
                self.broadcaster.subscribe()
            }
        }

        #[derive(Debug, Default)]
        struct SuccessReplicaBuilder {}

        #[async_trait]
        impl ReplicaFactory for SuccessReplicaBuilder {
            async fn make_replica_target(
                &self,
                peer_info: &PeerInfo,
            ) -> Result<ReplicaTargets, Box<dyn Error + Send + Sync>> {
                let write_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("write store should have been created");

                let read_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("read store should have been created");

                Ok(ReplicaTargets {
                    read: Some(ReplicationTarget::new(peer_info.clone(), read_store)),
                    write: Some(ReplicationTarget::new(peer_info.clone(), write_store)),
                })
            }
        }

        #[derive(Debug)]
        struct LimitedSuccessReplicaBuilder {
            success_builder: SuccessReplicaBuilder,
            successes_left: AtomicUsize,
        }

        impl LimitedSuccessReplicaBuilder {
            fn new(limit: usize) -> Self {
                Self {
                    success_builder: SuccessReplicaBuilder::default(),
                    successes_left: limit.into(),
                }
            }
        }

        #[async_trait]
        impl ReplicaFactory for LimitedSuccessReplicaBuilder {
            async fn make_replica_target(
                &self,
                peer_info: &PeerInfo,
            ) -> Result<ReplicaTargets, Box<dyn Error + Send + Sync>> {
                let old_successes_left = self.successes_left.fetch_sub(1, Ordering::Relaxed);
                if old_successes_left > 0 {
                    self.success_builder.make_replica_target(peer_info).await
                } else {
                    Err(Box::new(StoreError::internal(
                        "Failed to create data store for repository",
                    )))
                }
            }
        }

        fn create_peer_info(name: &str) -> PeerInfo {
            PeerInfo {
                id: name.to_string(),
                address: "0.0.0.0".to_string(),
                port: 8080,
                locality: Locality::SameRegion,
                metric_id: name.to_string(),
            }
        }

        async fn create_test_store_with_replica_builder(
            builder: Arc<dyn ReplicaFactory>,
        ) -> Arc<CompositeStore> {
            let local_durable = immutable::create(
                None::<&Path>,
                immutable::ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("local should have been created");
            let store = CompositeStoreBuilder::default()
                .with_durable("test-local".to_string(), local_durable)
                .expect("local should have worked")
                .with_replica_builder(builder)
                .build()
                .expect("build should have worked");
            Arc::new(store)
        }

        async fn create_test_store() -> Arc<CompositeStore> {
            create_test_store_with_replica_builder(Arc::new(SuccessReplicaBuilder::default())).await
        }

        fn replica_targets_contains_peer(targets: &[ReplicationTarget], info: &PeerInfo) -> bool {
            targets.iter().any(|target| {
                target
                    .peer_info()
                    .as_ref()
                    .is_some_and(|target_info| target_info == info)
            })
        }

        #[tokio::test]
        async fn can_remove_all_our_peers() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let peer_1 = create_peer_info("peer-1");

                    let store = create_test_store().await;
                    // first time we had peers
                    {
                        let summary = store
                            .topology_peers_refreshed(HashSet::from([peer_1.clone()]))
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(summary.detected_new_peers, HashSet::from([peer_1.clone()]));
                        assert_eq!(summary.lost_peers.len(), 0);
                        assert_eq!(store.clone_write_replicas().await.len(), 1);
                        assert_eq!(store.clone_read_replicas().await.len(), 1);
                        assert_eq!(summary.num_new_peers_errors, 0);
                    }

                    // upon change, we have nothing
                    {
                        let summary = store
                            .topology_peers_refreshed(HashSet::new())
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(summary.lost_peers, HashSet::from([peer_1.clone()]));
                        assert_eq!(summary.detected_new_peers.len(), 0);
                        assert_eq!(store.clone_write_replicas().await.len(), 0);
                        assert_eq!(store.clone_read_replicas().await.len(), 0);
                        assert_eq!(summary.num_new_peers_errors, 0);
                    }
                })
                .await;
        }

        #[tokio::test]
        async fn can_incrementally_add_peers() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let peer_1 = create_peer_info("peer-1");
                    let peer_2 = create_peer_info("peer-2");
                    let peer_3 = create_peer_info("peer-3");

                    let store = create_test_store().await;
                    {
                        // first time we had peers
                        let summary = store
                            .topology_peers_refreshed(HashSet::from([peer_1.clone()]))
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(summary.detected_new_peers, HashSet::from([peer_1.clone()]));
                        assert_eq!(summary.num_new_peers_errors, 0);
                    }

                    {
                        // upon refresh, we have more peers
                        let summary = store
                            .topology_peers_refreshed(HashSet::from([
                                peer_1.clone(),
                                peer_2.clone(),
                                peer_3.clone(),
                            ]))
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(summary.lost_peers.len(), 0);
                        assert_eq!(
                            summary.detected_new_peers,
                            HashSet::from([peer_2.clone(), peer_3.clone()])
                        );
                        assert_eq!(summary.num_new_peers_errors, 0);

                        let write_replicas = store.clone_write_replicas().await;
                        assert_eq!(write_replicas.len(), 3);
                        assert!(replica_targets_contains_peer(&write_replicas, &peer_1));
                        assert!(replica_targets_contains_peer(&write_replicas, &peer_2));
                        assert!(replica_targets_contains_peer(&write_replicas, &peer_3));

                        let read_replicas = store.clone_read_replicas().await;
                        assert_eq!(read_replicas.len(), 3);
                        assert!(replica_targets_contains_peer(&read_replicas, &peer_1));
                        assert!(replica_targets_contains_peer(&read_replicas, &peer_2));
                        assert!(replica_targets_contains_peer(&read_replicas, &peer_3));
                    }
                })
                .await;
        }

        #[tokio::test]
        async fn can_partially_remove_peers() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let peer_1 = create_peer_info("peer-1");
                    let peer_2 = create_peer_info("peer-2");
                    let peer_3 = create_peer_info("peer-3");

                    let store = create_test_store().await;
                    {
                        // first time we had many
                        let summary = store
                            .topology_peers_refreshed(HashSet::from([
                                peer_1.clone(),
                                peer_2.clone(),
                                peer_3.clone(),
                            ]))
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(
                            summary.detected_new_peers,
                            HashSet::from([peer_1.clone(), peer_2.clone(), peer_3.clone()])
                        );
                        assert_eq!(summary.lost_peers.len(), 0);
                    }

                    // upon refresh, we have fewer peers
                    {
                        let summary = store
                            .topology_peers_refreshed(HashSet::from([
                                peer_1.clone(),
                                peer_3.clone(),
                            ]))
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(summary.lost_peers, HashSet::from([peer_2.clone()]));
                        assert_eq!(summary.detected_new_peers.len(), 0);

                        let write_replicas = store.clone_write_replicas().await;
                        assert_eq!(write_replicas.len(), 2);
                        assert!(replica_targets_contains_peer(&write_replicas, &peer_1));
                        assert!(replica_targets_contains_peer(&write_replicas, &peer_3));

                        let read_replicas = store.clone_read_replicas().await;
                        assert_eq!(read_replicas.len(), 2);
                        assert!(replica_targets_contains_peer(&read_replicas, &peer_1));
                        assert!(replica_targets_contains_peer(&read_replicas, &peer_3));
                    }
                })
                .await;
        }

        #[tokio::test]
        async fn can_do_noop_updates() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let peer_1 = create_peer_info("peer-1");

                    let get_peers_result = HashSet::from([peer_1.clone()]);

                    let store = create_test_store().await;
                    {
                        // initial state
                        let summary = store
                            .topology_peers_refreshed(get_peers_result.clone())
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(summary.detected_new_peers, HashSet::from([peer_1.clone()]));
                        assert_eq!(store.clone_write_replicas().await.len(), 1);
                        assert_eq!(store.clone_read_replicas().await.len(), 1);
                    }

                    {
                        // 2nd update is the same
                        let summary = store
                            .topology_peers_refreshed(get_peers_result)
                            .await
                            .expect("refresh should have worked");
                        assert_eq!(summary.lost_peers.len(), 0);
                        assert_eq!(summary.detected_new_peers.len(), 0);
                        assert_eq!(store.clone_write_replicas().await.len(), 1);
                        assert_eq!(store.clone_read_replicas().await.len(), 1);
                    }
                })
                .await;
        }

        #[tokio::test]
        async fn can_update_through_subscription() {
            let peer_1 = create_peer_info("peer-1");
            let peer_2 = create_peer_info("peer-2");

            let topology = Arc::new(DummyTopology::default());

            let store = create_test_store().await;

            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), {
                    let topology = topology.clone();
                    let store = store.clone();
                    async move {
                        store.set_topology_subscription(topology).await;
                    }
                })
                .await;

            // send an initial update with 1 peer from topology
            topology
                .broadcaster
                .send(HashSet::from([peer_1.clone()]))
                .expect("broadcast should have worked");
            // yield for a safe amount of time for it to be processed
            tokio::time::sleep(Duration::from_millis(100)).await;

            // upon 2nd manual update we have different peers
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let summary = store
                        .topology_peers_refreshed(HashSet::from([peer_2.clone()]))
                        .await
                        .expect("refresh should have worked");

                    // peer 1 was previously added from topology event
                    assert_eq!(summary.lost_peers, HashSet::from([peer_1.clone()]));
                    assert_eq!(summary.detected_new_peers, HashSet::from([peer_2.clone()]));
                })
                .await;
        }

        #[tokio::test]
        async fn errors_with_peer_creation_are_swallowed() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let peer_1 = create_peer_info("peer-1");
                    let peer_2 = create_peer_info("peer-2");
                    let peer_3 = create_peer_info("peer-3");

                    let store = create_test_store_with_replica_builder(Arc::new(
                        LimitedSuccessReplicaBuilder::new(2),
                    ))
                    .await;
                    let summary = store
                        .topology_peers_refreshed(HashSet::from([
                            peer_1.clone(),
                            peer_2.clone(),
                            peer_3.clone(),
                        ]))
                        .await
                        .expect("refresh should have worked");
                    // 3 peers were new
                    assert_eq!(summary.detected_new_peers.len(), 3);
                    assert_eq!(summary.num_new_peers_errors, 1);

                    // but only 2 replicas successfully (both read and write)
                    let write_replicas = store.clone_write_replicas().await;
                    assert_eq!(write_replicas.len(), 2);
                    let read_replicas = store.clone_read_replicas().await;
                    assert_eq!(read_replicas.len(), 2);
                })
                .await;
        }

        #[tokio::test]
        async fn read_replica_fan_out_returns_results() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    use lore_base::types::Partition;
                    use lore_revision::fragment::generate_random;
                    use lore_revision::lore::RepositoryId;
                    use lore_storage::ImmutableStore;
                    use lore_storage::StoreMatch;

                    let (fragment, address, payload) = generate_random();
                    let repository: Partition = rand::random::<RepositoryId>();

                    // read replica store has the data
                    let read_store = immutable::create(
                        None::<&Path>,
                        immutable::ImmutableStoreCreateOptions::none(),
                        false,
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("should create");
                    read_store
                        .clone()
                        .put(repository, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("put should work");

                    // durable store is empty
                    let durable = immutable::create(
                        None::<&Path>,
                        immutable::ImmutableStoreCreateOptions::none(),
                        false,
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("should create");

                    let composite = Arc::new(
                        CompositeStoreBuilder::default()
                            .with_durable("durable".to_string(), durable)
                            .expect("durable should work")
                            .with_replica("read-replica".to_string(), read_store, true, false)
                            .build()
                            .expect("build should work"),
                    );

                    let match_result = composite
                        .clone()
                        .exist(repository, address, StoreMatch::MatchFull)
                        .await
                        .expect("exist should work");
                    assert_eq!(match_result, StoreMatch::MatchFull);

                    let (got_fragment, got_payload) = composite
                        .clone()
                        .get(repository, address, StoreMatch::MatchFull)
                        .await
                        .expect("get should work");
                    assert_eq!(got_fragment, fragment);
                    assert_eq!(got_payload, payload);
                })
                .await;
        }

        #[tokio::test]
        async fn failed_read_replica_does_not_block_read() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    use lore_base::types::Partition;
                    use lore_revision::fragment::generate_random;
                    use lore_revision::lore::RepositoryId;
                    use lore_storage::ImmutableStore;
                    use lore_storage::StoreMatch;

                    let (fragment, address, payload) = generate_random();
                    let repository: Partition = rand::random::<RepositoryId>();

                    // durable has the data
                    let durable = immutable::create(
                        None::<&Path>,
                        immutable::ImmutableStoreCreateOptions::none(),
                        false,
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("should create");
                    durable
                        .clone()
                        .put(repository, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("put should work");

                    // read replica is empty (will not find the data)
                    let empty_replica = immutable::create(
                        None::<&Path>,
                        immutable::ImmutableStoreCreateOptions::none(),
                        false,
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("should create");

                    let composite = Arc::new(
                        CompositeStoreBuilder::default()
                            .with_durable("durable".to_string(), durable)
                            .expect("durable should work")
                            .with_replica("empty-replica".to_string(), empty_replica, true, false)
                            .build()
                            .expect("build should work"),
                    );

                    let (got_fragment, got_payload) = composite
                        .clone()
                        .get(repository, address, StoreMatch::MatchFull)
                        .await
                        .expect("get should succeed via durable despite empty replica");
                    assert_eq!(got_fragment, fragment);
                    assert_eq!(got_payload, payload);
                })
                .await;
        }
    }
}
