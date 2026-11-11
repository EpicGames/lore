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
    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;
    use lore_base::types::HealResult;
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
    use lore_storage::immutable_store::CopyBehavior;
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
        /// What each `put` this store received carried: the fragment describing the content, and
        /// the payload's size where one came with it.
        puts: RwLock<Vec<(Fragment, Option<usize>)>>,
        compare_and_swap_result: Option<Hash>,
        get_immutable_result: Option<StoreGetData>,
        /// The representation `get_metadata` reports, where a test needs it to differ from what
        /// was put.
        metadata_fragment: Option<Fragment>,
        max_query_batch: Option<usize>,
        /// A gate each `put` waits at once recorded, for a test holding a write under way.
        put_gate: Option<Arc<tokio::sync::Semaphore>>,
        /// A gate each `get` waits at once recorded, for a test holding a read under way.
        get_gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    /// What a succeeding store hands back from `get` where a test has not named a payload of its
    /// own, so any path that reads content has bytes to carry.
    const DEFAULT_PAYLOAD: &[u8] = b"test store payload";

    impl TestStore<'_> {
        fn succeeding() -> Self {
            let payload = Bytes::from_static(DEFAULT_PAYLOAD);
            Self {
                succeed: true,
                get_immutable_result: Some(StoreGetData {
                    fragment: Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    },
                    match_made: StoreMatch::MatchFull,
                    partition: Partition::default(),
                    payload: Some(payload),
                    content: None,
                }),
                ..Default::default()
            }
        }

        fn succeeding_limited(limit: usize) -> Self {
            Self {
                max_query_batch: Some(limit),
                ..Self::succeeding()
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
                content: None,
            });
            self
        }

        fn with_mock_metadata_fragment(mut self, fragment: Fragment) -> Self {
            self.metadata_fragment = Some(fragment);
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

        /// The one `put` this store was given, for a test asserting on what it carried.
        fn only_put(&self) -> (Fragment, Option<usize>) {
            let puts = self.puts.read().unwrap();
            assert_eq!(
                puts.len(),
                1,
                "expected exactly one put, got {}",
                puts.len()
            );
            puts[0]
        }
    }

    /// Whether a fragment describes its payload as held by the durable store, which is what a
    /// store receiving it reports as `stored_durable`.
    fn claims_durable(fragment: Fragment) -> bool {
        fragment.flags & FragmentFlags::PayloadStoredDurable.bits() != 0
    }

    #[async_trait]
    impl lore_storage::MutableStore for TestStore<'_> {
        /// Nothing on disk to claim.
        async fn hold_for_command(
            self: std::sync::Arc<Self>,
        ) -> std::result::Result<
            Option<lore_storage::local::store_lock::StoreGuard>,
            lore_storage::StoreError,
        > {
            Ok(None)
        }

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

        async fn flush(
            self: Arc<Self>,
            _options: lore_storage::FlushOptions,
        ) -> Result<(), StoreError> {
            Ok(())
        }
    }

    #[async_trait]
    impl lore_storage::ImmutableStore for TestStore<'static> {
        /// Nothing on disk to claim.
        async fn hold_for_command(
            self: std::sync::Arc<Self>,
        ) -> std::result::Result<
            Option<lore_storage::local::store_lock::StoreGuard>,
            lore_storage::StoreError,
        > {
            Ok(None)
        }

        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.track_invocation("get_metadata");
            let _ = address;

            if self.succeed {
                // Describes the payload as the store's own `query` reports it, so a caller reading
                // one and then the other is not told two different things about the same content.
                let mut fragment = self.metadata_fragment.unwrap_or_default();
                if self.match_durable {
                    fragment.flags |= FragmentFlags::PayloadStoredDurable.bits();
                }
                Ok(StoreGetData::metadata(
                    fragment,
                    self.match_result.unwrap_or(StoreMatch::MatchFull),
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
            if let Some(gate) = &self.get_gate {
                let _passed = gate.acquire().await.expect("the gate stays open");
            }

            if !self.succeed {
                return Err(StoreError::internal("Mock store failure"));
            }
            let mut data = self.get_immutable_result.clone().unwrap();
            // One knob sets the level every verb reports, so a store cannot answer `get` at a
            // level its own `query` and `get_metadata` contradict.
            if let Some(match_result) = self.match_result {
                data.match_made = match_result;
            }
            Ok(data)
        }

        async fn put(
            self: Arc<Self>,
            _repository: Partition,
            _address: Address,
            fragment: Fragment,
            payload: Option<Bytes>,
            _force: bool,
        ) -> Result<(), StoreError> {
            self.track_invocation("put");
            self.puts
                .write()
                .unwrap()
                .push((fragment, payload.as_ref().map(Bytes::len)));
            if let Some(gate) = &self.put_gate {
                let _passed = gate.acquire().await.expect("the gate stays open");
            }

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
            _behavior: CopyBehavior,
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

        async fn flush(
            self: Arc<Self>,
            _options: lore_storage::FlushOptions,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
            Ok(())
        }

        async fn heal(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
        ) -> Result<HealResult, StoreError> {
            self.track_invocation("heal");
            Ok(HealResult::Healed)
        }
    }

    /// A heal reaches the local tier and the durable store, which holds the copy the local tier was
    /// filled from.
    #[tokio::test]
    async fn a_heal_reaches_the_local_tier_and_the_durable_store() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let (local_store, durable_store) = (
                    Arc::new(TestStore::failing()),
                    Arc::new(TestStore::succeeding()),
                );
                let composite_store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("test-durable".to_string(), durable_store.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local_store.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked"),
                );
                let (_, address, _) = generate_random();

                let healed = composite_store
                    .heal(random::<RepositoryId>(), address)
                    .await;

                assert!(matches!(healed, Ok(HealResult::Healed)), "{healed:?}");
                let heals =
                    |store: &TestStore<'_>| store.invocations.read().unwrap().get("heal").copied();
                assert_eq!(heals(&durable_store), Some(1));
                assert_eq!(heals(&local_store), Some(1));
            })
            .await;
    }

    /// A heal that arrives while a read's payload is written to the local tier waits for the
    /// write, so it checks the copy the write leaves rather than finding none and leaving a
    /// corrupt payload the durable store served to be served again.
    #[tokio::test]
    async fn a_heal_waits_for_the_local_write_a_read_started() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                // A local store that misses every read and holds every write at the gate.
                let local_store = Arc::new(TestStore {
                    put_gate: Some(gate.clone()),
                    ..TestStore::failing()
                });
                let durable_store = Arc::new(TestStore::succeeding());
                let composite_store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("test-durable".to_string(), durable_store.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local_store.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked"),
                );
                let count = |store: &Arc<TestStore<'static>>, verb: &str| {
                    store
                        .invocations
                        .read()
                        .unwrap()
                        .get(verb)
                        .copied()
                        .unwrap_or(0)
                };
                let repository: Partition = random::<RepositoryId>();
                let (_, address, _) = generate_random();

                composite_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect("the read");
                assert_eq!(
                    composite_store.caching(),
                    (1, DEFAULT_PAYLOAD.len() as u64),
                    "the write and its payload are counted while it runs"
                );
                let mut healing = std::pin::pin!(composite_store.clone().heal(repository, address));
                assert!(
                    futures::poll!(healing.as_mut()).is_pending(),
                    "the heal waits for the local write"
                );
                assert_eq!(
                    count(&local_store, "heal"),
                    0,
                    "no heal before the write ends"
                );

                gate.add_permits(1);
                let healed = tokio::time::timeout(tokio::time::Duration::from_secs(5), healing)
                    .await
                    .expect("the heal ends once the write has");
                assert!(matches!(healed, Ok(HealResult::Healed)), "{healed:?}");
                assert_eq!(count(&local_store, "put"), 1);
                assert_eq!(count(&local_store, "heal"), 1);
                assert_eq!(count(&durable_store, "heal"), 1);
                // The write's count ends before its hold, which the heal waited for.
                assert_eq!(
                    composite_store.caching(),
                    (0, 0),
                    "the ended write is no longer counted"
                );
            })
            .await;
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
    async fn replicas_get_metadata_results_are_cached_locally() {
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

                // and the local store should have the cache because composite store will
                // populate it out of band
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                let result = local_store
                    .clone()
                    .get_metadata(repository, address)
                    .await
                    .expect("local confirmation failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));
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

    /// **A get that caches what the durable store returned keeps the local store claimed past the
    /// span it ran in, until the cache write ends.** The get returns once it has spawned the
    /// write, so it claims the local store before the spawn: the span's end then leaves the flock
    /// held rather than releasing it before the write first runs, which on a single-threaded
    /// runtime is after the span has ended.
    #[tokio::test]
    async fn a_cached_get_keeps_the_local_flock_past_its_span_until_the_cache_write_ends() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let dir = lore_base::test_util::TempDir::new("composite-cache-claim-");
                let local = immutable::LocalImmutableStore::new(
                    Some(dir.to_path_buf()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("local should have been created");
                let lock = local.lock.clone().expect("a disk-backed store has a lock");
                let local_store: Arc<dyn ImmutableStore> = local;
                let durable_store = immutable::create(
                    None::<&Path>,
                    immutable::ImmutableStoreCreateOptions::none(),
                    false,
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("durable should have been created");
                let composite_store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("test-durable".to_string(), durable_store.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local_store.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked"),
                );
                let repository: Partition = random::<RepositoryId>();
                let (fragment, address, payload) = generate_random();
                durable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Put to durable failed");

                let span = composite_store
                    .clone()
                    .hold_for_command()
                    .await
                    .expect("span");
                let result = composite_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect("get");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));
                drop(span);
                assert!(
                    lock.is_held(),
                    "the cache write keeps the local flock past the span"
                );

                tokio::time::timeout(tokio::time::Duration::from_secs(10), async {
                    while !matches!(
                        local_store.clone().get(repository, address).await,
                        Ok(StoreGetData {
                            match_made: StoreMatch::MatchFull,
                            ..
                        })
                    ) {
                        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("the cache write lands");
                local_store
                    .clone()
                    .flush(lore_storage::FlushOptions::SYNC_DATA)
                    .await
                    .expect("flush");
                assert!(
                    !lock.is_held(),
                    "the flock goes once the cache write has ended and been flushed"
                );
            })
            .await;
    }

    /// **A read arriving while the local write of what the durable store served is under way takes
    /// the result the first read fetched,** without the content the first read's check expanded.
    /// Neither the local store nor the in-flight read has it for that read otherwise, and it would
    /// fetch the address again; once the write ends, a read the local store still misses asks the
    /// durable store again.
    #[tokio::test]
    async fn a_read_during_the_local_write_of_a_durable_result_takes_that_result() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                // A local store that misses every read and holds every write at the gate.
                let local_store = Arc::new(TestStore {
                    put_gate: Some(gate.clone()),
                    ..TestStore::failing()
                });
                // A durable store whose reads carry the content a check on receipt expanded.
                let mut durable = TestStore::succeeding();
                if let Some(data) = durable.get_immutable_result.as_mut() {
                    data.content = data.payload.clone().map(|payload| (data.fragment, payload));
                }
                let durable_store = Arc::new(durable);
                let composite_store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("test-durable".to_string(), durable_store.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local_store.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked"),
                );
                let count = |store: &Arc<TestStore<'static>>, verb: &str| {
                    store
                        .invocations
                        .read()
                        .unwrap()
                        .get(verb)
                        .copied()
                        .unwrap_or(0)
                };
                let repository: Partition = random::<RepositoryId>();
                let (_, address, _) = generate_random();

                let first = composite_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect("the first read");
                tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
                    while count(&local_store, "put") == 0 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("the local write starts");
                let second = composite_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect("the second read");

                assert_eq!(
                    count(&durable_store, "get"),
                    1,
                    "the second read takes the first one's result"
                );
                assert_eq!(second.payload, first.payload);
                assert!(first.content.is_some());
                assert!(second.content.is_none(), "the held result keeps no content");

                gate.add_permits(1);
                tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
                    while count(&durable_store, "get") < 2 {
                        composite_store
                            .clone()
                            .get(repository, address)
                            .await
                            .expect("a read after the write");
                        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("a read after the write asks the durable store again");
            })
            .await;
    }

    /// **A read waiting on another read's fetch from the durable store takes the fetched result
    /// whole, content included, while a read arriving during the local write of that result takes
    /// the result held for it, without the content.** The test's runtime runs one task at a time,
    /// so a read seen to have missed the local store has also subscribed to the fetch in flight.
    #[tokio::test]
    async fn reads_awaiting_a_fetch_get_its_content_and_reads_during_the_local_write_do_not() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let (get_gate, put_gate) = (
                    Arc::new(tokio::sync::Semaphore::new(0)),
                    Arc::new(tokio::sync::Semaphore::new(0)),
                );
                // A local store that misses every read and holds every write at its gate.
                let local_store = Arc::new(TestStore {
                    put_gate: Some(put_gate.clone()),
                    ..TestStore::failing()
                });
                // A durable store that holds every read at its gate, and whose reads carry the
                // content a check on receipt expanded.
                let mut durable = TestStore {
                    get_gate: Some(get_gate.clone()),
                    ..TestStore::succeeding()
                };
                if let Some(data) = durable.get_immutable_result.as_mut() {
                    data.content = data.payload.clone().map(|payload| (data.fragment, payload));
                }
                let durable_store = Arc::new(durable);
                let composite_store = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_durable("test-durable".to_string(), durable_store.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local_store.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked"),
                );
                let count = |store: &Arc<TestStore<'static>>, verb: &str| {
                    store
                        .invocations
                        .read()
                        .unwrap()
                        .get(verb)
                        .copied()
                        .unwrap_or(0)
                };
                let repository: Partition = random::<RepositoryId>();
                let (_, address, _) = generate_random();
                let read = || {
                    let store = composite_store.clone();
                    lore_spawn!(async move { store.get(repository, address).await })
                };

                let fetching = read();
                tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
                    while count(&durable_store, "get") == 0 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("the first read reaches the durable store");
                let waiting = read();
                tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
                    while count(&local_store, "get") < 2 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("the second read misses the local store");
                get_gate.add_permits(1);
                let fetched = fetching
                    .await
                    .expect("the first read's task")
                    .expect("the first read");
                let waited = waiting
                    .await
                    .expect("the second read's task")
                    .expect("the second read");
                let during_write = composite_store
                    .clone()
                    .get(repository, address)
                    .await
                    .expect("a read during the local write");

                assert_eq!(
                    count(&durable_store, "get"),
                    1,
                    "the reads share the first one's fetch"
                );
                assert!(fetched.content.is_some());
                assert_eq!(waited.payload, fetched.payload);
                assert!(
                    waited.content.is_some(),
                    "the read waiting on the fetch takes its content"
                );
                assert_eq!(during_write.payload, fetched.payload);
                assert!(
                    during_write.content.is_none(),
                    "the held result keeps no content"
                );
                put_gate.add_permits(1);
            })
            .await;
    }

    /// A composite store passes on the content a durable store expanded where it checked a payload
    /// on receipt, so a read into a buffer through the composite places that content.
    #[tokio::test]
    async fn a_durable_stores_checked_content_is_placed_through_the_composite() {
        let content = Bytes::from_static(b"content a durable store expanded to check its payload");
        let plain = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let payload = Bytes::from_static(b"payload");
        let durable = TestStore {
            get_immutable_result: Some(StoreGetData {
                fragment: Fragment {
                    flags: FragmentFlags::PayloadCompressedLZ4.bits(),
                    size_payload: payload.len() as u32,
                    size_content: content.len() as u64,
                },
                match_made: StoreMatch::MatchFull,
                partition: Partition::default(),
                payload: Some(payload),
                content: Some((plain, content.clone())),
            }),
            ..TestStore::succeeding()
        };

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let store = CompositeStoreBuilder::default()
                    .with_local("failing, local".to_string(), Arc::new(TestStore::failing()))
                    .expect("Failed add local")
                    .with_durable("checking, durable".to_string(), Arc::new(durable))
                    .expect("Failed add durable")
                    .build()
                    .expect("Failed store build");
                let address = Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                };
                let mut buffer = vec![0u8; content.len()];
                // SAFETY: the buffer outlives the read and nothing else touches it.
                let mut dst =
                    unsafe { lore_storage::CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };

                let (fragment, read) = Arc::new(store)
                    .get_into(random::<RepositoryId>(), address, &mut dst)
                    .await
                    .expect("the composite reads the durable store's content");

                assert!(matches!(
                    read,
                    lore_storage::store_types::PayloadRead::IntoBuffer
                ));
                assert_eq!(fragment, plain);
                assert_eq!(buffer, content.as_ref());
            })
            .await;
    }

    #[tokio::test]
    async fn replicas_get_results_are_cached_locally() {
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
                    .get(repository, address)
                    .await
                    .expect("post-put query failed");
                assert!(matches!(result.match_made, StoreMatch::MatchFull));

                // and the local store should have the cache, payload included, because
                // composite store will populate it out of band
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
    async fn replica_query_match_full_results_are_cached_locally() {
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
                    .expect("Put to replica failed");

                // query via composite: replica resolves MatchFull with a non-zero partition, so
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

    mod copy {
        use super::*;

        struct Fixture {
            composite: Arc<lore_revision::store::composite::CompositeStore>,
            local: Arc<dyn ImmutableStore>,
            durable: Arc<dyn ImmutableStore>,
            partition: Partition,
        }

        /// A composite over two real stores, so the match levels a local store reports for an
        /// address it holds under another context are the ones production sees.
        async fn fixture(cache_metadata: bool, semaphore_size: Option<usize>) -> Fixture {
            let durable = immutable::create(
                None::<&Path>,
                immutable::ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("durable should have been created");
            let local = immutable::create(
                None::<&Path>,
                immutable::ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings::default(),
            )
            .await
            .expect("local should have been created");

            let composite = CompositeStoreBuilder::default()
                .with_cache_metadata(cache_metadata, semaphore_size)
                .with_durable("test-durable".to_string(), durable.clone())
                .expect("durable should have worked")
                .with_local("test-local".to_string(), local.clone())
                .expect("local should have worked")
                .build()
                .expect("build should have worked");

            Fixture {
                composite: Arc::new(composite),
                local,
                durable,
                partition: random::<RepositoryId>(),
            }
        }

        /// The content the local store serves for `address` on its own, read after the detached
        /// caching task has had a chance to run.
        async fn local_payload(fixture: &Fixture, address: Address) -> Bytes {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            fixture
                .local
                .clone()
                .get(fixture.partition, address)
                .await
                .and_then(lore_storage::StoreGetData::into_payload)
                .expect("local get failed")
                .1
        }

        /// The level the local store resolves `context` at for the copied hash, read after the
        /// detached caching task has had a chance to run.
        async fn local_match(fixture: &Fixture, address: Address) -> StoreMatch {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            fixture
                .local
                .clone()
                .get_metadata(fixture.partition, address)
                .await
                .expect("local get_metadata failed")
                .match_made
        }

        fn destination(source: Address) -> Address {
            Address {
                hash: source.hash,
                context: random::<Context>(),
            }
        }

        /// The same composite over stores that record which verb each leg was asked for, for the
        /// cases about who is told about a copy rather than what is resolvable afterwards.
        struct Recording {
            composite: Arc<lore_revision::store::composite::CompositeStore>,
            local: Arc<TestStore<'static>>,
            replica: Arc<TestStore<'static>>,
            partition: Partition,
            source: Address,
            payload: Bytes,
        }

        /// `local_match` is what the local store answers for the destination, which decides whether
        /// the composite has anything to record there.
        fn recording(local_match: StoreMatch, durable: TestStore<'static>) -> Recording {
            recording_with_out_of_band(local_match, durable, None)
        }

        /// `record_out_of_band` is passed to the builder as the setting arrives from configuration,
        /// so `None` exercises the default.
        fn recording_with_out_of_band(
            local_match: StoreMatch,
            durable: TestStore<'static>,
            record_out_of_band: Option<bool>,
        ) -> Recording {
            let payload = Bytes::from_static(b"the payload the copy's destination reads back as");
            let fragment = Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            };
            // The source is durably stored, which is the condition under which a caller reaches
            // `copy` at all rather than storing the payload.
            let local: Arc<TestStore<'static>> = Arc::new(
                TestStore::succeeding()
                    .with_mock_match(local_match)
                    .with_mock_durable_match(random::<Context>())
                    .with_mock_get_immutable(&fragment, &payload),
            );
            let replica: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
            let composite = CompositeStoreBuilder::default()
                .with_cache_metadata(true, None)
                .with_record_copy_out_of_band(record_out_of_band)
                .with_local("local".to_string(), local.clone())
                .expect("local should have worked")
                .with_durable("durable".to_string(), Arc::new(durable))
                .expect("durable should have worked")
                .with_replica("replica".to_string(), replica.clone(), false, true)
                .build()
                .expect("build should have worked");

            Recording {
                composite: Arc::new(composite),
                local,
                replica,
                partition: random::<RepositoryId>(),
                source: Address {
                    hash: random::<Hash>(),
                    context: random::<Context>(),
                },
                payload,
            }
        }

        /// Copies `source` to a fresh context and waits out the detached work the copy spawns.
        async fn copy_recorded(
            recording: &Recording,
            durable: bool,
            do_not_replicate: bool,
        ) -> Result<(), StoreError> {
            let result = recording
                .composite
                .clone()
                .copy(
                    recording.partition,
                    recording.source,
                    recording.partition,
                    random::<Context>(),
                    CopyBehavior {
                        durable,
                        do_not_replicate,
                    },
                )
                .await;
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            result
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

        /// The destination the copy created is readable in process afterwards, content and all, so
        /// the next read of it does not leave for a partition and hash the durable store has
        /// already answered for.
        #[tokio::test]
        async fn caches_the_destination_locally() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(true, None).await;
                    let (fragment, source, payload) = generate_random();
                    fixture
                        .durable
                        .clone()
                        .put(
                            fixture.partition,
                            source,
                            fragment,
                            Some(payload.clone()),
                            false,
                        )
                        .await
                        .expect("put to durable failed");

                    let destination = destination(source);
                    fixture
                        .composite
                        .clone()
                        .copy(
                            fixture.partition,
                            source,
                            fixture.partition,
                            destination.context,
                            CopyBehavior {
                                durable: true,
                                do_not_replicate: false,
                            },
                        )
                        .await
                        .expect("copy failed");

                    assert_eq!(
                        local_match(&fixture, destination).await,
                        StoreMatch::MatchFull
                    );
                    assert_eq!(local_payload(&fixture, destination).await, payload);
                })
                .await;
        }

        /// The same, for a destination whose hash the local store already resolves under another
        /// context. A weaker level there is not the association the copy created, so it is no
        /// answer for the destination and it still has to be recorded.
        #[tokio::test]
        async fn caches_the_destination_over_a_partial_local_match() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(true, None).await;
                    let (fragment, source, payload) = generate_random();
                    fixture
                        .durable
                        .clone()
                        .put(
                            fixture.partition,
                            source,
                            fragment,
                            Some(payload.clone()),
                            false,
                        )
                        .await
                        .expect("put to durable failed");
                    fixture
                        .local
                        .clone()
                        .put(
                            fixture.partition,
                            source,
                            fragment,
                            Some(payload.clone()),
                            false,
                        )
                        .await
                        .expect("put to local failed");

                    let destination = destination(source);
                    assert_eq!(
                        fixture
                            .local
                            .clone()
                            .get_metadata(fixture.partition, destination)
                            .await
                            .expect("local get_metadata failed")
                            .match_made,
                        StoreMatch::MatchPartition,
                        "the fixture only tests what it means to if the local store answers the \
                         destination with a level below a full match"
                    );

                    fixture
                        .composite
                        .clone()
                        .copy(
                            fixture.partition,
                            source,
                            fixture.partition,
                            destination.context,
                            CopyBehavior {
                                durable: true,
                                do_not_replicate: false,
                            },
                        )
                        .await
                        .expect("copy failed");

                    assert_eq!(
                        local_match(&fixture, destination).await,
                        StoreMatch::MatchFull
                    );
                    assert_eq!(local_payload(&fixture, destination).await, payload);
                })
                .await;
        }

        /// Metadata caching governs what a `query` or `get_metadata` miss writes back, not what a
        /// copy records: the destination is read back for its content, and a read that reaches the
        /// durable store caches what it carried either way. `record_copy_out_of_band` is the
        /// setting that stops a copy being recorded.
        /// A local store asked to hold metadata alone is given the destination without its
        /// content, as it is on a put or a read. The write replicas are told separately and are
        /// not subject to the setting.
        #[tokio::test]
        async fn strips_the_payload_where_the_local_store_holds_metadata_only() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let durable = immutable::create(
                        None::<&Path>,
                        immutable::ImmutableStoreCreateOptions::none(),
                        false,
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("durable should have been created");
                    let local = immutable::create(
                        None::<&Path>,
                        immutable::ImmutableStoreCreateOptions::none(),
                        false,
                        ImmutableStoreSettings::default(),
                    )
                    .await
                    .expect("local should have been created");
                    let composite = CompositeStoreBuilder::default()
                        .with_cache_metadata(true, None)
                        .with_local_metadata_only(true)
                        .with_durable("test-durable".to_string(), durable.clone())
                        .expect("durable should have worked")
                        .with_local("test-local".to_string(), local.clone())
                        .expect("local should have worked")
                        .build()
                        .expect("build should have worked");

                    let partition: Partition = random::<RepositoryId>();
                    let (fragment, source, payload) = generate_random();
                    durable
                        .clone()
                        .put(partition, source, fragment, Some(payload), false)
                        .await
                        .expect("put to durable failed");

                    let destination = destination(source);
                    Arc::new(composite)
                        .copy(
                            partition,
                            source,
                            partition,
                            destination.context,
                            CopyBehavior {
                                durable: true,
                                do_not_replicate: false,
                            },
                        )
                        .await
                        .expect("copy failed");
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                    assert_eq!(
                        lore_storage::immutable_store::query_one(&local, partition, destination)
                            .await
                            .expect("local query failed")
                            .match_made,
                        StoreMatch::MatchFull
                    );
                    let error = local
                        .clone()
                        .get(partition, destination)
                        .await
                        .expect_err("the local store must not hold the content");
                    assert!(error.is_payload_not_found(), "{error:?}");
                })
                .await;
        }

        #[tokio::test]
        async fn records_the_destination_when_metadata_caching_is_disabled() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture(false, None).await;
                    let (fragment, source, payload) = generate_random();
                    fixture
                        .durable
                        .clone()
                        .put(fixture.partition, source, fragment, Some(payload), false)
                        .await
                        .expect("put to durable failed");

                    let destination = destination(source);
                    fixture
                        .composite
                        .clone()
                        .copy(
                            fixture.partition,
                            source,
                            fixture.partition,
                            destination.context,
                            CopyBehavior {
                                durable: true,
                                do_not_replicate: false,
                            },
                        )
                        .await
                        .expect("copy failed");

                    assert_eq!(
                        local_match(&fixture, destination).await,
                        StoreMatch::MatchFull
                    );
                })
                .await;
        }

        /// The write replicas are sent the destination's payload, not its fragment alone, so a
        /// replica holding none of the bytes can still answer for the address afterwards.
        #[tokio::test]
        async fn tells_the_write_replicas_of_the_destination() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let recording = recording(StoreMatch::MatchPartition, TestStore::succeeding());

                    copy_recorded(&recording, true, false)
                        .await
                        .expect("copy failed");

                    let (fragment, payload) = recording.replica.only_put();
                    assert_eq!(payload, Some(recording.payload.len()));
                    assert!(
                        claims_durable(fragment),
                        "the durable store made the association, so a replica must not be left \
                         describing the destination as content only this store holds"
                    );
                })
                .await;
        }

        /// A caller that has taken replication on itself gets no fan-out. This is what stops a
        /// copy arriving from a peer being sent back to the region it came from.
        #[tokio::test]
        async fn honours_do_not_replicate() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let recording = recording(StoreMatch::MatchPartition, TestStore::succeeding());

                    copy_recorded(&recording, true, true)
                        .await
                        .expect("copy failed");

                    assert_eq!(count(&recording.replica, "put"), 0);
                    assert_eq!(
                        count(&recording.local, "put"),
                        1,
                        "holding the copy back from peers does not hold it back from this process"
                    );
                    assert!(claims_durable(recording.local.only_put().0));
                })
                .await;
        }

        /// Recording a copy beyond the durable store is a deployment's to switch off, and doing so
        /// leaves the durable store the only holder of the destination association. Every other
        /// case here builds the composite without naming the setting, so the default is what they
        /// exercise.
        #[tokio::test]
        async fn records_nothing_out_of_band_when_switched_off() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let recording = recording_with_out_of_band(
                        StoreMatch::MatchPartition,
                        TestStore::succeeding(),
                        Some(false),
                    );

                    copy_recorded(&recording, true, false)
                        .await
                        .expect("copy failed");

                    assert_eq!(count(&recording.local, "put"), 0);
                    assert_eq!(count(&recording.replica, "put"), 0);
                    assert_eq!(
                        count(&recording.local, "get"),
                        0,
                        "nothing is recorded, so there is no reason to read the destination back \
                         either"
                    );
                })
                .await;
        }

        /// A local answer below a full match is not the association the copy created, so the
        /// destination is recorded whatever weaker level the read back resolved at.
        #[tokio::test]
        async fn records_the_destination_over_any_partial_local_level() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    for level in [StoreMatch::MatchHash, StoreMatch::MatchPartition] {
                        let recording = recording(level, TestStore::succeeding());

                        copy_recorded(&recording, true, false)
                            .await
                            .expect("copy failed");

                        assert_eq!(count(&recording.local, "put"), 1, "{level:?}");
                        assert_eq!(
                            count(&recording.replica, "put"),
                            1,
                            "a replica's holdings are not this store's to infer from its own"
                        );
                    }
                })
                .await;
        }

        /// Copies a random source to a fresh context through `composite` and waits out the
        /// detached work the copy spawns.
        async fn copy_through(composite: lore_revision::store::composite::CompositeStore) {
            Arc::new(composite)
                .copy(
                    random::<RepositoryId>(),
                    Address {
                        hash: random::<Hash>(),
                        context: random::<Context>(),
                    },
                    random::<RepositoryId>(),
                    random::<Context>(),
                    CopyBehavior {
                        durable: true,
                        do_not_replicate: false,
                    },
                )
                .await
                .expect("copy failed");
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        /// A read back the local store cannot answer and the durable store does is cached locally
        /// by the read itself, so the destination is written to the local store once, not once
        /// for the read and again for the copy.
        #[tokio::test]
        async fn writes_the_destination_locally_once_when_the_durable_store_answers_the_read() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let local: Arc<TestStore<'static>> = Arc::new(TestStore::failing());
                    let durable: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                    let composite = CompositeStoreBuilder::default()
                        .with_local("local".to_string(), local.clone())
                        .expect("local should have worked")
                        .with_durable("durable".to_string(), durable.clone())
                        .expect("durable should have worked")
                        .build()
                        .expect("build should have worked");

                    copy_through(composite).await;

                    assert_eq!(count(&durable, "get"), 1);
                    assert_eq!(count(&local, "put"), 1);
                })
                .await;
        }

        /// A read back answered by a replica is cached locally by the read itself, so the
        /// destination is written to the local store once, as when the durable store answers. The
        /// copy, not the replica, confirms the destination durable, and that write records it.
        #[tokio::test]
        async fn writes_the_destination_locally_once_when_a_replica_answers_the_read() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let local: Arc<TestStore<'static>> = Arc::new(TestStore::failing());
                    let durable: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                    let replica: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                    let composite = CompositeStoreBuilder::default()
                        .with_durable_delay(tokio::time::Duration::from_secs(10))
                        .with_local("local".to_string(), local.clone())
                        .expect("local should have worked")
                        .with_durable("durable".to_string(), durable.clone())
                        .expect("durable should have worked")
                        .with_replica("replica".to_string(), replica.clone(), true, false)
                        .build()
                        .expect("build should have worked");

                    copy_through(composite).await;

                    assert_eq!(
                        count(&durable, "get"),
                        0,
                        "the durable read is held back long enough for the replica to answer"
                    );
                    assert_eq!(count(&replica, "get"), 1);
                    assert_eq!(count(&local, "put"), 1);
                    let (recorded, _) = local.puts.read().unwrap()[0];
                    assert_ne!(
                        recorded.flags & FragmentFlags::PayloadStoredDurable.bits(),
                        0,
                        "the copy confirmed the destination durable, so the local record says so"
                    );
                })
                .await;
        }

        /// A destination the local store answers in full, content included, is already the record
        /// the copy would write, so it is not written again. The write replicas are told either
        /// way, since what they hold is not this store's to infer from its own.
        #[tokio::test]
        async fn writes_nothing_locally_for_a_destination_already_held_in_full() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let payload = Bytes::from_static(b"content the local store already holds");
                    let held = Fragment {
                        flags: FragmentFlags::PayloadStoredLocal.bits(),
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };
                    let local: Arc<TestStore<'static>> = Arc::new(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchFull)
                            .with_mock_durable_match(random::<Context>())
                            .with_mock_get_immutable(&held, &payload),
                    );
                    let replica: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                    let composite = CompositeStoreBuilder::default()
                        .with_cache_metadata(true, None)
                        .with_local("local".to_string(), local.clone())
                        .expect("local should have worked")
                        .with_durable("durable".to_string(), Arc::new(TestStore::succeeding()))
                        .expect("durable should have worked")
                        .with_replica("replica".to_string(), replica.clone(), false, true)
                        .build()
                        .expect("build should have worked");

                    Arc::new(composite)
                        .copy(
                            random::<RepositoryId>(),
                            Address {
                                hash: random::<Hash>(),
                                context: random::<Context>(),
                            },
                            random::<RepositoryId>(),
                            random::<Context>(),
                            CopyBehavior {
                                durable: true,
                                do_not_replicate: false,
                            },
                        )
                        .await
                        .expect("copy failed");
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                    assert_eq!(count(&local, "put"), 0);
                    assert_eq!(
                        count(&replica, "put"),
                        1,
                        "a replica's holdings are not this store's to infer from its own"
                    );
                })
                .await;
        }

        /// A refused copy is the caller's to retry, and nothing may be recorded as though it
        /// happened — a local or replica record of an association the durable store never made
        /// would answer for content no one holds.
        #[tokio::test]
        async fn a_refused_copy_records_nothing_anywhere() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let recording = recording(StoreMatch::MatchPartition, TestStore::failing());

                    let error = copy_recorded(&recording, true, false)
                        .await
                        .expect_err("a refused copy must fail");
                    assert!(error.is_internal(), "{error:?}");

                    assert_eq!(count(&recording.local, "put"), 0);
                    assert_eq!(count(&recording.replica, "put"), 0);
                })
                .await;
        }

        /// A composite with no local store has nowhere to cache, and the copy is the durable
        /// store's own.
        #[tokio::test]
        async fn a_durable_only_composite_caches_nothing() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let durable: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
                    let composite = CompositeStoreBuilder::default()
                        .with_cache_metadata(true, None)
                        .with_durable("durable".to_string(), durable.clone())
                        .expect("durable should have worked")
                        .build()
                        .expect("build should have worked");

                    Arc::new(composite)
                        .copy(
                            random::<RepositoryId>(),
                            Address {
                                hash: random::<Hash>(),
                                context: random::<Context>(),
                            },
                            random::<RepositoryId>(),
                            random::<Context>(),
                            CopyBehavior {
                                durable: true,
                                do_not_replicate: false,
                            },
                        )
                        .await
                        .expect("copy failed");

                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    let invocations = durable.invocations.read().unwrap();
                    assert_eq!(invocations.get("copy").copied(), Some(1));
                    assert_eq!(
                        invocations.get("get").copied(),
                        None,
                        "the durable store is the local store, so there is no cache to fill and no \
                         reason to read the destination back"
                    );
                })
                .await;
        }
    }

    /// **A composite claims the local store it wraps**, for a command and for a task.
    ///
    /// The replicas and the durable upstream are remote and have nothing to claim, so
    /// the local target's claim is the whole of it. A composite answering `None` would
    /// let a storage handle open holding nothing and then read and write a store no
    /// process has claimed — silently, since an empty claim is indistinguishable from
    /// a store that needed none.
    #[tokio::test]
    async fn a_composite_claims_the_local_store_it_wraps() {
        use lore_storage::local::immutable_store::ImmutableStoreSettings;
        use lore_storage::local::immutable_store::LocalImmutableStore;

        let local_dir = lore_base::test_util::TempDir::new("lore-composite-claim-local-");
        let durable_dir = lore_base::test_util::TempDir::new("lore-composite-claim-durable-");

        let execution = setup_test_execution();
        let local_path = local_dir.path().to_path_buf();
        let durable_path = durable_dir.path().to_path_buf();
        LORE_CONTEXT
            .scope(execution, async move {
                let local_store = LocalImmutableStore::new(
                    Some(local_path.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("local store");
                let local_lock = local_store
                    .lock
                    .clone()
                    .expect("a disk-backed store has a lock");
                let local: Arc<dyn lore_storage::ImmutableStore> = local_store;
                // A store of its own as the durable target, so the claim can be seen to land
                // on the local store and not on the one a durable read would reach.
                let durable: Arc<dyn lore_storage::ImmutableStore> = LocalImmutableStore::new(
                    Some(durable_path.clone()),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("durable store");
                let composite = Arc::new(
                    CompositeStoreBuilder::default()
                        .with_local("local".to_string(), local)
                        .expect("local target")
                        .with_durable("durable".to_string(), durable)
                        .expect("durable target")
                        .build()
                        .expect("composite builds"),
                );

                let claim = composite
                    .clone()
                    .hold_for_command()
                    .await
                    .expect("claim taken");
                assert!(
                    claim.is_some(),
                    "a composite wrapping a store with files on disk must claim it, \
                     not inherit the default that claims nothing"
                );
                // A lock opened on a store directory shares that directory's state, so it
                // reports what the composite's claim holds.
                let held = |path: &std::path::Path| {
                    lore_storage::local::store_lock::StoreLock::new(path.join("immutable"), false)
                        .expect("a store directory")
                        .is_held()
                };
                assert!(held(&local_path), "the claim holds the local store");
                assert!(!held(&durable_path), "and not the durable one");
                drop(claim);

                let task_claim = composite.hold_for_task().await.expect("claim taken");
                assert!(
                    task_claim.is_some(),
                    "a task's claim lands on the local store too"
                );
                assert!(held(&local_path), "the task's claim holds the local store");
                assert!(
                    !local_lock.covers(lore_storage::local::store_lock::Intent::Read),
                    "and covers no operation, as the local store's own task claim does"
                );
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
            build_fixture(local, false, false)
        }

        fn fixture_with_replica(local: TestStore<'static>, write_replica: bool) -> Fixture {
            build_fixture(local, write_replica, false)
        }

        /// The same with a write replica, over a local store asked to hold metadata alone.
        fn fixture_with_metadata_only_local(local: TestStore<'static>) -> Fixture {
            build_fixture(local, true, true)
        }

        fn build_fixture(
            local: TestStore<'static>,
            write_replica: bool,
            local_metadata_only: bool,
        ) -> Fixture {
            let local_store: Arc<TestStore<'static>> = Arc::new(local);
            let durable: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
            let replica: Arc<TestStore<'static>> = Arc::new(TestStore::succeeding());
            let store = CompositeStoreBuilder::default()
                .with_local_metadata_only(local_metadata_only)
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
                        count(&fixture.local, "put"),
                        1,
                        "the local cache must be given the association too, or the next read of the \
                         target address leaves the process for bytes it already holds"
                    );
                    let (fragment, payload) = fixture.local.only_put();
                    assert_eq!(
                        payload,
                        Some(128),
                        "the caller's own payload is carried into the copy, so the local record of \
                         the destination can answer a read without the content being read back"
                    );
                    assert!(
                        claims_durable(fragment),
                        "the durable store made this association, so the local record of it must \
                         say so or a later put will store the payload again"
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
        /// not drop that. The destination is read back and the replica sent the content, so a
        /// replica holding none of the bytes can still answer a read for the address rather than
        /// only accelerating `query` for it.
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
                        count(&fixture.replica, "put"),
                        1,
                        "the write replica must be told of the destination association too"
                    );
                    let (fragment, payload) = fixture.replica.only_put();
                    assert_eq!(
                        payload,
                        Some(128),
                        "the replica is sent the destination's content, not the representation \
                         alone, and on this path that content is the caller's own payload"
                    );
                    assert!(
                        claims_durable(fragment),
                        "a replica holds no opinion on durability of its own, so what it reports \
                         for the destination is what this store told it"
                    );
                })
                .await;
        }

        /// The caller's fragment and payload already describe the destination, so recording the
        /// copy reads nothing back. Reading the content would be the transfer the copy was taken to
        /// avoid, paid immediately after avoiding it.
        #[tokio::test]
        async fn a_copied_put_does_not_read_the_content_back() {
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
                    assert_eq!(
                        count(&fixture.local, "get"),
                        0,
                        "the put supplied the content, so no store is asked for it again"
                    );
                    assert_eq!(count(&fixture.durable, "get"), 0);
                    assert_eq!(
                        count(&fixture.local, "get_metadata"),
                        0,
                        "the put supplied the representation, so no store is asked for it either"
                    );
                    assert_eq!(count(&fixture.durable, "get_metadata"), 0);
                })
                .await;
        }

        /// The durable source can hold the content in another representation than the one the put
        /// supplies, so the destination is recorded with the put's fragment: it is the only one
        /// that describes the payload it travels with.
        #[tokio::test]
        async fn a_copied_put_pairs_its_payload_with_its_own_fragment() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture_with_replica(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>())
                            .with_mock_metadata_fragment(Fragment {
                                flags: FragmentFlags::PayloadCompressedZstd.bits(),
                                size_payload: 64,
                                size_content: 128,
                            }),
                        true,
                    );

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(count(&fixture.durable, "copy"), 1);
                    for (name, store) in [("local", &fixture.local), ("replica", &fixture.replica)]
                    {
                        let (fragment, payload) = store.only_put();
                        assert_eq!(payload, Some(128), "{name} must be sent the put's payload");
                        assert_eq!(
                            fragment.flags & FragmentFlags::PayloadCompressed.bits(),
                            0,
                            "{name} must not be told uncompressed bytes are compressed"
                        );
                        assert_eq!(
                            (fragment.size_payload, fragment.size_content),
                            (128, 128),
                            "{name} must be given the sizes of the payload it holds"
                        );
                        assert!(claims_durable(fragment));
                    }
                })
                .await;
        }

        /// `local_metadata_only` is the local store's setting and no one else's, so a replica is
        /// still sent the content even where the local record is the representation alone.
        #[tokio::test]
        async fn a_copied_put_strips_the_local_payload_alone() {
            let execution = setup_test_execution();
            LORE_CONTEXT
                .scope(execution, async move {
                    let fixture = fixture_with_metadata_only_local(
                        TestStore::succeeding()
                            .with_mock_match(StoreMatch::MatchPartition)
                            .with_mock_durable_match(random::<Context>()),
                    );

                    put(&fixture, Some(Bytes::from(vec![0u8; 128]))).await;

                    assert_eq!(
                        fixture.local.only_put().1,
                        None,
                        "a local store asked to hold metadata alone is given no content"
                    );
                    assert_eq!(
                        fixture.replica.only_put().1,
                        Some(128),
                        "the replica holds no such setting, and one that is sent no content cannot \
                         answer a read for the destination"
                    );
                })
                .await;
        }

        /// A replica that refuses the association leaves the put succeeding: the durable store
        /// holds it, which is what the put was for.
        #[tokio::test]
        async fn a_replica_refusing_the_association_does_not_fail_the_put() {
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
                    assert_eq!(count(&replica, "put"), 1);
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
                        count(&fixture.replica, "put"),
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
        use lore_revision::store::composite::CompositeStore;
        use lore_revision::store::composite::CompositeStoreBuilder;
        use lore_revision::store::composite::ReadReplicaSelection;
        use lore_storage::ImmutableStore;
        use lore_storage::StoreError;
        use lore_storage::StoreGetData;
        use lore_storage::StoreObliterateStats;
        use lore_storage::immutable_store::CopyBehavior;
        use lore_storage::local::immutable_store as immutable;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;
        use rand::random;
        use tokio::sync::RwLock;

        use crate::tests::setup_test_execution;

        struct DelayStore {
            get_delay: Duration,
            get_result: RwLock<Result<StoreGetData, StoreError>>,
            get_count: AtomicU32,
            get_metadata_count: AtomicU32,
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
                        content: None,
                    })),
                    get_count: AtomicU32::new(0),
                    get_metadata_count: AtomicU32::new(0),
                }
            }

            fn failing(error: StoreError, delay: Duration) -> Self {
                Self {
                    get_delay: delay,
                    get_result: RwLock::new(Err(error)),
                    get_count: AtomicU32::new(0),
                    get_metadata_count: AtomicU32::new(0),
                }
            }

            fn get_count(&self) -> u32 {
                self.get_count.load(Ordering::SeqCst)
            }

            fn get_metadata_count(&self) -> u32 {
                self.get_metadata_count.load(Ordering::SeqCst)
            }
        }

        impl std::fmt::Debug for DelayStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "DelayStore")
            }
        }

        #[async_trait]
        impl ImmutableStore for DelayStore {
            /// Nothing on disk to claim.
            async fn hold_for_command(
                self: std::sync::Arc<Self>,
            ) -> std::result::Result<
                Option<lore_storage::local::store_lock::StoreGuard>,
                lore_storage::StoreError,
            > {
                Ok(None)
            }

            /// Answers from the same configured result as `get`, minus the payload, so a metadata
            /// lookup and a full read of the same address agree on whether it is there.
            async fn get_metadata(
                self: Arc<Self>,
                _partition: Partition,
                _address: Address,
            ) -> Result<StoreGetData, StoreError> {
                self.get_metadata_count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.get_delay).await;
                self.get_result
                    .read()
                    .await
                    .clone()
                    .map(|data| StoreGetData {
                        payload: None,
                        ..data
                    })
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

            async fn flush(
                self: Arc<Self>,
                _options: lore_storage::FlushOptions,
            ) -> Result<(), StoreError> {
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
                _behavior: CopyBehavior,
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

        /// Long enough that every task in a batch reaches the inflight map before the first
        /// fan-out resolves, so what the assertions see is genuine contention.
        const FAN_OUT_DELAY: Duration = Duration::from_millis(200);

        const NUM_CONCURRENT: usize = 5;

        async fn composite_over(durable: Arc<DelayStore>) -> Arc<CompositeStore> {
            Arc::new(
                CompositeStoreBuilder::default()
                    .with_local("local".to_string(), create_empty_local().await)
                    .expect("local should work")
                    .with_durable("durable".to_string(), durable)
                    .expect("durable should work")
                    .build()
                    .expect("build should work"),
            )
        }

        mod get {
            use super::*;

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
                            FAN_OUT_DELAY,
                        ));
                        let composite = composite_over(durable.clone()).await;

                        let mut handles = Vec::with_capacity(NUM_CONCURRENT);
                        for _ in 0..NUM_CONCURRENT {
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

            /// Callers share one remote read, so with one try per read one replica is asked for
            /// all of them.
            #[tokio::test]
            async fn round_robin_shared_read_sends_one_replica_request() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution.clone(), async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();

                        let durable = Arc::new(DelayStore::succeeding(
                            fragment,
                            payload.clone(),
                            FAN_OUT_DELAY,
                        ));
                        let replicas: Vec<Arc<DelayStore>> = (0..3)
                            .map(|_| {
                                Arc::new(DelayStore::succeeding(
                                    fragment,
                                    payload.clone(),
                                    FAN_OUT_DELAY,
                                ))
                            })
                            .collect();
                        let mut builder = CompositeStoreBuilder::default()
                            .with_local("local".to_string(), create_empty_local().await)
                            .expect("local should work")
                            .with_durable("durable".to_string(), durable)
                            .expect("durable should work")
                            .with_read_replica_selection(ReadReplicaSelection::RoundRobin)
                            .with_round_robin_tries(Some(std::num::NonZeroUsize::MIN), None);
                        for (index, replica) in replicas.iter().enumerate() {
                            builder = builder.with_replica(
                                format!("read-replica-{index}"),
                                replica.clone(),
                                true,
                                false,
                            );
                        }
                        let composite = Arc::new(builder.build().expect("build should work"));

                        let mut handles = Vec::with_capacity(NUM_CONCURRENT);
                        for _ in 0..NUM_CONCURRENT {
                            let store = composite.clone();
                            handles.push(lore_spawn!(async move {
                                store.get(repository, address).await
                            }));
                        }
                        for handle in handles {
                            let (_, got_payload) = handle
                                .await
                                .expect("task panicked")
                                .and_then(StoreGetData::into_payload)
                                .expect("get failed");
                            assert_eq!(got_payload, payload);
                        }

                        let replica_gets: u32 = replicas.iter().map(|r| r.get_count()).sum();
                        assert_eq!(replica_gets, 1);
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
                            FAN_OUT_DELAY,
                        ));
                        let composite = composite_over(durable.clone()).await;

                        let mut handles = Vec::with_capacity(NUM_CONCURRENT);
                        for _ in 0..NUM_CONCURRENT {
                            let store = composite.clone();
                            handles.push(lore_spawn!(async move {
                                store.get(repository, address).await
                            }));
                        }

                        for handle in handles {
                            let error = handle
                                .await
                                .expect("task panicked")
                                .expect_err("get should have failed");
                            assert!(error.is_address_not_found(), "should be AddressNotFound");
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

        mod get_metadata {
            use super::*;

            #[tokio::test]
            async fn success_propagated_to_listeners() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution.clone(), async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();

                        let durable =
                            Arc::new(DelayStore::succeeding(fragment, payload, FAN_OUT_DELAY));
                        let composite = composite_over(durable.clone()).await;

                        let mut handles = Vec::with_capacity(NUM_CONCURRENT);
                        for _ in 0..NUM_CONCURRENT {
                            let store = composite.clone();
                            handles.push(lore_spawn!(async move {
                                store.get_metadata(repository, address).await
                            }));
                        }

                        for handle in handles {
                            let data = handle
                                .await
                                .expect("task panicked")
                                .expect("get_metadata failed");
                            assert_eq!(data.match_made, lore_storage::StoreMatch::MatchFull);
                            assert_eq!(data.fragment.size_payload, fragment.size_payload);
                            assert_eq!(
                                data.payload, None,
                                "a metadata answer carries no payload to copy to its listeners"
                            );
                        }

                        assert_eq!(
                            durable.get_metadata_count(),
                            1,
                            "inflight dedup should collapse concurrent get_metadata calls into a single durable lookup"
                        );
                    })
                    .await;
            }

            /// A durable store that cannot answer leaves the composite with nothing better than
            /// the empty starting result, and that is what every listener is handed.
            #[tokio::test]
            async fn durable_error_answers_every_listener_with_no_match() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution.clone(), async move {
                        let repository: Partition = random::<RepositoryId>();
                        let address = random::<Address>();

                        let durable = Arc::new(DelayStore::failing(
                            StoreError::from(lore_storage::AddressNotFound::from(address)),
                            FAN_OUT_DELAY,
                        ));
                        let composite = composite_over(durable.clone()).await;

                        let mut handles = Vec::with_capacity(NUM_CONCURRENT);
                        for _ in 0..NUM_CONCURRENT {
                            let store = composite.clone();
                            handles.push(lore_spawn!(async move {
                                store.get_metadata(repository, address).await
                            }));
                        }

                        for handle in handles {
                            let data = handle
                                .await
                                .expect("task panicked")
                                .expect("get_metadata reports a miss rather than failing");
                            assert_eq!(data.match_made, lore_storage::StoreMatch::MatchNone);
                        }

                        assert_eq!(
                            durable.get_metadata_count(),
                            1,
                            "inflight dedup should collapse concurrent get_metadata calls into a single durable lookup even on failure"
                        );
                    })
                    .await;
            }

            /// A durable store that refuses the caller is not read as holding nothing: every
            /// listener is handed the refusal.
            #[tokio::test]
            async fn durable_refusal_answers_every_listener_with_the_refusal() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution.clone(), async move {
                        let repository: Partition = random::<RepositoryId>();
                        let address = random::<Address>();

                        let durable = Arc::new(DelayStore::failing(
                            StoreError::from(lore_base::error::NotAuthorized),
                            FAN_OUT_DELAY,
                        ));
                        let composite = composite_over(durable.clone()).await;

                        let mut handles = Vec::with_capacity(NUM_CONCURRENT);
                        for _ in 0..NUM_CONCURRENT {
                            let store = composite.clone();
                            handles.push(lore_spawn!(async move {
                                store.get_metadata(repository, address).await
                            }));
                        }

                        for handle in handles {
                            let error = handle
                                .await
                                .expect("task panicked")
                                .expect_err("get_metadata reports the refusal");
                            assert!(error.is_not_authorized(), "{error:?}");
                        }

                        assert_eq!(durable.get_metadata_count(), 1);
                    })
                    .await;
            }

            /// The inflight slot is keyed by partition and address, so unrelated lookups running
            /// at the same time must not be served one another's answer.
            #[tokio::test]
            async fn distinct_addresses_each_fan_out() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution.clone(), async move {
                        let (fragment, first_address, payload) = generate_random();
                        let (_, second_address, _) = generate_random();
                        let repository: Partition = random::<RepositoryId>();

                        let durable =
                            Arc::new(DelayStore::succeeding(fragment, payload, FAN_OUT_DELAY));
                        let composite = composite_over(durable.clone()).await;

                        let mut handles = Vec::with_capacity(2);
                        for address in [first_address, second_address] {
                            let store = composite.clone();
                            handles.push(lore_spawn!(async move {
                                store.get_metadata(repository, address).await
                            }));
                        }

                        for handle in handles {
                            let data = handle
                                .await
                                .expect("task panicked")
                                .expect("get_metadata failed");
                            assert_eq!(data.match_made, lore_storage::StoreMatch::MatchFull);
                        }

                        assert_eq!(durable.get_metadata_count(), 2);
                    })
                    .await;
            }

            /// The slot is released once the fan-out it covers has broadcast, so a later caller
            /// starts a fresh one instead of subscribing to a channel nothing will send on.
            #[tokio::test]
            async fn slot_released_once_the_fan_out_completes() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution.clone(), async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();

                        let durable =
                            Arc::new(DelayStore::succeeding(fragment, payload, Duration::ZERO));
                        let composite = composite_over(durable.clone()).await;

                        for _ in 0..2 {
                            let data = composite
                                .clone()
                                .get_metadata(repository, address)
                                .await
                                .expect("get_metadata failed");
                            assert_eq!(data.match_made, lore_storage::StoreMatch::MatchFull);
                        }

                        assert_eq!(durable.get_metadata_count(), 2);
                    })
                    .await;
            }
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

        pub(super) fn create_peer_info(name: &str) -> PeerInfo {
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
                    // The store that served it holds the payload, and says so.
                    let mut held = fragment;
                    held.flags |= lore_base::types::FragmentFlags::PayloadStoredLocal.bits();
                    assert_eq!(got_fragment, held);
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
                    // The store that served it holds the payload, and says so.
                    let mut held = fragment;
                    held.flags |= lore_base::types::FragmentFlags::PayloadStoredLocal.bits();
                    assert_eq!(got_fragment, held);
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
        use lore_base::error::AddressNotFound;
        use lore_base::error::SlowDown;
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
        use lore_storage::immutable_store::CopyBehavior;
        use lore_storage::local::immutable_store as immutable;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;
        use rand::random;

        use crate::tests::setup_test_execution;

        /// Wraps an `ImmutableStore` and tracks how many times each read method is called.
        struct CountingStore {
            inner: Arc<dyn ImmutableStore>,
            get_count: AtomicU32,
            get_metadata_count: AtomicU32,
            resolve_count: AtomicU32,
            reads: Reads,
            /// The sequence this store's reads follow in place of `reads`, with this store's index.
            sequence: Option<(usize, Arc<ReadSequence>)>,
        }

        /// How a `CountingStore` answers the reads it counts.
        #[derive(Clone, Copy)]
        enum Reads {
            /// Reads pass through to the wrapped store.
            Answer,
            /// Reads never complete, standing in for a replica too slow to answer.
            Stall,
            /// Reads fail with an internal error.
            Fail,
            /// Reads panic, failing the task that runs them.
            Panic,
            /// Reads fail with `SlowDown`, as an overloaded store does.
            SlowDown,
            /// Reads find nothing, whatever the wrapped store holds.
            Miss,
            /// Reads find the content under another partition only.
            Partial,
        }

        /// A script that the stores of one test share: the n-th read any of them receives waits
        /// the n-th delay and then reads as the n-th `Reads` says. Reads past the script miss.
        struct ReadSequence {
            steps: Vec<(Duration, Reads)>,
            /// Which store each read reached, and when, in the order they arrived.
            requests: std::sync::Mutex<Vec<(usize, std::time::Instant)>>,
        }

        impl ReadSequence {
            fn new(steps: Vec<(Duration, Reads)>) -> Arc<Self> {
                Arc::new(Self {
                    steps,
                    requests: Default::default(),
                })
            }

            fn requests(&self) -> Vec<(usize, std::time::Instant)> {
                self.requests.lock().unwrap().clone()
            }
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
                    get_metadata_count: AtomicU32::new(0),
                    resolve_count: AtomicU32::new(0),
                    reads: Reads::Answer,
                    sequence: None,
                }
            }

            fn sequenced(
                inner: Arc<dyn ImmutableStore>,
                index: usize,
                sequence: Arc<ReadSequence>,
            ) -> Self {
                Self {
                    sequence: Some((index, sequence)),
                    ..Self::wrapping(inner)
                }
            }

            /// How this read answers, after the wait its step names.
            async fn next_reads(&self) -> Reads {
                let Some((index, sequence)) = &self.sequence else {
                    return self.reads;
                };
                let (delay, reads) = {
                    let mut requests = sequence.requests.lock().unwrap();
                    requests.push((*index, std::time::Instant::now()));
                    sequence
                        .steps
                        .get(requests.len() - 1)
                        .copied()
                        .unwrap_or((Duration::ZERO, Reads::Miss))
                };
                tokio::time::sleep(delay).await;
                reads
            }

            fn with_reads(inner: Arc<dyn ImmutableStore>, reads: Reads) -> Self {
                Self {
                    reads,
                    ..Self::wrapping(inner)
                }
            }

            async fn intercept_read(reads: Reads) -> Result<(), StoreError> {
                match reads {
                    Reads::Answer | Reads::Miss | Reads::Partial => Ok(()),
                    Reads::Stall => std::future::pending().await,
                    Reads::Fail => Err(StoreError::internal("replica read failed")),
                    Reads::Panic => panic!("replica read panicked"),
                    Reads::SlowDown => Err(StoreError::from(SlowDown)),
                }
            }
        }

        #[async_trait]
        impl ImmutableStore for CountingStore {
            /// Joins the claim of the store this one wraps.
            async fn hold_for_command(
                self: std::sync::Arc<Self>,
            ) -> std::result::Result<
                Option<lore_storage::local::store_lock::StoreGuard>,
                lore_storage::StoreError,
            > {
                lore_storage::ImmutableStore::hold_for_command(self.inner.clone()).await
            }

            async fn get_metadata(
                self: Arc<Self>,
                partition: Partition,
                address: Address,
            ) -> Result<StoreGetData, StoreError> {
                self.get_metadata_count.fetch_add(1, Ordering::SeqCst);
                let reads = self.next_reads().await;
                Self::intercept_read(reads).await?;
                if matches!(reads, Reads::Miss) {
                    return Ok(StoreGetData::default());
                }
                let mut data = self.inner.clone().get_metadata(partition, address).await?;
                if matches!(reads, Reads::Partial) {
                    data.match_made = StoreMatch::MatchHash;
                }
                Ok(data)
            }

            async fn query(
                self: Arc<Self>,
                partition: Partition,
                addresses: &[Address],
                results: &mut [lore_storage::StoreMatchResult],
            ) -> Result<(), StoreError> {
                self.resolve_count.fetch_add(1, Ordering::SeqCst);
                let reads = self.next_reads().await;
                Self::intercept_read(reads).await?;
                if matches!(reads, Reads::Miss) {
                    results.fill(lore_storage::StoreMatchResult::default());
                    return Ok(());
                }
                self.inner
                    .clone()
                    .query(partition, addresses, results)
                    .await?;
                if matches!(reads, Reads::Partial) {
                    for result in results.iter_mut() {
                        result.match_made = StoreMatch::MatchHash;
                    }
                }
                Ok(())
            }

            async fn get(
                self: Arc<Self>,
                partition: Partition,
                address: Address,
            ) -> Result<StoreGetData, StoreError> {
                self.get_count.fetch_add(1, Ordering::SeqCst);
                let reads = self.next_reads().await;
                Self::intercept_read(reads).await?;
                if matches!(reads, Reads::Miss) {
                    return Err(StoreError::from(AddressNotFound::from(address)));
                }
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

            async fn flush(
                self: Arc<Self>,
                options: lore_storage::FlushOptions,
            ) -> Result<(), StoreError> {
                self.inner.clone().flush(options).await
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
                behavior: CopyBehavior,
            ) -> Result<(), StoreError> {
                self.inner
                    .clone()
                    .copy(
                        source_partition,
                        source_address,
                        destination_partition,
                        destination_context,
                        behavior,
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

        async fn builder_with_read_replicas(
            durable: Arc<dyn ImmutableStore>,
            replicas: Vec<Arc<dyn ImmutableStore>>,
            durable_delay: Duration,
        ) -> CompositeStoreBuilder {
            let mut builder = CompositeStoreBuilder::default()
                .with_local("local".to_string(), make_store().await)
                .expect("local should work")
                .with_durable("durable".to_string(), durable)
                .expect("durable should work");
            for (index, replica) in replicas.into_iter().enumerate() {
                builder =
                    builder.with_replica(format!("read-replica-{index}"), replica, true, false);
            }
            builder.with_durable_delay(durable_delay)
        }

        async fn build_composite_with_read_replicas(
            durable: Arc<dyn ImmutableStore>,
            replicas: Vec<Arc<dyn ImmutableStore>>,
            durable_delay: Duration,
        ) -> Arc<CompositeStore> {
            Arc::new(
                builder_with_read_replicas(durable, replicas, durable_delay)
                    .await
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

        /// Long enough that an operation waiting it out fails the `PROMPTLY` bound below.
        const LONG_DELAY: Duration = Duration::from_secs(60);
        /// Generous bound for an operation that must not wait out `LONG_DELAY`.
        const PROMPTLY: Duration = Duration::from_secs(10);

        async fn seeded_durable(
            repository: Partition,
            address: Address,
            fragment: Fragment,
            payload: Bytes,
        ) -> Arc<CountingStore> {
            let inner = make_store().await;
            inner
                .clone()
                .put(repository, address, fragment, Some(payload), false)
                .await
                .expect("put to durable failed");
            Arc::new(CountingStore::wrapping(inner))
        }

        async fn missing_replicas() -> Vec<Arc<dyn ImmutableStore>> {
            vec![make_store().await, make_store().await]
        }

        /// What the composite's local store serves for `address` once the background cache write
        /// of a remote answer has landed. Panics if it does not land within `PROMPTLY`.
        async fn cached_locally(
            composite: &Arc<CompositeStore>,
            repository: Partition,
            address: Address,
        ) -> StoreGetData {
            tokio::time::timeout(PROMPTLY, async {
                loop {
                    if let Ok(cached) = composite.local().get(repository, address).await {
                        break cached;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the remote answer should be cached locally")
        }

        /// A composite whose only holder of the content is a read replica, which stores it with
        /// `flags`. The durable store is empty.
        async fn composite_reading_from_replica(
            repository: Partition,
            address: Address,
            mut fragment: Fragment,
            payload: Bytes,
            flags: lore_base::types::FragmentFlags,
        ) -> Arc<CompositeStore> {
            fragment.flags |= flags.bits();
            let replica = make_store().await;
            replica
                .clone()
                .put(repository, address, fragment, Some(payload), false)
                .await
                .expect("put to replica failed");
            build_composite_with_read_replicas(make_store().await, vec![replica], LONG_DELAY).await
        }

        mod get {
            use super::*;

            /// When a read replica responds with a full match before the durable delay elapses,
            /// the durable `get` is never started.
            #[tokio::test]
            async fn durable_not_called_when_replica_responds_within_delay() {
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

                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![replica],
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
                        // Gives a durable query spawned by mistake time to reach the store.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        assert_eq!(
                            durable.get_count.load(Ordering::SeqCst),
                            0,
                            "durable get must not be called when replica responds within the delay window"
                        );
                    })
                    .await;
            }

            /// A `get` answered by a read replica is cached locally, so the next `get` asks no
            /// remote store.
            #[tokio::test]
            async fn replica_answer_serves_the_next_get_locally() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();

                        let replica_inner = make_store().await;
                        replica_inner
                            .clone()
                            .put(repository, address, fragment, Some(payload.clone()), false)
                            .await
                            .expect("put to replica failed");
                        let replica = Arc::new(CountingStore::wrapping(replica_inner));
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;

                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![replica.clone()],
                            LONG_DELAY,
                        )
                        .await;

                        let get = || async {
                            let (_, got_payload) = composite
                                .clone()
                                .get(repository, address)
                                .await
                                .and_then(lore_storage::StoreGetData::into_payload)
                                .expect("get should succeed");
                            assert_eq!(got_payload, payload);
                        };

                        get().await;
                        cached_locally(&composite, repository, address).await;
                        get().await;

                        assert_eq!(replica.get_count.load(Ordering::SeqCst), 1);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            /// A replica answer is not confirmed durable by the composite's durable store, so the
            /// local copy keeps the durable bit the replica reported. Where the replica holds it
            /// only locally, the cached copy is protected from eviction: it may be the only copy
            /// this composite can still reach.
            #[tokio::test]
            async fn a_replica_answer_held_only_locally_there_is_not_evicted() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let composite = composite_reading_from_replica(
                            repository,
                            address,
                            fragment,
                            payload.clone(),
                            lore_base::types::FragmentFlags::empty(),
                        )
                        .await;

                        composite
                            .clone()
                            .get(repository, address)
                            .await
                            .expect("get should succeed via replica");
                        cached_locally(&composite, repository, address).await;
                        composite
                            .local()
                            .evict(0, false, None)
                            .await
                            .expect("evict failed");

                        let (_, kept) = composite
                            .local()
                            .get(repository, address)
                            .await
                            .and_then(lore_storage::StoreGetData::into_payload)
                            .expect("the local copy should survive eviction");
                        assert_eq!(kept, payload);
                    })
                    .await;
            }

            /// A replica answer the replica reports as durable keeps that bit locally, so the
            /// cached copy stays evictable.
            #[tokio::test]
            async fn a_replica_answer_durable_there_stays_evictable() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let composite = composite_reading_from_replica(
                            repository,
                            address,
                            fragment,
                            payload,
                            lore_base::types::FragmentFlags::PayloadStoredDurable,
                        )
                        .await;

                        composite
                            .clone()
                            .get(repository, address)
                            .await
                            .expect("get should succeed via replica");
                        cached_locally(&composite, repository, address).await;
                        composite
                            .local()
                            .evict(0, false, None)
                            .await
                            .expect("evict failed");

                        composite
                            .local()
                            .get(repository, address)
                            .await
                            .expect_err("the durable local copy should have been evicted");
                    })
                    .await;
            }

            /// When there are no replicas the configured delay is bypassed entirely — the
            /// durable store is queried immediately regardless of how large the delay is.
            #[tokio::test]
            async fn durable_not_delayed_when_no_replicas() {
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

                        let composite =
                            build_composite_no_replica(durable.clone(), LONG_DELAY).await;

                        let (got_fragment, got_payload) =
                            tokio::time::timeout(PROMPTLY, composite.get(repository, address))
                                .await
                                .expect("get should not wait out the durable delay")
                                .and_then(lore_storage::StoreGetData::into_payload)
                                .expect("get should succeed via durable");

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

            /// Once every read replica has missed, the durable `get` starts without waiting out the
            /// delay.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            missing_replicas().await,
                            LONG_DELAY,
                        )
                        .await;

                        let (_, got_payload) =
                            tokio::time::timeout(PROMPTLY, composite.get(repository, address))
                                .await
                                .expect("get should not wait out the durable delay")
                                .and_then(StoreGetData::into_payload)
                                .expect("get should succeed via durable");

                        assert_eq!(got_payload, payload);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A delay too long for the clock to represent still lets the durable `get` start once
            /// every read replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_under_an_unrepresentable_delay() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            missing_replicas().await,
                            Duration::MAX,
                        )
                        .await;

                        let (_, got_payload) =
                            tokio::time::timeout(PROMPTLY, composite.get(repository, address))
                                .await
                                .expect("get should not wait out the durable delay")
                                .and_then(StoreGetData::into_payload)
                                .expect("get should succeed via durable");

                        assert_eq!(got_payload, payload);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica whose read fails counts as having finished, so the durable `get` starts
            /// without waiting out the delay once the other replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_fails() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let failing: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Fail));
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, failing],
                            LONG_DELAY,
                        )
                        .await;

                        let (_, got_payload) =
                            tokio::time::timeout(PROMPTLY, composite.get(repository, address))
                                .await
                                .expect("get should not wait out the durable delay")
                                .and_then(StoreGetData::into_payload)
                                .expect("get should succeed via durable");

                        assert_eq!(got_payload, payload);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// While any read replica is still working on a request, the durable `get` waits out
            /// the delay, even after the other replicas have missed.
            #[tokio::test]
            async fn durable_waits_out_delay_while_a_replica_is_outstanding() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let stalled: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Stall));
                        let delay = Duration::from_millis(300);
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, stalled],
                            delay,
                        )
                        .await;

                        let started = std::time::Instant::now();
                        let (_, got_payload) = composite
                            .get(repository, address)
                            .await
                            .and_then(StoreGetData::into_payload)
                            .expect("get should succeed via durable");

                        assert!(
                            started.elapsed() >= delay,
                            "durable get must wait out the delay while a replica is outstanding"
                        );
                        assert_eq!(got_payload, payload);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica whose read task panics counts as having finished, so the durable `get`
            /// starts without waiting out the delay once the other replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_panics() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let panicking: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Panic));
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, panicking],
                            LONG_DELAY,
                        )
                        .await;

                        let (_, got_payload) =
                            tokio::time::timeout(PROMPTLY, composite.get(repository, address))
                                .await
                                .expect("get should not wait out the durable delay")
                                .and_then(StoreGetData::into_payload)
                                .expect("get should succeed via durable");

                        assert_eq!(got_payload, payload);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica that answers `SlowDown` counts as having finished, so the durable `get`
            /// starts without waiting out the delay once the other replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_slows_down() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let throttled: Arc<dyn ImmutableStore> = Arc::new(
                            CountingStore::with_reads(make_store().await, Reads::SlowDown),
                        );
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, throttled],
                            LONG_DELAY,
                        )
                        .await;

                        let (_, got_payload) =
                            tokio::time::timeout(PROMPTLY, composite.get(repository, address))
                                .await
                                .expect("get should not wait out the durable delay")
                                .and_then(StoreGetData::into_payload)
                                .expect("get should succeed via durable");

                        assert_eq!(got_payload, payload);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }
        }

        mod get_metadata {
            use super::*;

            /// Once every read replica has missed, the durable `get_metadata` starts without
            /// waiting out the delay.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            missing_replicas().await,
                            LONG_DELAY,
                        )
                        .await;

                        let metadata = tokio::time::timeout(
                            PROMPTLY,
                            composite.get_metadata(repository, address),
                        )
                        .await
                        .expect("get_metadata should not wait out the durable delay")
                        .expect("get_metadata should succeed via durable");

                        assert_eq!(metadata.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// While any read replica is still working on a request, the durable `get_metadata`
            /// waits out the delay, even after the other replicas have missed.
            #[tokio::test]
            async fn durable_waits_out_delay_while_a_replica_is_outstanding() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let stalled: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Stall));
                        let delay = Duration::from_millis(300);
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, stalled],
                            delay,
                        )
                        .await;

                        let started = std::time::Instant::now();
                        let metadata = composite
                            .get_metadata(repository, address)
                            .await
                            .expect("get_metadata should succeed via durable");

                        assert!(
                            started.elapsed() >= delay,
                            "durable get_metadata must wait out the delay while a replica is outstanding"
                        );
                        assert_eq!(metadata.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica whose read fails counts as having finished, so the durable `get_metadata`
            /// starts without waiting out the delay once the other replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_fails() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let failing: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Fail));
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, failing],
                            LONG_DELAY,
                        )
                        .await;

                        let metadata = tokio::time::timeout(
                            PROMPTLY,
                            composite.get_metadata(repository, address),
                        )
                        .await
                        .expect("get_metadata should not wait out the durable delay")
                        .expect("get_metadata should succeed via durable");

                        assert_eq!(metadata.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica whose read task panics counts as having finished, so the durable
            /// `get_metadata` starts without waiting out the delay once the other replica has
            /// missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_panics() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let panicking: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Panic));
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, panicking],
                            LONG_DELAY,
                        )
                        .await;

                        let metadata = tokio::time::timeout(
                            PROMPTLY,
                            composite.get_metadata(repository, address),
                        )
                        .await
                        .expect("get_metadata should not wait out the durable delay")
                        .expect("get_metadata should succeed via durable");

                        assert_eq!(metadata.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica that answers `SlowDown` counts as having finished, so the durable
            /// `get_metadata` starts without waiting out the delay once the other replica has
            /// missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_slows_down() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let throttled: Arc<dyn ImmutableStore> = Arc::new(
                            CountingStore::with_reads(make_store().await, Reads::SlowDown),
                        );
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, throttled],
                            LONG_DELAY,
                        )
                        .await;

                        let metadata = tokio::time::timeout(
                            PROMPTLY,
                            composite.get_metadata(repository, address),
                        )
                        .await
                        .expect("get_metadata should not wait out the durable delay")
                        .expect("get_metadata should succeed via durable");

                        assert_eq!(metadata.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// When a read replica responds with a full match before the durable delay elapses,
            /// the durable `get_metadata` is never started.
            #[tokio::test]
            async fn durable_not_called_when_replica_responds_within_delay() {
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
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![replica],
                            LONG_DELAY,
                        )
                        .await;

                        let metadata = composite
                            .get_metadata(repository, address)
                            .await
                            .expect("get_metadata should succeed via replica");

                        assert_eq!(metadata.match_made, StoreMatch::MatchFull);
                        // Gives a durable query spawned by mistake time to reach the store.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            /// When there are no replicas the durable `get_metadata` starts without waiting out the
            /// delay.
            #[tokio::test]
            async fn durable_not_delayed_when_no_replicas() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let composite =
                            build_composite_no_replica(durable.clone(), LONG_DELAY).await;

                        let metadata = tokio::time::timeout(
                            PROMPTLY,
                            composite.get_metadata(repository, address),
                        )
                        .await
                        .expect("get_metadata should not wait out the durable delay")
                        .expect("get_metadata should succeed via durable");

                        assert_eq!(metadata.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }
        }

        mod query {
            use super::*;

            /// When a read replica responds with `MatchFull` before the durable delay elapses,
            /// the durable `query` is never started.
            #[tokio::test]
            async fn durable_not_called_when_replica_responds_within_delay() {
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

                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![replica],
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
                        // Gives a durable query spawned by mistake time to reach the store.
                        tokio::time::sleep(Duration::from_millis(20)).await;
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
            async fn durable_not_delayed_when_no_replicas() {
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

                        let composite =
                            build_composite_no_replica(durable.clone(), LONG_DELAY).await;

                        let match_result = tokio::time::timeout(
                            PROMPTLY,
                            lore_storage::immutable_store::query_one(
                                &(composite as Arc<dyn ImmutableStore>),
                                repository,
                                address,
                            ),
                        )
                        .await
                        .expect("query should not wait out the durable delay")
                        .expect("query should succeed via durable");

                        assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                        assert_eq!(
                            durable.resolve_count.load(Ordering::SeqCst),
                            1,
                            "durable resolve must be called when there are no replicas"
                        );
                    })
                    .await;
            }

            /// Once every read replica has missed, the durable `query` starts without waiting out
            /// the delay.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            missing_replicas().await,
                            LONG_DELAY,
                        )
                        .await;

                        let match_result = tokio::time::timeout(
                            PROMPTLY,
                            lore_storage::immutable_store::query_one(
                                &(composite as Arc<dyn ImmutableStore>),
                                repository,
                                address,
                            ),
                        )
                        .await
                        .expect("query should not wait out the durable delay")
                        .expect("query should succeed via durable");

                        assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// While any read replica is still working on a request, the durable `query` waits out
            /// the delay, even after the other replicas have missed.
            #[tokio::test]
            async fn durable_waits_out_delay_while_a_replica_is_outstanding() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let stalled: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Stall));
                        let delay = Duration::from_millis(300);
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, stalled],
                            delay,
                        )
                        .await;

                        let started = std::time::Instant::now();
                        let match_result = lore_storage::immutable_store::query_one(
                            &(composite as Arc<dyn ImmutableStore>),
                            repository,
                            address,
                        )
                        .await
                        .expect("query should succeed via durable");

                        assert!(
                            started.elapsed() >= delay,
                            "durable query must wait out the delay while a replica is outstanding"
                        );
                        assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica whose read fails counts as having finished, so the durable `query` starts
            /// without waiting out the delay once the other replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_fails() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let failing: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Fail));
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, failing],
                            LONG_DELAY,
                        )
                        .await;

                        let match_result = tokio::time::timeout(
                            PROMPTLY,
                            lore_storage::immutable_store::query_one(
                                &(composite as Arc<dyn ImmutableStore>),
                                repository,
                                address,
                            ),
                        )
                        .await
                        .expect("query should not wait out the durable delay")
                        .expect("query should succeed via durable");

                        assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica whose read task panics counts as having finished, so the durable `query`
            /// starts without waiting out the delay once the other replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_panics() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let panicking: Arc<dyn ImmutableStore> =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Panic));
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, panicking],
                            LONG_DELAY,
                        )
                        .await;

                        let match_result = tokio::time::timeout(
                            PROMPTLY,
                            lore_storage::immutable_store::query_one(
                                &(composite as Arc<dyn ImmutableStore>),
                                repository,
                                address,
                            ),
                        )
                        .await
                        .expect("query should not wait out the durable delay")
                        .expect("query should succeed via durable");

                        assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A replica that answers `SlowDown` counts as having finished, so the durable `query`
            /// starts without waiting out the delay once the other replica has missed.
            #[tokio::test]
            async fn durable_starts_once_every_replica_misses_or_slows_down() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let throttled: Arc<dyn ImmutableStore> = Arc::new(
                            CountingStore::with_reads(make_store().await, Reads::SlowDown),
                        );
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            vec![make_store().await, throttled],
                            LONG_DELAY,
                        )
                        .await;

                        let match_result = tokio::time::timeout(
                            PROMPTLY,
                            lore_storage::immutable_store::query_one(
                                &(composite as Arc<dyn ImmutableStore>),
                                repository,
                                address,
                            ),
                        )
                        .await
                        .expect("query should not wait out the durable delay")
                        .expect("query should succeed via durable");

                        assert_eq!(match_result.match_made, StoreMatch::MatchFull);
                        assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }
        }

        mod read_replica_selection {
            use std::collections::HashMap;
            use std::num::NonZeroUsize;

            use async_trait::async_trait;
            use lore_revision::cluster::peer::PeerInfo;
            use lore_revision::store::composite::DEFAULT_ROUND_ROBIN_MAX_TRIES;
            use lore_revision::store::composite::ReadReplicaSelection;
            use lore_revision::store::composite::ReplicationTarget;
            use lore_revision::store::composite::replica_factory::ReplicaFactory;
            use lore_revision::store::composite::replica_factory::ReplicaTargets;

            use super::*;
            use crate::tests::TestStore;
            use crate::tests::topology::create_peer_info;

            /// Enough read replicas that a read sent to every one of them is told apart from a
            /// read sent to some.
            const NUM_REPLICAS: usize = 3;

            /// Long against the time a test store takes to answer, so a try that waits it out is
            /// told apart from one that does not.
            const TRY_LIMIT: Duration = Duration::from_millis(200);

            fn default_max_tries() -> u32 {
                DEFAULT_ROUND_ROBIN_MAX_TRIES.get() as u32
            }

            async fn counting_replicas(reads: Reads) -> Vec<Arc<CountingStore>> {
                let mut replicas = Vec::with_capacity(NUM_REPLICAS);
                for _ in 0..NUM_REPLICAS {
                    replicas.push(Arc::new(CountingStore::with_reads(
                        make_store().await,
                        reads,
                    )));
                }
                replicas
            }

            /// Replicas that hold the content and follow `sequence`.
            async fn sequenced_replicas(
                repository: Partition,
                address: Address,
                fragment: Fragment,
                payload: Bytes,
                sequence: &Arc<ReadSequence>,
            ) -> Vec<Arc<dyn ImmutableStore>> {
                let mut replicas = Vec::with_capacity(NUM_REPLICAS);
                for index in 0..NUM_REPLICAS {
                    let inner = make_store().await;
                    inner
                        .clone()
                        .put(repository, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("put to replica failed");
                    replicas.push(
                        Arc::new(CountingStore::sequenced(inner, index, sequence.clone()))
                            as Arc<dyn ImmutableStore>,
                    );
                }
                replicas
            }

            fn as_stores(replicas: &[Arc<CountingStore>]) -> Vec<Arc<dyn ImmutableStore>> {
                replicas
                    .iter()
                    .map(|replica| replica.clone() as Arc<dyn ImmutableStore>)
                    .collect()
            }

            /// The requests each replica received, as `count` names them.
            fn requests(
                replicas: &[Arc<CountingStore>],
                count: fn(&CountingStore) -> &AtomicU32,
            ) -> Vec<u32> {
                replicas
                    .iter()
                    .map(|replica| count(replica).load(Ordering::SeqCst))
                    .collect()
            }

            fn total_requests(
                replicas: &[Arc<CountingStore>],
                count: fn(&CountingStore) -> &AtomicU32,
            ) -> u32 {
                requests(replicas, count).iter().sum()
            }

            /// Checks that `sequence` saw `tries` requests, each to the replica after the one
            /// before it in list order.
            fn assert_consecutive_tries(sequence: &ReadSequence, tries: usize) {
                let tried: Vec<usize> = sequence
                    .requests()
                    .iter()
                    .map(|(index, _)| *index)
                    .collect();
                assert_eq!(tried.len(), tries, "tried {tried:?}");
                for pair in tried.windows(2) {
                    assert_eq!(pair[1], (pair[0] + 1) % NUM_REPLICAS, "tried {tried:?}");
                }
            }

            async fn round_robin_builder(
                durable: Arc<dyn ImmutableStore>,
                replicas: Vec<Arc<dyn ImmutableStore>>,
                durable_delay: Duration,
            ) -> CompositeStoreBuilder {
                builder_with_read_replicas(durable, replicas, durable_delay)
                    .await
                    .with_read_replica_selection(ReadReplicaSelection::RoundRobin)
            }

            /// A round robin composite whose durable reads wait `LONG_DELAY` for the replicas, so a
            /// read that completes within `PROMPTLY` did not wait for the delay.
            async fn round_robin_over(
                durable: Arc<dyn ImmutableStore>,
                replicas: Vec<Arc<dyn ImmutableStore>>,
            ) -> Arc<CompositeStore> {
                Arc::new(
                    round_robin_builder(durable, replicas, LONG_DELAY)
                        .await
                        .build()
                        .expect("build should work"),
                )
            }

            /// As [`round_robin_over`], with tries that wait `TRY_LIMIT` each.
            async fn round_robin_with_try_limit(
                durable: Arc<dyn ImmutableStore>,
                replicas: Vec<Arc<dyn ImmutableStore>>,
                durable_delay: Duration,
            ) -> Arc<CompositeStore> {
                Arc::new(
                    round_robin_builder(durable, replicas, durable_delay)
                        .await
                        .with_round_robin_tries(None, Some(TRY_LIMIT))
                        .build()
                        .expect("build should work"),
                )
            }

            async fn get_promptly(
                composite: &Arc<CompositeStore>,
                repository: Partition,
                address: Address,
            ) -> Bytes {
                let (_, payload) =
                    tokio::time::timeout(PROMPTLY, composite.clone().get(repository, address))
                        .await
                        .expect("get should not wait out the durable delay")
                        .and_then(StoreGetData::into_payload)
                        .expect("get should succeed");
                payload
            }

            async fn get_metadata_promptly(
                composite: &Arc<CompositeStore>,
                repository: Partition,
                address: Address,
            ) -> StoreMatch {
                tokio::time::timeout(
                    PROMPTLY,
                    composite.clone().get_metadata(repository, address),
                )
                .await
                .expect("get_metadata should not wait out the durable delay")
                .expect("get_metadata should succeed")
                .match_made
            }

            async fn query_promptly(
                composite: &Arc<CompositeStore>,
                repository: Partition,
                address: Address,
            ) -> StoreMatch {
                tokio::time::timeout(
                    PROMPTLY,
                    lore_storage::immutable_store::query_one(
                        &(composite.clone() as Arc<dyn ImmutableStore>),
                        repository,
                        address,
                    ),
                )
                .await
                .expect("query should not wait out the durable delay")
                .expect("query should succeed")
                .match_made
            }

            /// A read that has no round robin tries, such as every fan-out read, must not carry
            /// their state and timer, so an operation holds them boxed: it is smaller than its own
            /// timer and the tries together.
            #[test]
            fn an_operation_does_not_hold_its_round_robin_tries() {
                use lore_revision::store::composite::CompositeOperation;
                use lore_revision::store::composite::Tries;

                type Query = std::future::Ready<Result<(), StoreError>>;
                type Builder = fn(Arc<dyn ImmutableStore>) -> Query;

                let operation = size_of::<CompositeOperation<(), Query, Builder>>();
                let timer = size_of::<Option<tokio::time::Sleep>>();
                let tries = size_of::<Tries<Builder>>();
                assert!(
                    operation < timer + tries,
                    "the operation holds {operation} bytes, its timer {timer} and the tries {tries}"
                );
            }

            #[tokio::test]
            async fn fan_out_get_sends_one_request_to_every_replica() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let replicas = counting_replicas(Reads::Miss).await;
                        let composite = build_composite_with_read_replicas(
                            durable.clone(),
                            as_stores(&replicas),
                            LONG_DELAY,
                        )
                        .await;

                        assert_eq!(get_promptly(&composite, repository, address).await, payload);
                        assert_eq!(
                            requests(&replicas, |replica| &replica.get_count),
                            vec![1; NUM_REPLICAS]
                        );
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_get_that_misses_sends_max_tries_replica_requests() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let replicas = counting_replicas(Reads::Miss).await;
                        let composite =
                            round_robin_over(durable.clone(), as_stores(&replicas)).await;

                        assert_eq!(get_promptly(&composite, repository, address).await, payload);
                        assert_eq!(
                            total_requests(&replicas, |replica| &replica.get_count),
                            default_max_tries()
                        );
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_get_metadata_that_misses_sends_max_tries_replica_requests() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let replicas = counting_replicas(Reads::Miss).await;
                        let composite =
                            round_robin_over(durable.clone(), as_stores(&replicas)).await;

                        assert_eq!(
                            get_metadata_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );
                        assert_eq!(
                            total_requests(&replicas, |replica| &replica.get_metadata_count),
                            default_max_tries()
                        );
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_query_that_misses_sends_max_tries_replica_requests() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let replicas = counting_replicas(Reads::Miss).await;
                        let composite =
                            round_robin_over(durable.clone(), as_stores(&replicas)).await;

                        assert_eq!(
                            query_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );
                        assert_eq!(
                            total_requests(&replicas, |replica| &replica.resolve_count),
                            default_max_tries()
                        );
                        assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// The `get_metadata` a durable full match starts to fill the local cache is a read of
            /// its own, and follows the same selection.
            #[tokio::test]
            async fn round_robin_query_cache_fill_sends_max_tries_replica_requests() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let replicas = counting_replicas(Reads::Miss).await;
                        let composite = Arc::new(
                            round_robin_builder(durable.clone(), as_stores(&replicas), LONG_DELAY)
                                .await
                                .with_cache_metadata(true, None)
                                .build()
                                .expect("build should work"),
                        );

                        assert_eq!(
                            query_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );
                        // The durable store is asked only once every try has missed, so its count
                        // marks the cache fill as past the replica stage.
                        tokio::time::timeout(PROMPTLY, async {
                            while durable.get_metadata_count.load(Ordering::SeqCst) == 0 {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                        })
                        .await
                        .expect("the cache fill should reach the durable store");

                        assert_eq!(
                            total_requests(&replicas, |replica| &replica.resolve_count),
                            default_max_tries()
                        );
                        assert_eq!(
                            total_requests(&replicas, |replica| &replica.get_metadata_count),
                            default_max_tries()
                        );
                    })
                    .await;
            }

            /// Each read takes one turn of the rotation, so reads that each settle on their first
            /// try start at successive replicas.
            #[tokio::test]
            async fn round_robin_spreads_first_tries_evenly_over_an_unchanged_list() {
                const ROTATIONS: u32 = 4;
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let mut replicas = Vec::with_capacity(NUM_REPLICAS);
                        for _ in 0..NUM_REPLICAS {
                            let inner = make_store().await;
                            inner
                                .clone()
                                .put(repository, address, fragment, Some(payload.clone()), false)
                                .await
                                .expect("put to replica failed");
                            replicas.push(Arc::new(CountingStore::wrapping(inner)));
                        }
                        let composite =
                            round_robin_over(durable.clone(), as_stores(&replicas)).await;

                        for _ in 0..ROTATIONS as usize * NUM_REPLICAS {
                            assert_eq!(
                                get_metadata_promptly(&composite, repository, address).await,
                                StoreMatch::MatchFull
                            );
                        }

                        assert_eq!(
                            requests(&replicas, |replica| &replica.get_metadata_count),
                            vec![ROTATIONS; NUM_REPLICAS]
                        );
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_get_tries_the_next_replica_after_a_miss() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let sequence = ReadSequence::new(vec![
                            (Duration::ZERO, Reads::Miss),
                            (Duration::ZERO, Reads::Answer),
                        ]);
                        let replicas = sequenced_replicas(
                            repository,
                            address,
                            fragment,
                            payload.clone(),
                            &sequence,
                        )
                        .await;
                        let composite = round_robin_over(durable.clone(), replicas).await;

                        assert_eq!(get_promptly(&composite, repository, address).await, payload);
                        assert_consecutive_tries(&sequence, 2);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_get_metadata_tries_the_next_replica_after_a_failure_or_partial_match()
             {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        for first in [Reads::Fail, Reads::Partial] {
                            let (fragment, address, payload) = generate_random();
                            let repository: Partition = random::<RepositoryId>();
                            let durable =
                                seeded_durable(repository, address, fragment, payload.clone())
                                    .await;
                            let sequence = ReadSequence::new(vec![
                                (Duration::ZERO, first),
                                (Duration::ZERO, Reads::Answer),
                            ]);
                            let replicas = sequenced_replicas(
                                repository, address, fragment, payload, &sequence,
                            )
                            .await;
                            let composite = round_robin_over(durable.clone(), replicas).await;

                            assert_eq!(
                                get_metadata_promptly(&composite, repository, address).await,
                                StoreMatch::MatchFull
                            );
                            assert_consecutive_tries(&sequence, 2);
                            assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 0);
                        }
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_query_tries_the_next_replica_after_a_partial_match() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let sequence = ReadSequence::new(vec![
                            (Duration::ZERO, Reads::Partial),
                            (Duration::ZERO, Reads::Answer),
                        ]);
                        let replicas =
                            sequenced_replicas(repository, address, fragment, payload, &sequence)
                                .await;
                        let composite = round_robin_over(durable.clone(), replicas).await;

                        assert_eq!(
                            query_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );
                        assert_consecutive_tries(&sequence, 2);
                        assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_next_try_starts_once_the_try_limit_passes() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let sequence = ReadSequence::new(vec![
                            (LONG_DELAY, Reads::Miss),
                            (Duration::ZERO, Reads::Answer),
                        ]);
                        let replicas =
                            sequenced_replicas(repository, address, fragment, payload, &sequence)
                                .await;
                        let composite =
                            round_robin_with_try_limit(durable.clone(), replicas, LONG_DELAY).await;

                        let started = std::time::Instant::now();
                        assert_eq!(
                            get_metadata_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );

                        assert_consecutive_tries(&sequence, 2);
                        let second_try = sequence.requests()[1].1;
                        assert!(
                            second_try.duration_since(started) >= TRY_LIMIT,
                            "the second try must wait out the first try's limit"
                        );
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            /// Long enough that a late result, which a panic delays by its report, still arrives
            /// well before the next try is due.
            const PASSED_OVER_LIMIT: Duration = Duration::from_millis(500);

            /// A round robin composite that tries every replica, each for `PASSED_OVER_LIMIT`.
            async fn round_robin_trying_every_replica(
                durable: Arc<dyn ImmutableStore>,
                replicas: Vec<Arc<dyn ImmutableStore>>,
            ) -> Arc<CompositeStore> {
                Arc::new(
                    round_robin_builder(durable, replicas, LONG_DELAY)
                        .await
                        .with_round_robin_tries(
                            NonZeroUsize::new(NUM_REPLICAS),
                            Some(PASSED_OVER_LIMIT),
                        )
                        .build()
                        .expect("build should work"),
                )
            }

            /// Reads with the first try ending as `first` after it has passed its limit, the second
            /// stalled and the third answering, and returns when the third try started, from the
            /// start of the read.
            async fn third_try_start_after_a_late_first_try(first: Reads) -> Duration {
                let (fragment, address, payload) = generate_random();
                let repository: Partition = random::<RepositoryId>();
                let durable = seeded_durable(repository, address, fragment, payload.clone()).await;
                let sequence = ReadSequence::new(vec![
                    (PASSED_OVER_LIMIT * 6 / 5, first),
                    (Duration::ZERO, Reads::Stall),
                    (Duration::ZERO, Reads::Answer),
                ]);
                let replicas =
                    sequenced_replicas(repository, address, fragment, payload, &sequence).await;
                let composite = round_robin_trying_every_replica(durable.clone(), replicas).await;

                let started = std::time::Instant::now();
                assert_eq!(
                    get_metadata_promptly(&composite, repository, address).await,
                    StoreMatch::MatchFull
                );
                assert_consecutive_tries(&sequence, 3);
                assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 0);
                sequence.requests()[2].1.duration_since(started)
            }

            /// Only the current try's result moves the read on: a try past its limit that misses
            /// later leaves the next try to the current try's limit.
            #[tokio::test]
            async fn round_robin_late_miss_of_a_passed_over_try_does_not_start_the_next() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let third = third_try_start_after_a_late_first_try(Reads::Miss).await;
                        assert!(
                            third >= PASSED_OVER_LIMIT * 2,
                            "the third try started after {third:?}"
                        );
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_late_panic_of_a_passed_over_try_does_not_start_the_next() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let third = third_try_start_after_a_late_first_try(Reads::Panic).await;
                        assert!(
                            third >= PASSED_OVER_LIMIT * 2,
                            "the third try started after {third:?}"
                        );
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_panic_of_the_current_try_starts_the_next() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let sequence = ReadSequence::new(vec![
                            (Duration::ZERO, Reads::Panic),
                            (Duration::ZERO, Reads::Answer),
                        ]);
                        let replicas =
                            sequenced_replicas(repository, address, fragment, payload, &sequence)
                                .await;
                        // Neither the try limit nor the durable delay passes within `PROMPTLY`, so
                        // only the panic can start the second try.
                        let composite = Arc::new(
                            round_robin_builder(durable.clone(), replicas, LONG_DELAY)
                                .await
                                .with_round_robin_tries(None, Some(LONG_DELAY))
                                .build()
                                .expect("build should work"),
                        );

                        assert_eq!(
                            get_metadata_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );
                        assert_consecutive_tries(&sequence, 2);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            /// A try past its limit keeps running, and settles the read if it answers first.
            #[tokio::test]
            async fn round_robin_slow_try_that_answers_first_settles_the_read() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let sequence = ReadSequence::new(vec![
                            (TRY_LIMIT * 2, Reads::Answer),
                            (Duration::ZERO, Reads::Stall),
                        ]);
                        let replicas =
                            sequenced_replicas(repository, address, fragment, payload, &sequence)
                                .await;
                        let composite =
                            round_robin_with_try_limit(durable.clone(), replicas, LONG_DELAY).await;

                        assert_eq!(
                            get_metadata_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );
                        assert_consecutive_tries(&sequence, 2);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 0);
                    })
                    .await;
            }

            /// The durable delay counts from the start of the read, so the durable read starts
            /// once it passes even while tries are still running.
            #[tokio::test]
            async fn round_robin_durable_starts_at_the_delay_while_tries_run() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let sequence = ReadSequence::new(vec![
                            (Duration::ZERO, Reads::Stall),
                            (Duration::ZERO, Reads::Stall),
                        ]);
                        let replicas =
                            sequenced_replicas(repository, address, fragment, payload, &sequence)
                                .await;
                        let durable_delay = TRY_LIMIT * 2;
                        let composite =
                            round_robin_with_try_limit(durable.clone(), replicas, durable_delay)
                                .await;

                        let started = std::time::Instant::now();
                        assert_eq!(
                            get_metadata_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );

                        assert!(started.elapsed() >= durable_delay);
                        assert_consecutive_tries(&sequence, 2);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_tries_are_capped_at_the_replica_count() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let replica =
                            Arc::new(CountingStore::with_reads(make_store().await, Reads::Miss));
                        let composite = Arc::new(
                            round_robin_builder(durable.clone(), vec![replica.clone()], LONG_DELAY)
                                .await
                                .with_round_robin_tries(NonZeroUsize::new(NUM_REPLICAS), None)
                                .build()
                                .expect("build should work"),
                        );

                        assert_eq!(
                            get_metadata_promptly(&composite, repository, address).await,
                            StoreMatch::MatchFull
                        );
                        assert_eq!(replica.get_metadata_count.load(Ordering::SeqCst), 1);
                        assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// Builds a read replica for each peer that misses every read and counts its reads.
            #[derive(Debug, Default)]
            struct CountingReplicaBuilder {
                replicas: std::sync::Mutex<HashMap<String, Arc<CountingStore>>>,
            }

            impl CountingReplicaBuilder {
                /// The `get_metadata` reads each peer's replica has received, by peer.
                fn requests(&self) -> HashMap<String, u32> {
                    self.replicas
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(peer, replica)| {
                            (
                                peer.clone(),
                                replica.get_metadata_count.load(Ordering::SeqCst),
                            )
                        })
                        .collect()
                }
            }

            #[async_trait]
            impl ReplicaFactory for CountingReplicaBuilder {
                async fn make_replica_target(
                    &self,
                    peer_info: &PeerInfo,
                ) -> Result<ReplicaTargets, Box<dyn std::error::Error + Send + Sync>>
                {
                    let replica =
                        Arc::new(CountingStore::with_reads(make_store().await, Reads::Miss));
                    self.replicas
                        .lock()
                        .unwrap()
                        .insert(peer_info.id.clone(), replica.clone());
                    Ok(ReplicaTargets {
                        read: Some(ReplicationTarget::new(peer_info.clone(), replica)),
                        write: None,
                    })
                }
            }

            /// A refresh removes lost peers and appends new ones, so reads after it go only to the
            /// peers then in the list.
            #[tokio::test]
            async fn round_robin_reads_follow_peer_changes() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable = seeded_durable(repository, address, fragment, payload).await;
                        let builder = Arc::new(CountingReplicaBuilder::default());
                        let composite = Arc::new(
                            round_robin_builder(durable.clone(), Vec::new(), LONG_DELAY)
                                .await
                                .with_replica_builder(builder.clone())
                                .build()
                                .expect("build should work"),
                        );
                        let read = async |times: usize| {
                            for _ in 0..times {
                                assert_eq!(
                                    get_metadata_promptly(&composite, repository, address).await,
                                    StoreMatch::MatchFull
                                );
                            }
                        };
                        let peers: Vec<PeerInfo> = (1..=6)
                            .map(|index| create_peer_info(&format!("peer-{index}")))
                            .collect();
                        let ids = |indices: &[usize]| -> Vec<String> {
                            indices
                                .iter()
                                .map(|&index| peers[index].id.clone())
                                .collect()
                        };
                        let refresh = async |indices: &[usize]| {
                            composite
                                .topology_peers_refreshed(
                                    indices.iter().map(|&index| peers[index].clone()).collect(),
                                )
                                .await
                                .expect("refresh should have worked");
                        };

                        refresh(&[0, 1, 2, 3]).await;
                        read(3).await;
                        let before = builder.requests();

                        // One peer left: each read tries it once.
                        refresh(&[0]).await;
                        read(2).await;
                        let after_removal = builder.requests();
                        for peer in ids(&[1, 2, 3]) {
                            assert_eq!(after_removal[&peer], before[&peer], "{peer} was removed");
                        }
                        assert_eq!(after_removal[&peers[0].id], before[&peers[0].id] + 2);

                        // Three peers: six reads of two tries each start at successive positions,
                        // so each peer is tried four times, whatever the start.
                        refresh(&[0, 4, 5]).await;
                        read(6).await;
                        let after_addition = builder.requests();
                        for peer in ids(&[1, 2, 3]) {
                            assert_eq!(after_addition[&peer], before[&peer], "{peer} was removed");
                        }
                        assert_eq!(
                            after_addition[&peers[0].id],
                            after_removal[&peers[0].id] + 4
                        );
                        for peer in ids(&[4, 5]) {
                            assert_eq!(after_addition[&peer], 4, "{peer} was added");
                        }
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_without_read_replicas_reads_from_durable() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        let (fragment, address, payload) = generate_random();
                        let repository: Partition = random::<RepositoryId>();
                        let durable =
                            seeded_durable(repository, address, fragment, payload.clone()).await;
                        let composite = round_robin_over(durable.clone(), Vec::new()).await;

                        assert_eq!(get_promptly(&composite, repository, address).await, payload);
                        assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                    })
                    .await;
            }

            /// A `get` takes any answer a replica gives, so only a miss or a failure leaves it to
            /// the durable store.
            #[tokio::test]
            async fn round_robin_get_falls_back_promptly_when_the_replicas_miss_or_fail() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        for reads in [Reads::Miss, Reads::Fail] {
                            let (fragment, address, payload) = generate_random();
                            let repository: Partition = random::<RepositoryId>();
                            let durable =
                                seeded_durable(repository, address, fragment, payload.clone())
                                    .await;
                            let replicas = counting_replicas(reads).await;
                            let composite =
                                round_robin_over(durable.clone(), as_stores(&replicas)).await;

                            assert_eq!(
                                get_promptly(&composite, repository, address).await,
                                payload
                            );
                            assert_eq!(durable.get_count.load(Ordering::SeqCst), 1);
                        }
                    })
                    .await;
            }

            /// Replicas that each miss, partially match or fail, so every try does whichever it
            /// is.
            async fn unhelpful_replicas() -> Vec<Vec<Arc<dyn ImmutableStore>>> {
                let partial = (0..NUM_REPLICAS)
                    .map(|_| {
                        Arc::new(TestStore::succeeding().with_mock_match(StoreMatch::MatchHash))
                            as Arc<dyn ImmutableStore>
                    })
                    .collect();
                vec![
                    as_stores(&counting_replicas(Reads::Miss).await),
                    partial,
                    as_stores(&counting_replicas(Reads::Fail).await),
                ]
            }

            #[tokio::test]
            async fn round_robin_get_metadata_falls_back_promptly_when_no_try_settles_it() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        for replicas in unhelpful_replicas().await {
                            let (fragment, address, payload) = generate_random();
                            let repository: Partition = random::<RepositoryId>();
                            let durable =
                                seeded_durable(repository, address, fragment, payload).await;
                            let composite = round_robin_over(durable.clone(), replicas).await;

                            assert_eq!(
                                get_metadata_promptly(&composite, repository, address).await,
                                StoreMatch::MatchFull
                            );
                            assert_eq!(durable.get_metadata_count.load(Ordering::SeqCst), 1);
                        }
                    })
                    .await;
            }

            #[tokio::test]
            async fn round_robin_query_falls_back_promptly_when_no_try_settles_it() {
                let execution = setup_test_execution();
                LORE_CONTEXT
                    .scope(execution, async move {
                        for replicas in unhelpful_replicas().await {
                            let (fragment, address, payload) = generate_random();
                            let repository: Partition = random::<RepositoryId>();
                            let durable =
                                seeded_durable(repository, address, fragment, payload).await;
                            let composite = round_robin_over(durable.clone(), replicas).await;

                            assert_eq!(
                                query_promptly(&composite, repository, address).await,
                                StoreMatch::MatchFull
                            );
                            assert_eq!(durable.resolve_count.load(Ordering::SeqCst), 1);
                        }
                    })
                    .await;
            }
        }
    }
}
