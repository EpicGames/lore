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
    use lore_storage::StoreGetData;
    use lore_storage::StoreMatch;
    use lore_storage::StoreMatchResult;
    use lore_storage::StoreObliterateStats;
    use lore_storage::local::immutable_store as immutable;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use rand::random;

    include!("helper.rs");

    #[derive(Default)]
    struct TestStore<'a> {
        succeed: bool,
        match_result: Option<StoreMatch>,
        /// The context a match reports having found the content under, where it is not the one
        /// asked about.
        match_context: Option<Context>,
        /// Whether a match reports the durable store as holding the association.
        match_durable: bool,
        invocations: RwLock<HashMap<&'a str, u32>>,
        compare_and_swap_result: Option<Hash>,
        get_immutable_result: Option<StoreGetData>,
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
            self.get_immutable_result = Some(StoreGetData {
                fragment: *fragment,
                match_made: StoreMatch::MatchFull,
                partition: Partition::default(),
                payload: Some(payload.clone()),
            });
            self
        }

        fn with_mock_match(mut self, match_result: StoreMatch) -> Self {
            self.match_result = Some(match_result);
            self
        }

        /// Report a match found under `context` and already held by the durable store, which is what
        /// a put needs before it can duplicate an association instead of storing the payload.
        fn with_mock_durable_match(mut self, context: Context) -> Self {
            self.match_context = Some(context);
            self.match_durable = true;
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
        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.track_invocation("get_metadata");
            let _ = address;

            if self.succeed {
                Ok(StoreGetData::metadata(
                    Fragment::default(),
                    StoreMatch::MatchFull,
                    partition,
                ))
            } else {
                Err(StoreError::internal("Mock store failure"))
            }
        }

        async fn query(
            self: Arc<Self>,
            _repository: Partition,
            addresses: &[Address],
            results: &mut [lore_storage::StoreMatchResult],
        ) -> Result<(), StoreError> {
            self.track_invocation("resolve");

            if !self.succeed {
                return Err(StoreError::internal("Mock store failure"));
            }

            let match_made = self.match_result.unwrap_or(StoreMatch::MatchFull);
            for (result, address) in results.iter_mut().zip(addresses.iter()) {
                *result = lore_storage::StoreMatchResult {
                    match_made,
                    partition: _repository,
                    context: self.match_context.unwrap_or(address.context),
                    stored_local: false,
                    stored_durable: self.match_durable,
                };
            }

            Ok(())
        }

        async fn get(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
        ) -> Result<StoreGetData, StoreError> {
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

        async fn copy(
            self: Arc<Self>,
            _source_partition: Partition,
            _source_address: Address,
            _destination_partition: Partition,
            _destination_context: Context,
            _durable: bool,
        ) -> Result<(), StoreError> {
            self.track_invocation("copy");

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
                lore_storage::immutable_store::query_one(
                    &(store as Arc<dyn ImmutableStore>),
                    repository,
                    address,
                )
                .await
                .expect("Store resolve failed");

                assert_eq!(
                    *store1.invocations.read().unwrap().get("resolve").unwrap(),
                    1
                );
                assert_eq!(
                    *store2.invocations.read().unwrap().get("resolve").unwrap(),
                    1
                );
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

                lore_storage::immutable_store::query_one(
                    &(store.clone() as Arc<dyn ImmutableStore>),
                    repository,
                    address,
                )
                .await
                .expect("Store resolve failed");

                assert_eq!(
                    *store1.invocations.read().unwrap().get("resolve").unwrap(),
                    1
                );

                assert!(store2.invocations.read().unwrap().get("resolve").is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn get_metadata_local_hit_short_circuits() {
        let store1 = Arc::new(TestStore::succeeding());
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
                    .with_local("successful, local".to_string(), store1.clone())
                    .expect("Failed add local")
                    .with_durable("successful, durable".to_string(), store2.clone())
                    .expect("Failed add durable")
                    .build()
                    .expect("Failed store build");
                let store = Arc::new(store);

                store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("get_metadata should succeed");

                assert_eq!(
                    *store1
                        .invocations
                        .read()
                        .unwrap()
                        .get("get_metadata")
                        .unwrap(),
                    1
                );
                // durable should not be consulted when local hits
                assert!(
                    store2
                        .invocations
                        .read()
                        .unwrap()
                        .get("get_metadata")
                        .is_none()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn get_metadata_falls_back_to_durable_when_local_misses() {
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

                store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("get_metadata should succeed via durable");

                assert_eq!(
                    *store1
                        .invocations
                        .read()
                        .unwrap()
                        .get("get_metadata")
                        .unwrap(),
                    1
                );
                assert_eq!(
                    *store2
                        .invocations
                        .read()
                        .unwrap()
                        .get("get_metadata")
                        .unwrap(),
                    1
                );
            })
            .await;
    }

    #[tokio::test]
    async fn get_metadata_consults_replica_when_local_misses() {
        let local = Arc::new(TestStore::failing());
        let durable = Arc::new(TestStore::failing());
        let replica = Arc::new(TestStore::succeeding());

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let repository: Partition = random::<RepositoryId>();
                let address = Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                };

                let store = CompositeStoreBuilder::default()
                    .with_local("failing, local".to_string(), local.clone())
                    .expect("Failed add local")
                    .with_durable("failing, durable".to_string(), durable.clone())
                    .expect("Failed add durable")
                    .with_replica(
                        "successful, replica".to_string(),
                        replica.clone(),
                        true,
                        false,
                    )
                    .build()
                    .expect("Failed store build");
                let store = Arc::new(store);

                store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("get_metadata should succeed via replica");

                assert_eq!(
                    *local
                        .invocations
                        .read()
                        .unwrap()
                        .get("get_metadata")
                        .unwrap(),
                    1
                );
                assert_eq!(
                    *replica
                        .invocations
                        .read()
                        .unwrap()
                        .get("get_metadata")
                        .unwrap(),
                    1
                );
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
                        .get(repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
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

                let mut result = [lore_storage::StoreMatchResult::default(); 2];
                store
                    .query(repository, &addresses, &mut result)
                    .await
                    .expect("Resolve failed");

                assert_eq!(result[0].match_made, StoreMatch::MatchFull);
                assert_eq!(result[1].match_made, StoreMatch::MatchFull);
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

    /// The answer the copy path is built on: a cache holding content under a partition the caller
    /// also reaches, surviving the fan-out to a caller that can act on it.
    ///
    /// The local store keeps several partitions and does not isolate them, so it is the only thing
    /// in the stack that can establish a hash match. The durable store below has nothing, and the
    /// merge must keep the weaker-but-useful answer rather than let the miss overwrite it - along
    /// with the partition, which is what a caller names as the source of a copy.
    #[tokio::test]
    async fn a_foreign_partition_survives_the_merge_to_the_caller() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let durable_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings {
                        implicit_durable_stored: true,
                        ..Default::default()
                    },
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

                let store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("test-durable".to_string(), durable_store.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local_store.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked"),
                );

                let held_under: Partition = random::<RepositoryId>();
                let asked_under: Partition = random::<RepositoryId>();

                // Only the local store has this one, and only under another partition.
                let (elsewhere_fragment, elsewhere, elsewhere_payload) = generate_random();
                local_store
                    .put(
                        held_under,
                        elsewhere,
                        elsewhere_fragment,
                        Some(elsewhere_payload),
                        false,
                    )
                    .await
                    .expect("put under the holding partition failed");

                // Only the durable store has this one, under the partition being asked about.
                let (here_fragment, here, here_payload) = generate_random();
                durable_store
                    .put(asked_under, here, here_fragment, Some(here_payload), false)
                    .await
                    .expect("put under the asked partition failed");

                let addresses = [elsewhere, here];
                let mut resolved = [lore_storage::StoreMatchResult::default(); 2];
                store
                    .query(asked_under, &addresses, &mut resolved)
                    .await
                    .expect("query failed");

                assert_eq!(resolved[0].match_made, StoreMatch::MatchHash);
                assert_eq!(
                    resolved[0].partition, held_under,
                    "the merged answer must name where the content actually is, not where it was \
                     asked for"
                );
                assert!(resolved[0].stored_local);

                // The two answers came from different stores in one merge, so each partition has to
                // travel with the level that carried it - swapping them would point a copy at a
                // store that never saw the content.
                assert_eq!(resolved[1].match_made, StoreMatch::MatchFull);
                assert_eq!(resolved[1].partition, asked_under);
            })
            .await;
    }

    #[tokio::test]
    async fn resolve_merges_answers_from_every_store() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let durable_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings {
                        implicit_durable_stored: true,
                        ..Default::default()
                    },
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

                let store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("test-durable".to_string(), durable_store.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local_store.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked"),
                );

                let repository: Partition = random::<RepositoryId>();
                let (local_fragment, local_address, local_payload) = generate_random();
                let (durable_fragment, durable_address, durable_payload) = generate_random();
                let (_, missing_address, _) = generate_random();

                local_store
                    .put(
                        repository,
                        local_address,
                        local_fragment,
                        Some(local_payload),
                        false,
                    )
                    .await
                    .expect("put to local failed");
                durable_store
                    .put(
                        repository,
                        durable_address,
                        durable_fragment,
                        Some(durable_payload),
                        false,
                    )
                    .await
                    .expect("put to durable failed");

                let addresses = [local_address, missing_address, durable_address];
                let mut results = [lore_storage::StoreMatchResult::default(); 3];
                store
                    .query(repository, &addresses, &mut results)
                    .await
                    .expect("resolve failed");

                assert_eq!(results[0].match_made, StoreMatch::MatchFull);
                assert!(results[0].stored_local);
                assert!(!results[0].stored_durable);

                assert_eq!(results[1].match_made, StoreMatch::MatchNone);
                assert!(!results[1].stored_local);
                assert!(!results[1].stored_durable);

                // Answered by the durable store, which is a local store in this topology and so
                // reports content on its own disk. That is not our local store, and the composite
                // is the only thing that knows the difference.
                assert_eq!(results[2].match_made, StoreMatch::MatchFull);
                assert!(!results[2].stored_local);
                assert!(results[2].stored_durable);
            })
            .await;
    }

    #[tokio::test]
    async fn satisfies_the_immutable_store_contract() {
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
                    .with_durable("test-durable".to_string(), durable_store)
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store)
                    .expect("local should have worked")
                    .build()
                    .expect("build should have worked");

                lore_storage::conformance::verify_immutable_store(
                    Arc::new(store),
                    lore_storage::conformance::Capabilities::new("CompositeStore"),
                )
                .await;
            })
            .await;
    }

    #[tokio::test]
    async fn durable_store_get_metadata_results_are_cached_locally() {
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
                    .with_cache_metadata(true, None)
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
                    .get_metadata(repository, address)
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
                    .get_metadata(repository, address)
                    .await
                    .expect("local confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchNone));

                // now composite get_metadata should find the address
                let result = composite_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("post-put query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // and the local store should have the cache because composite store will
                // populate it out of band
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                let result = local_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("local confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // but local 'get' still fails as it does not have the payload
                let result = local_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect_err("local get didn't fail");
                assert!(result.is_payload_not_found());
            })
            .await;
    }

    #[tokio::test]
    async fn replicas_get_metadata_results_not_cached_locally() {
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
                let replica_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("replica should have been created");

                let store = CompositeStoreBuilder::default()
                    .with_cache_metadata(true, None)
                    .with_durable("test-durable".to_string(), durable_store.clone())
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store.clone())
                    .expect("local should have worked")
                    .with_replica(
                        "test-replica".to_string(),
                        replica_store.clone(),
                        true,
                        true,
                    )
                    .build()
                    .expect("build should have worked");
                let composite_store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();

                // confirm we don't find the address via composite store
                let result = composite_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("Initial query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchNone));

                // write to the replica store without going through composite, so we recreate
                // the scenario where a remote store has data that our composite's local does not
                replica_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put to replica failed");

                // confirm local store doesn't know about this address before going via composite
                let result = local_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("local confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchNone));

                // now composite get_metadata should find the address
                let result = composite_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("post-put query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // and the local store won't have the cache because composite store won't
                // cache read replicas
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                let result = local_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("replica confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchNone));
            })
            .await;
    }

    #[tokio::test]
    async fn durable_store_get_results_are_cached_locally() {
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
                    .with_cache_metadata(true, None)
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
                    .get_metadata(repository, address)
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
                    .get(repository, address)
                    .await
                    .expect_err("get success");
                assert!(result.is_address_not_found());

                // now composite get should find the address
                let result = composite_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect("post-put query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // and the local store should have the cache because composite store will
                // populate it out of band
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                let result = local_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect("local confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));
            })
            .await;
    }

    #[tokio::test]
    async fn replicas_get_results_not_cached_locally() {
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
                let replica_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("replica should have been created");

                let store = CompositeStoreBuilder::default()
                    .with_cache_metadata(true, None)
                    .with_durable("test-durable".to_string(), durable_store.clone())
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store.clone())
                    .expect("local should have worked")
                    .with_replica(
                        "test-replica".to_string(),
                        replica_store.clone(),
                        true,
                        true,
                    )
                    .build()
                    .expect("build should have worked");
                let composite_store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();

                // confirm we don't find the address via composite store
                let result = composite_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect_err("get success");
                assert!(result.is_address_not_found());

                // write to the replica store without going through composite, so we recreate
                // the scenario where a remote store has data that our composite's local does not
                replica_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put to replica failed");

                // confirm local store doesn't know about this address before going via composite
                let result = local_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect_err("get success");
                assert!(result.is_address_not_found());

                // now composite get should find the address
                let result = composite_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("post-put query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // and the local store won't have the cache because composite store won't
                // cache read replicas
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                let result = local_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect_err("get success");
                assert!(result.is_address_not_found());
            })
            .await;
    }

    #[tokio::test]
    async fn durable_query_match_full_results_are_cached_locally() {
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
                    .with_cache_metadata(true, None)
                    .with_durable("test-durable".to_string(), durable_store.clone())
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store.clone())
                    .expect("local should have worked")
                    .build()
                    .expect("build should have worked");
                let composite_store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();

                // write to durable directly so the local store has no entry
                durable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put to durable failed");

                // query via composite: durable resolves MatchFull with a non-zero partition, so
                // a background get_metadata is spawned to populate the local store
                let mut results = [StoreMatchResult::default()];
                composite_store
                    .clone()
                    .query(repository, &[address], &mut results)
                    .await
                    .expect("query failed");
                assert!(matches!(results[0].match_made, StoreMatch::MatchFull));
                assert!(!results[0].partition.is_zero());

                // wait for the background cache write-back
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // local store should now have the metadata cached
                let result = local_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("local get_metadata failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));
            })
            .await;
    }

    #[tokio::test]
    async fn replica_query_match_full_results_not_cached_locally() {
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
                let replica_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("replica should have been created");

                let store = CompositeStoreBuilder::default()
                    .with_cache_metadata(true, None)
                    .with_durable("test-durable".to_string(), durable_store.clone())
                    .expect("durable should have worked")
                    .with_local("test-local".to_string(), local_store.clone())
                    .expect("local should have worked")
                    .with_replica(
                        "test-replica".to_string(),
                        replica_store.clone(),
                        true,
                        true,
                    )
                    .build()
                    .expect("build should have worked");
                let composite_store = Arc::new(store);

                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();

                // write to replica directly so the local store has no entry
                replica_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put to durable failed");

                // query via composite: replica resolves MatchFull with a non-zero partition.
                // because the results came from the replica no caching takes place
                let mut results = [StoreMatchResult::default()];
                composite_store
                    .clone()
                    .query(repository, &[address], &mut results)
                    .await
                    .expect("query failed");
                assert!(matches!(results[0].match_made, StoreMatch::MatchFull));
                assert!(!results[0].partition.is_zero());

                // wait for an erroneous background cache write-back
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // local store should not have the metadata cached
                let result = local_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("local get_metadata failed");
                assert!(matches!(result.match_made, StoreMatch::MatchNone));
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
                    .get(repository, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
                    .expect("Composite get failed");
                assert_eq!(got_fragment.size_payload, fragment.size_payload);
                assert_eq!(got_payload, payload);

                // Wait for detached local cache task
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Local store should have the fragment (resolve works)
                let match_result =
                    lore_storage::immutable_store::query_one(&local_store, repository, address)
                        .await
                        .expect("local resolve failed");
                assert_eq!(match_result.match_made, StoreMatch::MatchFull);

                // But local get should fail with PayloadNotFound (no payload cached)
                let result = local_store
                    .clone()
                    .get(repository, address)
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
                let match_result =
                    lore_storage::immutable_store::query_one(&local_store, repository, address)
                        .await
                        .expect("local resolve failed");
                assert_eq!(match_result.match_made, StoreMatch::MatchFull);

                // But local get should fail — payload was stripped
                let result = local_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect_err("local get should have failed — payload not cached");
                assert!(result.is_payload_not_found());

                // Durable should have the full payload
                let (_, durable_payload) = durable_store
                    .clone()
                    .get(repository, address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
                    .expect("Durable get failed");
                assert_eq!(durable_payload, payload);
            })
            .await;
    }

    /// A put whose content the durable store already holds, under an association the local store
    /// can name, is a write the durable store can answer with a copy. The caller supplying the
    /// payload is what makes naming that source its own to use, since ingress verified the payload
    /// against this address.
    mod put_duplicates_a_durable_association {
        use super::*;

        struct Fixture {
            store: Arc<lore_revision::store::composite::CompositeStore>,
            local: Arc<TestStore<'static>>,
            durable: Arc<TestStore<'static>>,
            replica: Arc<TestStore<'static>>,
            partition: Partition,
            address: Address,
        }

        /// A composite whose local store answers `match` for the address, and a durable store that
        /// records whichever verb it is asked for.
        fn fixture(local: TestStore<'static>) -> Fixture {
            fixture_with_replica(local, false)
        }

        fn fixture_with_replica(local: TestStore<'static>, write_replica: bool) -> Fixture {
            let local_store: Arc<TestStore<'static>> = Arc::new(local);
            let durable: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
            let replica: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
            let store = CompositeStoreBuilder::default()
                .with_local("local".to_string(), local_store.clone())
                .expect("Failed add local")
                .with_durable("durable".to_string(), durable.clone())
                .expect("Failed add durable")
                .with_replica("replica".to_string(), replica.clone(), false, write_replica)
                .build()
                .expect("Failed store build");
            Fixture {
                store: Arc::new(store),
                local: local_store,
                durable,
                replica,
                partition: random::<RepositoryId>(),
                address: Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                },
            }
        }

        async fn put(fixture: &Fixture, payload: Option<Bytes>) {
            fixture
                .store
                .clone()
                .put(
                    fixture.partition,
                    fixture.address,
                    Fragment {
                        flags: 0,
                        size_payload: 128,
                        size_content: 128,
                    },
                    payload,
                    false,
                )
                .await
                .expect("Put failed");
            // The local mirror of a copy is detached.
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        fn count(store: &Arc<TestStore<'static>>, verb: &str) -> u32 {
            store
                .invocations
                .read()
                .unwrap()
                .get(verb)
                .copied()
                .unwrap_or_default()
        }

        #[tokio::test]
        async fn a_durable_partition_match_is_copied_not_stored() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(
                        count(&fixture.durable, "copy"),
                        1,
                        "the durable store holds these bytes already, so it must be asked for the \
                         association rather than the payload"
                    );
                    assert_eq!(
                        count(&fixture.durable, "put"),
                        0,
                        "no payload should reach the durable store"
                    );
                    assert_eq!(
                        count(&fixture.local, "copy"),
                        1,
                        "the local cache must be given the association too, or the next read of the \
                         target address leaves the process for bytes it already holds"
                    );
                })
                .await;
        }

        /// The same for a hash held in another partition, which only a local store that reads across
        /// them reports — the ownership the payload proves is what makes it usable as a source.
        #[tokio::test]
        async fn a_durable_hash_match_is_copied_not_stored() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchHash)
                            .with_mock_durable_match(random::<Context>()),
                    );

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(count(&fixture.durable, "copy"), 1);
                    assert_eq!(count(&fixture.durable, "put"), 0);
                })
                .await;
        }

        /// Without a payload the caller has proved nothing, so it gets no association it did not
        /// already have.
        #[tokio::test]
        async fn a_match_without_a_payload_is_stored_not_copied() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );

                    put(&fixture, None).await;

                    assert_eq!(count(&fixture.durable, "copy"), 0);
                    assert_eq!(count(&fixture.durable, "put"), 1);
                })
                .await;
        }

        /// A match the local cache holds but the durable store does not names a source the copy
        /// would not find, so the payload is stored as before.
        #[tokio::test]
        async fn a_match_the_durable_store_lacks_is_stored_not_copied() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(
                        TestStore::succeeding().with_mock_match(StoreMatch::MatchPartition),
                    );

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(count(&fixture.durable, "copy"), 0);
                    assert_eq!(count(&fixture.durable, "put"), 1);
                })
                .await;
        }

        /// Nothing matched, so there is no association to duplicate.
        #[tokio::test]
        async fn a_miss_is_stored_not_copied() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture =
                        fixture(TestStore::succeeding().with_mock_match(StoreMatch::MatchNone));

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(count(&fixture.durable, "copy"), 0);
                    assert_eq!(count(&fixture.durable, "put"), 1);
                })
                .await;
        }

        /// A put's contract includes replicating it, and satisfying the durable leg with a copy must
        /// not drop that. The replica is issued the same copy rather than the payload — one that
        /// cannot answer it holds no association, which replicas being an acceleration makes
        /// acceptable, but it must be asked.
        #[tokio::test]
        async fn a_copied_put_still_reaches_the_write_replicas() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture_with_replica(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                        true,
                    );

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(count(&fixture.durable, "copy"), 1);
                    assert_eq!(count(&fixture.durable, "put"), 0);
                    assert_eq!(
                        count(&fixture.replica, "copy"),
                        1,
                        "the write replica must be asked to duplicate the association too"
                    );
                    assert_eq!(
                        count(&fixture.replica, "put"),
                        0,
                        "no payload should reach a replica either"
                    );
                })
                .await;
        }

        /// A replica that refuses the copy leaves the put succeeding: the durable store holds the
        /// association, which is what the put was for.
        #[tokio::test]
        async fn a_replica_refusing_the_copy_does_not_fail_the_put() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let local: Arc<TestStore<'static>> = Arc::new(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );
                    let durable: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                    let replica: Arc<TestStore<'static>> = Arc::new(TestStore::failing());
                    let store = CompositeStoreBuilder::default()
                        .with_local("local".to_string(), local)
                        .expect("Failed add local")
                        .with_durable("durable".to_string(), durable.clone())
                        .expect("Failed add durable")
                        .with_replica("replica".to_string(), replica.clone(), false, true)
                        .build()
                        .expect("Failed store build");

                    Arc::new(store)
                        .put(
                            random::<RepositoryId>(),
                            Address {
                                hash: random::<Hash>(),
                                context: random::<Context>(),
                            },
                            Fragment {
                                flags: 0,
                                size_payload: 128,
                                size_content: 128,
                            },
                            Some(Bytes::from(vec![0u8; 128])),
                            false,
                        )
                        .await
                        .expect("a replica that cannot duplicate must not fail the put");

                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    assert_eq!(count(&durable, "copy"), 1);
                    assert_eq!(count(&replica, "copy"), 1);
                })
                .await;
        }

        /// And a replica that was never configured is not a reason to hold the payload.
        #[tokio::test]
        async fn a_copied_put_without_replicas_sends_nothing_further() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(count(&fixture.durable, "copy"), 1);
                    assert_eq!(count(&fixture.replica, "put"), 0);
                })
                .await;
        }

        /// The consequence of releasing the payload first: a refused copy has nothing to fall back
        /// on, so the put fails and nothing is stored. Recovery is the caller's retry.
        #[tokio::test]
        async fn a_refused_copy_fails_the_put_without_storing_the_payload() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let local: Arc<TestStore<'static>> = Arc::new(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );
                    let durable: Arc<TestStore<'static>> = Arc::new(TestStore::failing());
                    let store = CompositeStoreBuilder::default()
                        .with_local("local".to_string(), local)
                        .expect("Failed add local")
                        .with_durable("durable".to_string(), durable.clone())
                        .expect("Failed add durable")
                        .build()
                        .expect("Failed store build");

                    Arc::new(store)
                        .put(
                            random::<RepositoryId>(),
                            Address {
                                hash: random::<Hash>(),
                                context: random::<Context>(),
                            },
                            Fragment {
                                flags: 0,
                                size_payload: 128,
                                size_content: 128,
                            },
                            Some(Bytes::from(vec![0u8; 128])),
                            false,
                        )
                        .await
                        .expect_err("a refused copy must fail the put");

                    assert_eq!(count(&durable, "copy"), 1);
                    assert_eq!(
                        count(&durable, "put"),
                        0,
                        "the payload was released, so there is nothing to fall back to and nothing \
                         may be stored"
                    );
                })
                .await;
        }

        /// `PayloadDoNotReplicate` governs the copy fan-out as it governs the put one.
        #[tokio::test]
        async fn a_copied_put_honours_do_not_replicate() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture_with_replica(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                        true,
                    );

                    fixture
                        .store
                        .clone()
                        .put(
                            fixture.partition,
                            fixture.address,
                            Fragment {
                                flags: FragmentFlags::PayloadDoNotReplicate.into(),
                                size_payload: 128,
                                size_content: 128,
                            },
                            Some(Bytes::from(vec![0u8; 128])),
                            false,
                        )
                        .await
                        .expect("Put failed");
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                    assert_eq!(count(&fixture.durable, "copy"), 1);
                    assert_eq!(
                        count(&fixture.replica, "copy"),
                        0,
                        "do_not_replicate must hold the copy back as it holds the put back"
                    );
                })
                .await;
        }

        /// A composite with no local store answers its own query from the durable store, so a
        /// partial match there is duplicated too — and there is no local mirror to spawn.
        #[tokio::test]
        async fn a_durable_only_composite_copies_from_its_own_match() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let durable: Arc<TestStore<'static>> = Arc::new(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );
                    let store = CompositeStoreBuilder::default()
                        .with_durable("durable".to_string(), durable.clone())
                        .expect("Failed add durable")
                        .build()
                        .expect("Failed store build");

                    Arc::new(store)
                        .put(
                            random::<RepositoryId>(),
                            Address {
                                hash: random::<Hash>(),
                                context: random::<Context>(),
                            },
                            Fragment {
                                flags: 0,
                                size_payload: 128,
                                size_content: 128,
                            },
                            Some(Bytes::from(vec![0u8; 128])),
                            false,
                        )
                        .await
                        .expect("Put failed");
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                    assert_eq!(count(&durable, "copy"), 1);
                    assert_eq!(count(&durable, "put"), 0);
                })
                .await;
        }

        /// A forced put is a write the caller asked for outright, so it is neither short-circuited
        /// by a full match nor turned into a copy.
        #[tokio::test]
        async fn a_forced_put_is_stored_not_copied() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );

                    fixture
                        .store
                        .clone()
                        .put(
                            fixture.partition,
                            fixture.address,
                            Fragment {
                                flags: 0,
                                size_payload: 128,
                                size_content: 128,
                            },
                            Some(Bytes::from(vec![0u8; 128])),
                            true,
                        )
                        .await
                        .expect("Put failed");

                    assert_eq!(count(&fixture.durable, "copy"), 0);
                    assert_eq!(count(&fixture.durable, "put"), 1);
                })
                .await;
        }
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
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::fragment::generate_random;
        use lore_revision::lore::RepositoryId;
        use lore_revision::store::composite::CompositeStoreBuilder;
        use lore_storage::ImmutableStore;
        use lore_storage::StoreError;
        use lore_storage::StoreGetData;
        use lore_storage::StoreObliterateStats;
        use lore_storage::local::immutable_store as immutable;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;
        use rand::random;
        use tokio::sync::RwLock;

        use crate::tests::setup_test_execution;

        struct DelayStore {
            get_delay: Duration,
            get_result: RwLock<Result<StoreGetData, StoreError>>,
            get_count: AtomicU32,
        }

        impl DelayStore {
            fn succeeding(fragment: Fragment, payload: Bytes, delay: Duration) -> Self {
                Self {
                    get_delay: delay,
                    get_result: RwLock::new(Ok(StoreGetData {
                        fragment,
                        match_made: lore_storage::StoreMatch::MatchFull,
                        partition: Partition::default(),
                        payload: Some(payload),
                    })),
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
            async fn get_metadata(
                self: Arc<Self>,
                partition: Partition,
                address: Address,
            ) -> Result<StoreGetData, StoreError> {
                let _ = (partition, address);
                Ok(StoreGetData::default())
            }

            async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
                true
            }

            async fn query(
                self: Arc<Self>,
                _repository: Partition,
                _addresses: &[Address],
                _results: &mut [lore_storage::StoreMatchResult],
            ) -> Result<(), StoreError> {
                Ok(())
            }

            async fn get(
                self: Arc<Self>,
                _repository: Partition,
                _address: Address,
            ) -> Result<StoreGetData, StoreError> {
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

            fn max_query_batch(&self) -> Option<usize> {
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
                        handles.push(lore_spawn!(
                            async move { store.get(repository, address).await }
                        ));
                    }

                    for handle in handles {
                        let (got_fragment, got_payload) = handle
                            .await
                            .expect("task panicked")
                            .and_then(StoreGetData::into_payload)
                            .expect("get failed");
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
                            store.get(repository, address).await
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

                    let match_result = lore_storage::immutable_store::query_one(
                        &(composite.clone() as Arc<dyn ImmutableStore>),
                        repository,
                        address,
                    )
                    .await
                    .expect("resolve should work");
                    assert_eq!(match_result.match_made, StoreMatch::MatchFull);

                    let (got_fragment, got_payload) = composite
                        .clone()
                        .get(repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
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
                        .get(repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
                        .expect("get should succeed via durable despite empty replica");
                    assert_eq!(got_fragment, fragment);
                    assert_eq!(got_payload, payload);
                })
                .await;
        }
    }

    mod durable_delay {
        use std::path::Path;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU32;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        use async_trait::async_trait;
        use bytes::Bytes;
        use lore_base::runtime::LORE_CONTEXT;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::fragment::generate_random;
        use lore_revision::lore::RepositoryId;
        use lore_revision::store::composite::CompositeStore;
        use lore_revision::store::composite::CompositeStoreBuilder;
        use lore_storage::ImmutableStore;
        use lore_storage::StoreError;
        use lore_storage::StoreGetData;
        use lore_storage::StoreMatch;
        use lore_storage::StoreObliterateStats;
        use lore_storage::local::immutable_store as immutable;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;
        use rand::random;

        use crate::tests::setup_test_execution;

        /// Wraps an `ImmutableStore` and tracks how many times each read method is called.
        struct CountingStore {
            inner: Arc<dyn ImmutableStore>,
            get_count: AtomicU32,
            resolve_count: AtomicU32,
        }

        impl std::fmt::Debug for CountingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "CountingStore")
            }
        }

        impl CountingStore {
            fn wrapping(inner: Arc<dyn ImmutableStore>) -> Self {
                Self {
                    inner,
                    get_count: AtomicU32::new(0),
                    resolve_count: AtomicU32::new(0),
                }
            }
        }

        #[async_trait]
        impl ImmutableStore for CountingStore {
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
                results: &mut [lore_storage::StoreMatchResult],
            ) -> Result<(), StoreError> {
                self.resolve_count.fetch_add(1, Ordering::SeqCst);
                self.inner
                    .clone()
                    .query(partition, addresses, results)
                    .await
            }

            async fn get(
                self: Arc<Self>,
                partition: Partition,
                address: Address,
            ) -> Result<StoreGetData, StoreError> {
                self.get_count.fetch_add(1, Ordering::SeqCst);
                self.inner.clone().get(partition, address).await
            }

            async fn put(
                self: Arc<Self>,
                partition: Partition,
                address: Address,
                fragment: Fragment,
                payload: Option<Bytes>,
                force: bool,
            ) -> Result<(), StoreError> {
                self.inner
                    .clone()
                    .put(partition, address, fragment, payload, force)
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

            async fn copy(
                self: Arc<Self>,
                source_partition: Partition,
                source_address: Address,
                destination_partition: Partition,
                destination_context: Context,
                durable: bool,
            ) -> Result<(), StoreError> {
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
        }

        async fn make_store() -> Arc<dyn ImmutableStore> {
            immutable::create(
                None::<&Path>,
                immutable::ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("should create in-memory store")
        }

        async fn build_composite_with_read_replica(
            durable: Arc<dyn ImmutableStore>,
            replica: Arc<dyn ImmutableStore>,
            durable_delay: Duration,
        ) -> Arc<CompositeStore> {
            Arc::new(
                CompositeStoreBuilder::default()
                    .with_local("local".to_string(), make_store().await)
                    .expect("local should work")
                    .with_durable("durable".to_string(), durable)
                    .expect("durable should work")
                    .with_replica("read-replica".to_string(), replica, true, false)
                    .with_durable_delay(durable_delay)
                    .build()
                    .expect("build should work"),
            )
        }

        async fn build_composite_no_replica(
            durable: Arc<dyn ImmutableStore>,
            durable_delay: Duration,
        ) -> Arc<CompositeStore> {
            Arc::new(
                CompositeStoreBuilder::default()
                    .with_local("local".to_string(), make_store().await)
                    .expect("local should work")
                    .with_durable("durable".to_string(), durable)
                    .expect("durable should work")
                    .with_durable_delay(durable_delay)
                    .build()
                    .expect("build should work"),
            )
        }

        /// When a read replica responds with a full match before the durable delay elapses,
        /// the durable `get` should be cancelled — never invoked.
        #[tokio::test]
        async fn get_durable_not_called_when_replica_responds_within_delay() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let (fragment, address, payload) = generate_random();
                    let repository: Partition = random::<RepositoryId>();

                    let replica = make_store().await;
                    replica
                        .clone()
                        .put(repository, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("put to replica failed");

                    let durable_inner = make_store().await;
                    durable_inner
                        .clone()
                        .put(repository, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("put to durable failed");
                    let durable = Arc::new(CountingStore::wrapping(durable_inner));

                    let composite = build_composite_with_read_replica(
                        durable.clone(),
                        replica,
                        Duration::from_millis(500),
                    )
                    .await;

                    let (got_fragment, got_payload) = composite
                        .get(repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
                        .expect("get should succeed via replica");

                    assert_eq!(got_fragment.size_payload, fragment.size_payload);
                    assert_eq!(got_payload, payload);
                    assert_eq!(
                        durable.get_count.load(Ordering::SeqCst),
                        0,
                        "durable get must not be called when replica responds within the delay window"
                    );
                })
                .await;
        }

        /// When there are no replicas the configured delay is bypassed entirely — the
        /// durable store is queried immediately regardless of how large the delay is.
        #[tokio::test]
        async fn get_durable_not_delayed_when_no_replicas() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let (fragment, address, payload) = generate_random();
                    let repository: Partition = random::<RepositoryId>();

                    let durable_inner = make_store().await;
                    durable_inner
                        .clone()
                        .put(repository, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("put to durable failed");
                    let durable = Arc::new(CountingStore::wrapping(durable_inner));

                    // Use a large delay that would block the test if it were applied.
                    let composite =
                        build_composite_no_replica(durable.clone(), Duration::from_millis(500))
                            .await;

                    let (got_fragment, got_payload) = composite
                        .get(repository, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
                        .expect("get should succeed immediately via durable");

                    assert_eq!(got_fragment.size_payload, fragment.size_payload);
                    assert_eq!(got_payload, payload);
                    assert_eq!(
                        durable.get_count.load(Ordering::SeqCst),
                        1,
                        "durable get must be called when there are no replicas"
                    );
                })
                .await;
        }

        /// When a read replica responds with `MatchFull` before the durable delay elapses,
        /// the durable `exist` should be cancelled — never invoked.
        #[tokio::test]
        async fn resolve_durable_not_called_when_replica_responds_within_delay() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let (fragment, address, payload) = generate_random();
                    let repository: Partition = random::<RepositoryId>();

                    let replica = make_store().await;
                    replica
                        .clone()
                        .put(repository, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("put to replica failed");

                    let durable_inner = make_store().await;
                    durable_inner
                        .clone()
                        .put(repository, address, fragment, Some(payload), false)
                        .await
                        .expect("put to durable failed");
                    let durable = Arc::new(CountingStore::wrapping(durable_inner));

                    let composite = build_composite_with_read_replica(
                        durable.clone(),
                        replica,
                        Duration::from_millis(500),
                    )
                    .await;

                    let match_result = lore_storage::immutable_store::query_one(
                        &(composite as Arc<dyn ImmutableStore>),
                        repository,
                        address,
                    )
                    .await
                    .expect("resolve should succeed via replica");

                    assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                    assert_eq!(
                        durable.resolve_count.load(Ordering::SeqCst),
                        0,
                        "durable resolve must not be called when replica responds within the delay window"
                    );
                })
                .await;
        }

        /// When there are no replicas the configured delay is bypassed entirely — the
        /// durable store is queried immediately regardless of how large the delay is.
        #[tokio::test]
        async fn resolve_durable_not_delayed_when_no_replicas() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let (fragment, address, payload) = generate_random();
                    let repository: Partition = random::<RepositoryId>();

                    let durable_inner = make_store().await;
                    durable_inner
                        .clone()
                        .put(repository, address, fragment, Some(payload), false)
                        .await
                        .expect("put to durable failed");
                    let durable = Arc::new(CountingStore::wrapping(durable_inner));

                    // Use a large delay that would block the test if it were applied.
                    let composite =
                        build_composite_no_replica(durable.clone(), Duration::from_millis(500))
                            .await;

                    let match_result = lore_storage::immutable_store::query_one(
                        &(composite as Arc<dyn ImmutableStore>),
                        repository,
                        address,
                    )
                    .await
                    .expect("resolve should succeed immediately via durable");

                    assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                    assert_eq!(
                        durable.resolve_count.load(Ordering::SeqCst),
                        1,
                        "durable resolve must be called when there are no replicas"
                    );
                })
                .await;
        }
    }
}
