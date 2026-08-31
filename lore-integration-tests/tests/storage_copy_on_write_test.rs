// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the write path duplicating an association instead of transferring a
//! payload.
//!
//! Content the peer already holds under another context or another partition needs an association,
//! not an upload. The write path establishes that from the resolution it already runs per fragment
//! and issues `Copy` where it would have issued `Put`. These tests spin up a real gRPC server whose
//! store counts what it was asked to do, so "no payload was transferred" is observed at the peer
//! rather than inferred from the client.

#[cfg(all(test, feature = "integration_tests"))]
mod storage_copy_on_write_tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use bytes::Bytes;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::Partition;
    use lore_revision::environment::EnvironmentConfig;
    use lore_revision::event::LoreBytes;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_server::grpc::server::FeatureSettings;
    use lore_server::grpc::server::GrpcServerBuilder;
    use lore_server::hooks::HookDispatcher;
    use lore_storage::ImmutableStore;
    use lore_storage::StoreError;
    use lore_storage::StoreGetData;
    use lore_storage::StoreMatch;
    use lore_storage::StoreMatchResult;
    use lore_storage::StoreObliterateStats;
    use lore_storage::immutable_store::query_one;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;

    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    /// What the peer was asked to do, so a test can say a payload never crossed the wire.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct Traffic {
        payload_puts: usize,
        empty_puts: usize,
        copies: usize,
    }

    /// Counts the write verbs reaching the served store, and optionally refuses `copy` so the
    /// caller's fallback can be exercised against a peer that will not answer one.
    struct CountingStore {
        inner: Arc<dyn ImmutableStore>,
        payload_puts: AtomicUsize,
        empty_puts: AtomicUsize,
        copies: AtomicUsize,
        copy_sources: Mutex<Vec<(Partition, Address)>>,
        refuse_copy: bool,
    }

    impl CountingStore {
        fn wrapping(inner: Arc<dyn ImmutableStore>, refuse_copy: bool) -> Arc<Self> {
            Arc::new(Self {
                inner,
                payload_puts: AtomicUsize::new(0),
                empty_puts: AtomicUsize::new(0),
                copies: AtomicUsize::new(0),
                copy_sources: Mutex::new(Vec::new()),
                refuse_copy,
            })
        }

        fn new(inner: Arc<dyn ImmutableStore>) -> Arc<Self> {
            Self::wrapping(inner, false)
        }

        fn refusing_copy(inner: Arc<dyn ImmutableStore>) -> Arc<Self> {
            Self::wrapping(inner, true)
        }

        fn traffic(&self) -> Traffic {
            Traffic {
                payload_puts: self.payload_puts.load(Ordering::SeqCst),
                empty_puts: self.empty_puts.load(Ordering::SeqCst),
                copies: self.copies.load(Ordering::SeqCst),
            }
        }

        fn reset(&self) {
            self.payload_puts.store(0, Ordering::SeqCst);
            self.empty_puts.store(0, Ordering::SeqCst);
            self.copies.store(0, Ordering::SeqCst);
            self.copy_sources.lock().unwrap().clear();
        }

        fn copy_sources(&self) -> Vec<(Partition, Address)> {
            self.copy_sources.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ImmutableStore for CountingStore {
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
            if payload.is_some() {
                self.payload_puts.fetch_add(1, Ordering::SeqCst);
            } else {
                self.empty_puts.fetch_add(1, Ordering::SeqCst);
            }
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
            self.copies.fetch_add(1, Ordering::SeqCst);
            self.copy_sources
                .lock()
                .unwrap()
                .push((source_partition, source_address));
            if self.refuse_copy {
                return Err(StoreError::from(lore_base::error::AddressNotFound::from(
                    source_address,
                )));
            }
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

    struct TestServer {
        url: String,
        counted: Arc<CountingStore>,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    async fn start_server() -> TestServer {
        start_server_with(CountingStore::new).await
    }

    async fn start_refusing_server() -> TestServer {
        start_server_with(CountingStore::refusing_copy).await
    }

    async fn start_server_with(
        wrap: impl FnOnce(Arc<dyn ImmutableStore>) -> Arc<CountingStore>,
    ) -> TestServer {
        // The served store isolates partitions and implies durability, as a real server's does:
        // both decide what the client is told about content it did not store itself.
        let backend = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                protect_local_fragment: false,
                implicit_durable_stored: true,
                isolate_partitions: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            backend.clone(),
        )
        .await
        .unwrap();

        let counted = wrap(backend);
        let served: Arc<dyn ImmutableStore> = counted.clone();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let signal = async {
            shutdown_rx.await.ok();
        };

        let notification_sender: Arc<dyn lore_revision::notification::NotificationSender> =
            Arc::new(lore_server::notification::local::NotificationSender::default());

        let (stopped_tx, mut stopped_rx) = tokio::sync::oneshot::channel::<String>();
        // Background server task in a test; LORE_CONTEXT propagation is unnecessary here.
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            let outcome = GrpcServerBuilder::new()
                .with_environment(EnvironmentConfig::default())
                .with_feature(FeatureSettings::default())
                .with_immutable_store(served.clone(), served)
                .with_mutable_store(mutable)
                .with_lock_store(None)
                .with_notification(notification_sender, None)
                .with_hook_dispatcher(Arc::new(HookDispatcher::empty()))
                .with_tls_config(None, None, None)
                .unwrap()
                .with_admin_endpoints(HashMap::new(), vec![])
                .with_http2_config(
                    None,
                    None,
                    Duration::from_secs(30),
                    None,
                    Default::default(),
                    None,
                )
                .with_jwt_verifier(None)
                .unwrap()
                .serve_with_listener(listener, signal)
                .await;
            let _ = stopped_tx.send(match outcome {
                Ok(()) => "stopped before the test finished".to_string(),
                Err(error) => format!("failed: {error}"),
            });
        });

        let mut ready = false;
        for _ in 0..50 {
            if let Ok(reason) = stopped_rx.try_recv() {
                panic!("test server on {addr} {reason}");
            }
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready, "test server on {addr} never accepted a connection");

        TestServer {
            url: format!("grpc://127.0.0.1:{}", addr.port()),
            counted,
            _shutdown: shutdown_tx,
        }
    }

    async fn open_handle(server: &TestServer) -> u64 {
        let opened: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let sink = opened.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageOpened(data) = event {
                *sink.lock().unwrap() = Some(data.handle_id);
            }
        }));
        let status = lore::storage::open::open(
            LoreGlobalArgs::default(),
            lore::storage::open::LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                remote_config: lore::storage::open::LoreStorageRemoteConfig {
                    remote_url: LoreString::from(server.url.as_str()),
                },
                has_remote_config: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "open with remote_config must succeed");
        opened.lock().unwrap().expect("STORAGE_OPENED must fire")
    }

    async fn close_handle(handle_id: u64) {
        let status = lore::storage::close::close(
            LoreGlobalArgs::default(),
            lore::storage::close::LoreStorageCloseArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
            },
            None,
        )
        .await;
        assert_eq!(status, 0, "close must succeed");
    }

    /// One `lore_storage_put`, returning the address it produced.
    async fn put(
        handle_id: u64,
        partition: Partition,
        context: Context,
        bytes: &[u8],
        remote_write: u8,
        chunk: u64,
    ) -> Address {
        let captured: Arc<Mutex<Vec<(Address, LoreErrorCode)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                sink.lock().unwrap().push((data.address, data.error_code));
            }
        }));
        let status = lore::storage::put::put(
            LoreGlobalArgs::default(),
            lore::storage::put::LoreStoragePutArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![lore::storage::put::LoreStoragePutItem {
                    id: 1,
                    partition,
                    context,
                    data: LoreBytes {
                        ptr: bytes.as_ptr().cast(),
                        len: bytes.len(),
                    },
                    remote_write,
                    local_cache: 0,
                    fixed_size_chunk: chunk,
                }]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "put must succeed");
        let events = captured.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, LoreErrorCode::None);
        events[0].0
    }

    async fn put_remote(
        handle_id: u64,
        partition: Partition,
        context: Context,
        bytes: &[u8],
    ) -> Address {
        put(handle_id, partition, context, bytes, 1, 0).await
    }

    async fn put_local_only(
        handle_id: u64,
        partition: Partition,
        context: Context,
        bytes: &[u8],
    ) -> Address {
        put(handle_id, partition, context, bytes, 0, 0).await
    }

    async fn assert_peer_holds(
        server: &TestServer,
        partition: Partition,
        address: Address,
        payload: &[u8],
    ) {
        let store: Arc<dyn ImmutableStore> = server.counted.clone();
        let resolved = query_one(&store, partition, address)
            .await
            .expect("query the peer");
        assert_eq!(
            resolved.match_made,
            StoreMatch::MatchFull,
            "the peer must hold the association the write registered"
        );

        let (_fragment, bytes) = lore_storage::read::read(
            store,
            partition,
            address,
            None,
            lore_storage::options::ReadOptions::default(),
            None,
        )
        .await
        .expect("read the address back from the peer");
        assert_eq!(
            bytes.as_ref(),
            payload,
            "the copied association must serve the original bytes"
        );
    }

    /// The flagship case: the same bytes written again under a different context in the same
    /// partition. The peer holds the payload already, so it is asked for an association.
    #[tokio::test]
    async fn a_second_context_in_the_same_partition_copies_instead_of_uploading() -> TestResult {
        let execution = setup_execution("copy-on-write-same-partition".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle_id = open_handle(&server).await;

                let partition = Partition::from([0x11u8; 16]);
                let first_context = Context::from([0x12u8; 16]);
                let second_context = Context::from([0x13u8; 16]);
                let payload = b"one payload, two files that happen to be identical".to_vec();

                let first = put_remote(handle_id, partition, first_context, &payload).await;
                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 1,
                        empty_puts: 0,
                        copies: 0
                    },
                    "content the peer has never seen has to be uploaded"
                );

                server.counted.reset();
                let second = put_remote(handle_id, partition, second_context, &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 0,
                        empty_puts: 0,
                        copies: 1
                    },
                    "the peer already holds these bytes, so it must be asked for an association"
                );
                assert_eq!(first.hash, second.hash);
                assert_eq!(second.context, second_context);

                assert_eq!(
                    server.counted.copy_sources(),
                    vec![(
                        partition,
                        Address {
                            hash: first.hash,
                            context: first_context
                        }
                    )],
                    "the source named is the association the client knows the peer holds"
                );

                assert_peer_holds(&server, partition, second, &payload).await;
                assert_peer_holds(&server, partition, first, &payload).await;

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// The same bytes written into a second partition. The source is one the client has a claim to,
    /// so the association can be duplicated across the boundary rather than uploaded again.
    #[tokio::test]
    async fn a_second_partition_copies_from_the_first() -> TestResult {
        let execution = setup_execution("copy-on-write-cross-partition".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle_id = open_handle(&server).await;

                let source_partition = Partition::from([0x21u8; 16]);
                let target_partition = Partition::from([0x22u8; 16]);
                let context = Context::from([0x23u8; 16]);
                let payload = b"shared store, two repositories, one payload".to_vec();

                let first = put_remote(handle_id, source_partition, context, &payload).await;
                server.counted.reset();

                let second = put_remote(handle_id, target_partition, context, &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 0,
                        empty_puts: 0,
                        copies: 1
                    },
                    "a partition the caller can reach is a source, not a reason to re-upload"
                );
                assert_eq!(
                    server.counted.copy_sources(),
                    vec![(
                        source_partition,
                        Address {
                            hash: first.hash,
                            context
                        }
                    )],
                    "the copy must name the partition the client found the content in"
                );

                assert_peer_holds(&server, target_partition, second, &payload).await;
                assert_peer_holds(&server, source_partition, first, &payload).await;

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// A local association the peer never received names no source it could copy from. Skipping the
    /// upload on the strength of it would leave the address registered nowhere.
    #[tokio::test]
    async fn a_source_the_peer_never_received_is_uploaded() -> TestResult {
        let execution = setup_execution("copy-on-write-not-durable".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle_id = open_handle(&server).await;

                let partition = Partition::from([0x31u8; 16]);
                let payload = b"stored locally, never pushed".to_vec();

                put_local_only(handle_id, partition, Context::from([0x32u8; 16]), &payload).await;
                assert_eq!(
                    server.counted.traffic(),
                    Traffic::default(),
                    "a local-only write must not touch the peer at all"
                );

                let second =
                    put_remote(handle_id, partition, Context::from([0x33u8; 16]), &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 1,
                        empty_puts: 0,
                        copies: 0
                    },
                    "the peer holds nothing to copy from, so the payload has to be transferred"
                );
                assert_peer_holds(&server, partition, second, &payload).await;

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// A copy is an attempt, not a commitment. A peer that refuses one leaves the write to upload
    /// exactly as it would have without ever trying.
    #[tokio::test]
    async fn a_refused_copy_falls_back_to_uploading() -> TestResult {
        let execution = setup_execution("copy-on-write-refused".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_refusing_server().await;
                let handle_id = open_handle(&server).await;

                let partition = Partition::from([0x41u8; 16]);
                let payload = b"the peer will not answer a copy for this".to_vec();

                put_remote(handle_id, partition, Context::from([0x42u8; 16]), &payload).await;
                server.counted.reset();

                let second =
                    put_remote(handle_id, partition, Context::from([0x43u8; 16]), &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 1,
                        empty_puts: 0,
                        copies: 1
                    },
                    "the refused copy must be followed by the upload it was standing in for"
                );
                assert_peer_holds(&server, partition, second, &payload).await;

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// An address the peer already holds outright is a full match, which needs neither verb.
    #[tokio::test]
    async fn re_writing_the_same_address_asks_the_peer_for_nothing() -> TestResult {
        let execution = setup_execution("copy-on-write-full-match".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle_id = open_handle(&server).await;

                let partition = Partition::from([0x51u8; 16]);
                let context = Context::from([0x52u8; 16]);
                let payload = b"written once, written again identically".to_vec();

                put_remote(handle_id, partition, context, &payload).await;
                server.counted.reset();

                put_remote(handle_id, partition, context, &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic::default(),
                    "the association already exists, so there is nothing to send or duplicate"
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Content cut into a fragment tree makes the choice per fragment. Every leaf and every list
    /// block is content-addressed, so a second context copies the whole tree.
    #[tokio::test]
    async fn every_fragment_of_a_tree_copies_rather_than_uploads() -> TestResult {
        let execution = setup_execution("copy-on-write-fragmented".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle_id = open_handle(&server).await;

                let partition = Partition::from([0x61u8; 16]);
                let chunk = 64 * 1024u64;
                // Deterministic and non-repeating, so the fragments differ from one another.
                let payload: Vec<u8> = (0..(chunk as usize * 6 + 977))
                    .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
                    .collect();

                let first = put(
                    handle_id,
                    partition,
                    Context::from([0x62u8; 16]),
                    &payload,
                    1,
                    chunk,
                )
                .await;
                let uploaded = server.counted.traffic();
                assert!(
                    uploaded.payload_puts > 6,
                    "the test wants a real tree, got {uploaded:?}"
                );

                server.counted.reset();
                let second = put(
                    handle_id,
                    partition,
                    Context::from([0x63u8; 16]),
                    &payload,
                    1,
                    chunk,
                )
                .await;
                let duplicated = server.counted.traffic();

                assert_eq!(
                    duplicated.payload_puts, 0,
                    "no fragment of an identical tree needs its payload transferred again"
                );
                assert_eq!(
                    duplicated.copies, uploaded.payload_puts,
                    "every fragment the first write uploaded is one the second duplicates"
                );
                assert_eq!(first.hash, second.hash);

                assert_peer_holds(&server, partition, second, &payload).await;

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// The source a copy names is the exact association the client's own store recorded, which is
    /// what lets a peer confirm it with a keyed read instead of searching the partition.
    #[tokio::test]
    async fn the_copy_names_the_exact_association_the_client_knows_about() -> TestResult {
        let execution = setup_execution("copy-on-write-exact-source".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle_id = open_handle(&server).await;

                let partition = Partition::from([0x71u8; 16]);
                let held = Context::from([0x72u8; 16]);
                let payload = b"the source is named, not searched for".to_vec();

                let first = put_remote(handle_id, partition, held, &payload).await;
                server.counted.reset();

                put_remote(handle_id, partition, Context::from([0x73u8; 16]), &payload).await;

                let sources = server.counted.copy_sources();
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].0, partition);
                assert_eq!(sources[0].1.hash, first.hash);
                assert_eq!(
                    sources[0].1.context, held,
                    "an unnamed context would make the peer search the partition instead"
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }
}
