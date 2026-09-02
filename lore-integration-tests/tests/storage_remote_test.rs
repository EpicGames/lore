// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the content-addressed storage API's remote-backed paths.
//!
//! Spin up a real gRPC server backed by a `LocalImmutableStore` in process; open a storage
//! handle with `remote_config` pointing at it; exercise the remote-touching ops. Gated under
//! the `integration_tests` feature so default `cargo test` (which does not start servers)
//! stays fast.

#[cfg(all(test, feature = "integration_tests"))]
mod storage_remote_tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use lore::storage::close;
    use lore::storage::open;
    use lore::storage::open::LoreStorageOpenArgs;
    use lore::storage::open::LoreStorageRemoteConfig;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::environment::EnvironmentConfig;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_server::grpc::server::FeatureSettings;
    use lore_server::grpc::server::GrpcServerBuilder;
    use lore_server::hooks::HookDispatcher;
    use lore_server::quic::quinn::QuinnConfigBuilder;
    use lore_server::quic::quinn::QuinnServer;
    use lore_server::quic::tests::TestHandlerFactory;
    use lore_server::quic::tests::server_certs;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;

    use crate::common::net_common::bind_matched_pair;
    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    /// What a `PUT_ITEM_COMPLETE` callback records per item: `(id, address, code, stored_local,
    /// stored_remote)`.
    type PutOutcomes = Arc<
        Mutex<
            Vec<(
                u64,
                lore_base::types::Address,
                lore_revision::event::LoreErrorCode,
                u8,
                u8,
            )>,
        >,
    >;

    /// What a streaming `GET_DATA` callback records per event: `(offset, bytes)`.
    type StreamChunks = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

    /// Which wire protocol the test server speaks. Every storage test runs against both, because
    /// system-design.md 18.1 makes the two transports one logical surface — a bug in either is a
    /// bug in the operation.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Transport {
        Grpc,
        Quic,
    }

    const TRANSPORTS: [Transport; 2] = [Transport::Grpc, Transport::Quic];

    struct TestServer {
        url: String,
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
        /// Signals the gRPC server to stop serving. Dropping it starts the shutdown.
        grpc_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        /// Fires when the gRPC serve task returns, which is when its listener is closed.
        grpc_stopped: Option<tokio::sync::oneshot::Receiver<String>>,
        /// The QUIC endpoint, on a `lore://` server.
        quic: Option<QuinnServer>,
    }

    impl TestServer {
        /// Stop serving and wait until this server is unreachable.
        ///
        /// Dropping a `TestServer` only *signals* the stop: the gRPC listener closes when its
        /// serve task returns, and QUIC keeps accepting until its endpoint does. A test that
        /// needs the peer gone rather than going has to wait for both, or the request it expects
        /// to fail can still be answered.
        async fn shutdown(mut self) {
            if let Some(quic) = self.quic.take() {
                quic.close().await;
            }
            drop(self.grpc_shutdown.take());
            if let Some(stopped) = self.grpc_stopped.take()
                && tokio::time::timeout(SHUTDOWN_WAIT, stopped).await.is_err()
            {
                panic!("test server at {} did not stop serving", self.url);
            }
        }
    }

    /// Bound on a test server's shutdown. Long enough that a loaded runner is not mistaken for a
    /// server that will not stop.
    const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

    /// A server-side store that refuses to store exactly one leaf fragment.
    ///
    /// Everything else delegates, so a fragmented upload lands with every leaf present but one —
    /// the mixed tree that tells an intersection fold apart from a union.
    ///
    /// The rejected fragment is picked by size: leaves carry a full chunk of content, the list
    /// that roots them does not, so no prediction of upload order is needed.
    /// Wraps a store to interfere with `put` for one content size, so a test can make an upload
    /// fail or hold it long enough that a concurrent writer of the same address is still in flight
    /// when it lands. Other sizes pass straight through.
    struct InterceptPutStore {
        inner: Arc<dyn lore_storage::ImmutableStore>,
        size: u64,
        /// Reject this many `put`s of `size` before letting one through.
        reject_first: usize,
        /// Hold each `put` of `size` this long before deciding, so the guard is held meanwhile.
        delay: Duration,
        /// `put`s of `size` this store was asked for, whether rejected or forwarded.
        seen: std::sync::atomic::AtomicUsize,
        /// `put`s of `size` that reached the inner store.
        forwarded: std::sync::atomic::AtomicUsize,
    }

    impl InterceptPutStore {
        fn new(
            inner: Arc<dyn lore_storage::ImmutableStore>,
            size: u64,
            reject_first: usize,
            delay: Duration,
        ) -> Arc<Self> {
            Arc::new(Self {
                inner,
                size,
                reject_first,
                delay,
                seen: std::sync::atomic::AtomicUsize::new(0),
                forwarded: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn forwarded(&self) -> usize {
            self.forwarded.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl lore_storage::ImmutableStore for InterceptPutStore {
        fn is_local(&self) -> bool {
            self.inner.clone().is_local()
        }

        fn isolates_partitions(&self) -> bool {
            self.inner.isolates_partitions()
        }

        fn read_scope(&self) -> lore_storage::StoreMatch {
            self.inner.read_scope()
        }

        fn query_scope(&self) -> lore_storage::StoreMatch {
            self.inner.query_scope()
        }

        async fn query(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            addresses: &[lore_base::types::Address],
            results: &mut [lore_storage::StoreMatchResult],
        ) -> Result<(), lore_storage::StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn get(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
        ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
            self.inner.clone().get(partition, address).await
        }

        /// Forwarded explicitly, as the trait requires: delegating `query` alone would leave the
        /// inner store's own override unused.
        async fn get_metadata(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
        ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        async fn put(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
            fragment: lore_base::types::Fragment,
            payload: Option<bytes::Bytes>,
            force: bool,
        ) -> Result<(), lore_storage::StoreError> {
            if fragment.size_content == self.size {
                let seen = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                if seen < self.reject_first {
                    return Err(lore_storage::StoreError::internal("rejected on purpose"));
                }
                self.forwarded
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await
        }

        async fn obliterate(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
            stats: Arc<lore_storage::StoreObliterateStats>,
        ) -> Result<(), lore_storage::StoreError> {
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
        ) -> Result<usize, lore_storage::StoreError> {
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
        ) -> Result<Option<usize>, lore_storage::StoreError> {
            self.inner
                .clone()
                .compact(max_size, at, sync_data, sink)
                .await
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            self.inner.clone().compact_resume_at().await
        }

        fn max_query_batch(&self) -> Option<usize> {
            None
        }

        async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), lore_storage::StoreError> {
            self.inner.clone().flush(sync_data).await
        }

        async fn verify(self: Arc<Self>, heal: bool) -> Result<(), lore_storage::StoreError> {
            self.inner.clone().verify(heal).await
        }

        async fn copy(
            self: Arc<Self>,
            source_partition: lore_base::types::Partition,
            source_address: lore_base::types::Address,
            destination_partition: lore_base::types::Partition,
            destination_context: lore_base::types::Context,
            durable: bool,
        ) -> Result<(), lore_storage::StoreError> {
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

    /// Start a server speaking `transport`, backed by fresh in-memory stores.
    async fn start_server(transport: Transport) -> TestServer {
        match transport {
            Transport::Grpc => start_test_server().await,
            Transport::Quic => start_quic_test_server().await,
        }
    }

    /// Block until `addr` accepts a TCP connection, panicking if it never does.
    ///
    /// The gRPC server binds inside a spawned task, so a failed bind cannot propagate to the
    /// caller. Returning quietly on timeout leaves the test to meet it as a peer that never
    /// answers — minutes of client retries reported as a hang instead of the bind error it is,
    /// which is why `stopped` is polled alongside: a server that died says why.
    async fn await_listening(
        addr: SocketAddr,
        stopped: &mut tokio::sync::oneshot::Receiver<String>,
    ) {
        for _ in 0..100 {
            if let Ok(reason) = stopped.try_recv() {
                panic!("test server on {addr} {reason}");
            }
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("server at {addr} never started listening");
    }

    /// Serve gRPC on `listener` until the returned sender is dropped, reporting through the
    /// returned receiver once the serve task has returned.
    fn spawn_grpc_server(
        listener: std::net::TcpListener,
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
    ) -> (
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<String>,
    ) {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel::<String>();

        let notification_sender: Arc<dyn lore_revision::notification::NotificationSender> =
            Arc::new(lore_server::notification::local::NotificationSender::default());
        let hook_dispatcher = Arc::new(HookDispatcher::empty());

        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            let outcome = GrpcServerBuilder::new()
                .with_environment(EnvironmentConfig::default())
                .with_feature(FeatureSettings::default())
                .with_immutable_store(backend_immutable.clone(), backend_immutable)
                .with_mutable_store(backend_mutable)
                .with_lock_store(None)
                .with_notification(notification_sender, None)
                .with_hook_dispatcher(hook_dispatcher)
                .with_tls_config(None, None, None)
                .unwrap()
                .with_admin_endpoints(HashMap::new(), vec![])
                .with_http2_config(
                    None,
                    None,
                    Duration::from_secs(30),
                    Default::default(),
                    Default::default(),
                    None,
                )
                .with_jwt_verifier(None)
                .unwrap()
                .serve_with_listener(listener, async {
                    shutdown_rx.await.ok();
                })
                .await;
            let _ = stopped_tx.send(match outcome {
                Ok(()) => "stopped before the test finished".to_string(),
                Err(error) => format!("failed: {error}"),
            });
        });

        (shutdown_tx, stopped_rx)
    }

    /// Build a fresh in-memory backend pair for a test server to serve.
    async fn make_backends() -> (
        Arc<dyn lore_storage::ImmutableStore>,
        Arc<dyn lore_storage::MutableStore>,
    ) {
        let backend_immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                protect_local_fragment: false,
                implicit_durable_stored: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let backend_mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            backend_immutable.clone(),
        )
        .await
        .unwrap();

        (backend_immutable, backend_mutable)
    }

    async fn start_test_server() -> TestServer {
        let (backend_immutable, backend_mutable) = make_backends().await;
        start_test_server_with(backend_immutable, backend_mutable).await
    }

    /// Start a server over caller-supplied backends, so a test can wrap the served store in a
    /// fault-injecting decorator.
    async fn start_test_server_with(
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
    ) -> TestServer {
        let backend_for_test = backend_immutable.clone();
        let backend_mutable_for_test: Arc<dyn lore_storage::MutableStore> = backend_mutable.clone();

        // Bound here and handed over, so nothing can take the port between choosing it and
        // serving on it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();

        let (shutdown_tx, mut stopped_rx) =
            spawn_grpc_server(listener, backend_immutable, backend_mutable);
        await_listening(addr, &mut stopped_rx).await;

        TestServer {
            url: format!("grpc://127.0.0.1:{}", addr.port()),
            backend_immutable: backend_for_test,
            backend_mutable: backend_mutable_for_test,
            grpc_shutdown: Some(shutdown_tx),
            grpc_stopped: Some(stopped_rx),
            quic: None,
        }
    }

    /// A gRPC server whose immutable store refuses exactly one leaf of `leaf_size` bytes.
    ///
    /// `backend_immutable` on the returned handle is the *inner* store, so assertions see what
    /// actually landed rather than the refusing wrapper.
    async fn start_server_rejecting_one_leaf(leaf_size: u64) -> TestServer {
        start_server_intercepting_puts(|inner| {
            InterceptPutStore::new(inner, leaf_size, 1, Duration::ZERO)
        })
        .await
        .0
    }

    /// A gRPC server whose immutable store is wrapped by `intercept`, returned alongside it so a
    /// test can read what the store was asked to do.
    ///
    /// `backend_immutable` on the returned handle is the *inner* store, so assertions see what
    /// actually landed rather than the wrapper.
    async fn start_server_intercepting_puts(
        intercept: impl FnOnce(Arc<dyn lore_storage::ImmutableStore>) -> Arc<InterceptPutStore>,
    ) -> (TestServer, Arc<InterceptPutStore>) {
        let backend_immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                protect_local_fragment: false,
                implicit_durable_stored: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let backend_mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            backend_immutable.clone(),
        )
        .await
        .unwrap();
        let backend_for_test = backend_immutable.clone();
        let intercept = intercept(backend_immutable);
        let intercept_for_test = intercept.clone();
        let backend_immutable: Arc<dyn lore_storage::ImmutableStore> = intercept;
        let backend_mutable_for_test: Arc<dyn lore_storage::MutableStore> = backend_mutable.clone();

        // Bound here and handed over, so nothing can take the port between choosing it and
        // serving on it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();

        let (shutdown_tx, mut stopped_rx) =
            spawn_grpc_server(listener, backend_immutable, backend_mutable);
        await_listening(addr, &mut stopped_rx).await;

        (
            TestServer {
                url: format!("grpc://127.0.0.1:{}", addr.port()),
                backend_immutable: backend_for_test,
                backend_mutable: backend_mutable_for_test,
                grpc_shutdown: Some(shutdown_tx),
                grpc_stopped: Some(stopped_rx),
                quic: None,
            },
            intercept_for_test,
        )
    }

    /// A `lore://` server: QUIC for storage, gRPC for everything else.
    ///
    /// `LoreProtocol` routes storage over QUIC but revision, repository, admin and environment
    /// over gRPC, and `Connection::connect` fetches the environment config before anything else —
    /// so a `lore://` peer must serve both. QUIC is UDP and gRPC is TCP, so they share one port
    /// number. Both are backed by the same stores, as a real deployment has them.
    async fn start_quic_test_server() -> TestServer {
        let backend_immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                protect_local_fragment: false,
                implicit_durable_stored: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let backend_mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            backend_immutable.clone(),
        )
        .await
        .unwrap();
        let backend_for_test = backend_immutable.clone();
        let backend_mutable_for_test: Arc<dyn lore_storage::MutableStore> = backend_mutable.clone();

        // Both legs serve on one number: gRPC on its TCP half, QUIC on its UDP half. Both are bound
        // before either server starts, so neither has a port to lose.
        let (listener, udp) = bind_matched_pair();
        let addr: SocketAddr = listener.local_addr().expect("tcp local addr");

        let (cert_path, key_path, _ca) = server_certs().expect("test certs");
        let quic = QuinnServer::start(
            QuinnConfigBuilder::new()
                .socket(udp)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    backend_immutable.clone(),
                    backend_mutable.clone(),
                )))
                .build()
                .expect("quinn config"),
        )
        .expect("quinn server start");

        let (shutdown_tx, mut stopped_rx) =
            spawn_grpc_server(listener, backend_immutable, backend_mutable);
        await_listening(addr, &mut stopped_rx).await;

        TestServer {
            url: format!("lore://127.0.0.1:{}", addr.port()),
            backend_immutable: backend_for_test,
            backend_mutable: backend_mutable_for_test,
            grpc_shutdown: Some(shutdown_tx),
            grpc_stopped: Some(stopped_rx),
            quic: Some(quic),
        }
    }

    /// The shutdown barrier itself, because two tests depend on a server actually being gone
    /// and dropping one does not achieve that: the serve task tears the listener down
    /// asynchronously, so a dropped server keeps accepting. Measured before this barrier
    /// existed, a dropped server was still accepting on every attempt.
    ///
    /// TCP covers the gRPC leg on both server kinds, `lore://` included, since both serve gRPC
    /// on the port's TCP half. The QUIC half has no equivalent probe and rests on
    /// `Endpoint::close` plus `wait_idle`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_awaited_shutdown_leaves_the_port_closed() {
        let execution = setup_execution("storage-remote-shutdown-barrier".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let addr: SocketAddr = server
                        .url
                        .rsplit_once('/')
                        .expect("url carries a scheme")
                        .1
                        .parse()
                        .expect("url carries a socket address");

                    server.shutdown().await;

                    assert!(
                        tokio::net::TcpStream::connect(addr).await.is_err(),
                        "{transport:?}: the port must refuse a connection once the server has \
                         stopped, or a test that needs an unreachable peer does not have one"
                    );
                }
            })
            .await;
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Captured {
        Opened { handle_id: u64 },
        Error,
        Complete(i32),
        Other,
    }

    fn make_sink() -> (Arc<Mutex<Vec<Captured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let rec = match event {
                LoreEvent::StorageOpened(data) => Captured::Opened {
                    handle_id: data.handle_id,
                },
                LoreEvent::Error(_) => Captured::Error,
                LoreEvent::Complete(data) => Captured::Complete(data.status),
                _ => Captured::Other,
            };
            sink_for_cb.lock().unwrap().push(rec);
        }));
        (sink, callback)
    }

    fn take_opened(events: &[Captured]) -> Option<u64> {
        events.iter().find_map(|e| match e {
            Captured::Opened { handle_id } => Some(*handle_id),
            _ => None,
        })
    }

    async fn open_remote_handle(server: &TestServer) -> u64 {
        let (sink, callback) = make_sink();
        let status = open::open(
            LoreGlobalArgs::default(),
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                remote_config: LoreStorageRemoteConfig {
                    remote_url: LoreString::from(server.url.as_str()),
                },
                has_remote_config: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "open with remote_config must succeed");
        let events = sink.lock().unwrap().clone();
        take_opened(&events).expect("STORAGE_OPENED must fire on remote-configured open")
    }

    async fn close_handle(handle_id: u64) {
        let close_status = close::close(
            LoreGlobalArgs::default(),
            lore::storage::close::LoreStorageCloseArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
            },
            None,
        )
        .await;
        assert_eq!(close_status, 0, "close must succeed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_with_remote_config_succeeds_against_real_server() -> TestResult {
        let execution = setup_execution("storage-remote-open".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;

                    let (sink, callback) = make_sink();
                    let status = open::open(
                        LoreGlobalArgs::default(),
                        LoreStorageOpenArgs {
                            repository_path: LoreString::default(),
                            in_memory: 1,
                            remote_config: LoreStorageRemoteConfig {
                                remote_url: LoreString::from(server.url.as_str()),
                            },
                            has_remote_config: 1,
                            ..Default::default()
                        },
                        callback,
                    )
                    .await;

                    assert_eq!(status, 0, "open with remote_config must succeed");
                    let events = sink.lock().unwrap().clone();
                    let handle_id = take_opened(&events)
                        .expect("STORAGE_OPENED must fire on remote-configured open");
                    assert!(handle_id != 0, "handle_id must be non-zero");

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_with_remote_write_uploads_payload_to_server() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-put".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;

                    let payload = b"phase-d remote upload payload".to_vec();
                    let partition = Partition::from([0xa7u8; 16]);

                    let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                            ));
                        }
                    }));

                    let item = LoreStoragePutItem {
                        id: 1,
                        partition,
                        context: Context::default(),
                        data: LoreBytes {
                            ptr: payload.as_ptr().cast(),
                            len: payload.len(),
                        },
                        remote_write: 1,
                        local_cache: 0,
                        fixed_size_chunk: 0,
                    };
                    let status = put::put(
                        LoreGlobalArgs::default(),
                        LoreStoragePutArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "put with remote_write=1 must succeed");

                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), 1, "exactly one PUT_ITEM_COMPLETE expected");
                    let (id, address, code) = events[0];
                    assert_eq!(id, 1);
                    assert_eq!(code, LoreErrorCode::None);
                    assert_ne!(address.hash, lore_base::types::Hash::default());

                    let server_match =
                        query_one(&server.backend_immutable.clone(), partition, address)
                            .await
                            .expect("backend exist call");
                    assert_eq!(
                        server_match.match_made,
                        StoreMatch::MatchFull,
                        "remote backend must hold the address after remote_write=1 put",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_without_remote_write_does_not_upload() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-put-localonly".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;

                    let payload = b"local-only put (remote_write=0)".to_vec();
                    let partition = Partition::from([0xa8u8; 16]);

                    let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                            ));
                        }
                    }));

                    let item = LoreStoragePutItem {
                        id: 42,
                        partition,
                        context: Context::default(),
                        data: LoreBytes {
                            ptr: payload.as_ptr().cast(),
                            len: payload.len(),
                        },
                        remote_write: 0,
                        local_cache: 0,
                        fixed_size_chunk: 0,
                    };
                    let status = put::put(
                        LoreGlobalArgs::default(),
                        LoreStoragePutArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0);

                    let events = captured.lock().unwrap().clone();
                    let (_, address, code) = events[0];
                    assert_eq!(code, LoreErrorCode::None);

                    let server_match =
                        query_one(&server.backend_immutable.clone(), partition, address)
                            .await
                            .expect("backend exist call");
                    assert_eq!(
                        server_match.match_made,
                        StoreMatch::MatchNone,
                        "remote backend must NOT hold the address when remote_write=0",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_falls_back_to_remote_on_local_miss() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-get".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;

                    let payload_bytes = b"phase-d remote get on miss".to_vec();
                    let payload = Bytes::from(payload_bytes.clone());
                    let partition = Partition::from([0xb1u8; 16]);
                    let hash = lore_storage::hash_slice(payload.as_ref());
                    let address = Address {
                        hash,
                        context: Context::default(),
                    };
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };
                    server
                        .backend_immutable
                        .clone()
                        .put(partition, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("seed server with payload");

                    let handle_id = open_remote_handle(&server).await;

                    let received: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                    let received_for_cb = received.clone();
                    let outcomes: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let callback: LoreEventCallback =
                        Some(Box::new(move |event: &LoreEvent| match event {
                            LoreEvent::StorageGetData(data) => {
                                let slice = unsafe {
                                    std::slice::from_raw_parts(
                                        data.bytes.ptr.cast::<u8>(),
                                        data.bytes.len,
                                    )
                                };
                                received_for_cb.lock().unwrap().extend_from_slice(slice);
                            }
                            LoreEvent::StorageGetItemComplete(data) => {
                                outcomes_for_cb
                                    .lock()
                                    .unwrap()
                                    .push((data.id, data.error_code));
                            }
                            _ => {}
                        }));

                    let item = LoreStorageGetItem {
                        id: 7,
                        partition,
                        address,
                        streaming: 0,
                        local_cache: 0,
                        ..Default::default()
                    };
                    let status = get::get(
                        LoreGlobalArgs::default(),
                        LoreStorageGetArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "get must succeed against remote-only address");

                    let outcomes = outcomes.lock().unwrap().clone();
                    assert_eq!(outcomes.len(), 1);
                    assert_eq!(outcomes[0], (7, LoreErrorCode::None));

                    let received = received.lock().unwrap().clone();
                    assert_eq!(received, payload_bytes, "fetched bytes must match remote");

                    let local = lore::storage::handle::immutable_for_test(
                        lore::storage::handle::LoreStore { handle_id },
                    )
                    .expect("handle still registered");
                    let local_match = lore_storage::immutable_store::query_one(
                        &local.clone(),
                        partition,
                        address,
                    )
                    .await
                    .expect("local exist call");
                    assert_eq!(
                        local_match.match_made,
                        lore_storage::store_types::StoreMatch::MatchNone,
                        "unflagged remote-fetched payload must NOT be cached locally",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// `get` does not blanket-cache remote-fetched payloads, but a producer-side write hint
    /// (`PayloadLocalCachePriority` set on the seed fragment) opts the payload into local
    /// caching via `load_fragment`'s `should_store` gate. Verify the gate fires for that
    /// case so producers retain the ability to mark "this should always be cached".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_caches_locally_when_payload_has_local_cache_priority_flag() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_base::types::fragment_flags::FragmentFlags;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-cache-priority".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;

                    let payload_bytes = b"priority-flagged payload".to_vec();
                    let payload = Bytes::from(payload_bytes.clone());
                    let partition = Partition::from([0xc7u8; 16]);
                    let hash = lore_storage::hash_slice(payload.as_ref());
                    let address = Address {
                        hash,
                        context: Context::default(),
                    };
                    let fragment = Fragment {
                        flags: FragmentFlags::PayloadLocalCachePriority.bits(),
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };
                    server
                        .backend_immutable
                        .clone()
                        .put(partition, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("seed server with priority-flagged payload");

                    let handle_id = open_remote_handle(&server).await;

                    let outcomes: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(data) = event {
                            outcomes_for_cb
                                .lock()
                                .unwrap()
                                .push((data.id, data.error_code));
                        }
                    }));
                    let status = get::get(
                        LoreGlobalArgs::default(),
                        LoreStorageGetArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageGetItem {
                                id: 41,
                                partition,
                                address,
                                streaming: 0,
                                local_cache: 0,
                                ..Default::default()
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0);
                    assert_eq!(outcomes.lock().unwrap()[0].1, LoreErrorCode::None);

                    let local = lore::storage::handle::immutable_for_test(
                        lore::storage::handle::LoreStore { handle_id },
                    )
                    .expect("handle still registered");
                    let local_match = query_one(&local.clone(), partition, address)
                        .await
                        .expect("local exist call");
                    assert_eq!(
                        local_match.match_made,
                        StoreMatch::MatchFull,
                        "priority-flagged remote-fetched payload must be cached locally",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// `local_cache=1` on a get item forces `with_cache()` for that fetch even when the
    /// fragment is not flagged with `PayloadLocalCachePriority`. The companion
    /// `get_falls_back_to_remote_on_local_miss` test asserts the opposite (no flag, no
    /// per-item opt-in → no cache); this one proves the per-item opt-in works.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_with_local_cache_flag_caches_remote_fetched_payload_locally() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-get-localcache".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let payload_bytes = b"per-item local_cache opt-in".to_vec();
                    let payload = Bytes::from(payload_bytes.clone());
                    let partition = Partition::from([0xc8u8; 16]);
                    let address = Address {
                        hash: lore_storage::hash_slice(payload.as_ref()),
                        context: Context::default(),
                    };
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };
                    server
                        .backend_immutable
                        .clone()
                        .put(partition, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("seed server");
                    let handle_id = open_remote_handle(&server).await;

                    let outcomes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(data) = event {
                            outcomes_for_cb.lock().unwrap().push(data.error_code);
                        }
                    }));
                    let status = get::get(
                        LoreGlobalArgs::default(),
                        LoreStorageGetArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageGetItem {
                                id: 51,
                                partition,
                                address,
                                streaming: 0,
                                local_cache: 1,
                                ..Default::default()
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0);
                    assert_eq!(outcomes.lock().unwrap()[0], LoreErrorCode::None);

                    let local = lore::storage::handle::immutable_for_test(
                        lore::storage::handle::LoreStore { handle_id },
                    )
                    .expect("handle still registered");
                    let local_match = query_one(&local.clone(), partition, address)
                        .await
                        .expect("local exist");
                    assert_eq!(
                        local_match.match_made,
                        StoreMatch::MatchFull,
                        "local_cache=1 must populate the local store after remote fetch",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// `local_cache=1` on a put item tags the resulting fragment with
    /// `PayloadLocalCachePriority`. A subsequent local query of the address shows the flag
    /// is preserved so future remote reads of this content cache regardless of the reader's
    /// caching choice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_with_local_cache_flag_tags_fragment_with_priority() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-put-localcache".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;

                    let payload = b"put with local_cache priority".to_vec();
                    let partition = Partition::from([0xc9u8; 16]);
                    let context = Context::from([0xa9u8; 16]);

                    let captured: Arc<Mutex<Vec<(Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            captured_for_cb
                                .lock()
                                .unwrap()
                                .push((data.address, data.error_code));
                        }
                    }));
                    let item = LoreStoragePutItem {
                        id: 61,
                        partition,
                        context,
                        data: LoreBytes {
                            ptr: payload.as_ptr().cast(),
                            len: payload.len(),
                        },
                        remote_write: 0,
                        local_cache: 1,
                        fixed_size_chunk: 0,
                    };
                    let status = put::put(
                        LoreGlobalArgs::default(),
                        LoreStoragePutArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0);
                    let (address, code) = captured.lock().unwrap()[0];
                    assert_eq!(code, LoreErrorCode::None);
                    drop(payload);

                    let local = lore::storage::handle::immutable_for_test(
                        lore::storage::handle::LoreStore { handle_id },
                    )
                    .expect("handle still registered");
                    let (fragment, _bytes) = local
                        .clone()
                        .get(partition, address)
                        .await
                        .and_then(lore_storage::StoreGetData::into_payload)
                        .expect("local fragment fetch");
                    assert!(
                    fragment.flags
                        & lore_base::types::fragment_flags::FragmentFlags::PayloadLocalCachePriority
                            .bits() != 0,
                    "local_cache=1 must set PayloadLocalCachePriority on the fragment; \
                     got flags={:#x}",
                    fragment.flags,
                );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    async fn put_local_via_handle(
        handle_id: u64,
        partition: lore_base::types::Partition,
        bytes: &[u8],
    ) -> lore_base::types::Address {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Context;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.address, data.error_code));
            }
        }));
        let item = LoreStoragePutItem {
            id: 99,
            partition,
            context: Context::default(),
            data: LoreBytes {
                ptr: bytes.as_ptr().cast(),
                len: bytes.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let status = put::put(
            LoreGlobalArgs::default(),
            LoreStoragePutArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = captured.lock().unwrap().clone();
        assert_eq!(events[0].2, LoreErrorCode::None);
        events[0].1
    }

    async fn copy_one_item(
        handle_id: u64,
        source_partition: lore_base::types::Partition,
        source_address: lore_base::types::Address,
        target_partition: lore_base::types::Partition,
    ) -> (
        i32,
        Vec<(
            u64,
            lore_base::types::Address,
            lore_revision::event::LoreErrorCode,
        )>,
    ) {
        use lore::storage::copy;
        use lore::storage::copy::LoreStorageCopyArgs;
        use lore::storage::copy::LoreStorageCopyItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageCopyItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push((
                    data.id,
                    data.source_address,
                    data.error_code,
                ));
            }
        }));
        let status = copy::copy(
            LoreGlobalArgs::default(),
            LoreStorageCopyArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageCopyItem {
                    id: 11,
                    source_partition,
                    target_partition,
                    source_address,
                    target_context: source_address.context,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Put source via the handle with `remote_write=1`, so the bytes land both locally and on
    /// the server. Returns the resulting address.
    async fn put_local_and_remote_via_handle(
        handle_id: u64,
        partition: lore_base::types::Partition,
        bytes: &[u8],
    ) -> lore_base::types::Address {
        put_local_and_remote_chunked(handle_id, partition, bytes, 0).await
    }

    /// As above, but cutting the content at `chunk` so it is stored — and uploaded — as a
    /// fragment tree rather than one fragment.
    async fn put_local_and_remote_chunked(
        handle_id: u64,
        partition: lore_base::types::Partition,
        bytes: &[u8],
        chunk: u64,
    ) -> lore_base::types::Address {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Context;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.address, data.error_code));
            }
        }));
        let item = LoreStoragePutItem {
            id: 100,
            partition,
            context: Context::default(),
            data: LoreBytes {
                ptr: bytes.as_ptr().cast(),
                len: bytes.len(),
            },
            remote_write: 1,
            local_cache: 0,
            fixed_size_chunk: chunk,
        };
        let status = put::put(
            LoreGlobalArgs::default(),
            LoreStoragePutArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = captured.lock().unwrap().clone();
        assert_eq!(events[0].2, LoreErrorCode::None);
        events[0].1
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_tier1_server_side_when_source_on_both() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-copy-tier1".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let source_partition = Partition::from([0xc1u8; 16]);
                    let target_partition = Partition::from([0xc2u8; 16]);

                    let handle_id = open_remote_handle(&server).await;
                    let payload_bytes = b"copy tier-1 payload (both local and server)".to_vec();
                    let address = put_local_and_remote_via_handle(
                        handle_id,
                        source_partition,
                        payload_bytes.as_slice(),
                    )
                    .await;

                    let (status, events) =
                        copy_one_item(handle_id, source_partition, address, target_partition).await;
                    assert_eq!(status, 0);
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].2, LoreErrorCode::None);
                    assert_eq!(events[0].1, address);

                    let on_server =
                        query_one(&server.backend_immutable.clone(), target_partition, address)
                            .await
                            .unwrap();
                    assert_eq!(
                        on_server.match_made,
                        StoreMatch::MatchFull,
                        "server must hold target entry after server-side copy",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_tier2_upload_fallback_when_local_source_only() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-copy-tier2".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let source_partition = Partition::from([0xd1u8; 16]);
                    let target_partition = Partition::from([0xd2u8; 16]);

                    let handle_id = open_remote_handle(&server).await;
                    let payload_bytes = b"copy tier-2 upload-fallback payload".to_vec();
                    let address =
                        put_local_via_handle(handle_id, source_partition, payload_bytes.as_slice())
                            .await;

                    let server_pre =
                        query_one(&server.backend_immutable.clone(), source_partition, address)
                            .await
                            .unwrap();
                    assert_eq!(
                        server_pre.match_made,
                        StoreMatch::MatchNone,
                        "precondition: server must NOT have source",
                    );

                    let (status, events) =
                        copy_one_item(handle_id, source_partition, address, target_partition).await;
                    assert_eq!(status, 0);
                    assert_eq!(events[0].2, LoreErrorCode::None);
                    assert_eq!(events[0].1, address);

                    let on_server =
                        query_one(&server.backend_immutable.clone(), target_partition, address)
                            .await
                            .unwrap();
                    assert_eq!(
                        on_server.match_made,
                        StoreMatch::MatchFull,
                        "server must hold target after upload fallback",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    async fn obliterate_one_item(
        handle_id: u64,
        partition: lore_base::types::Partition,
        address: lore_base::types::Address,
    ) -> (
        i32,
        Vec<(
            u64,
            lore_base::types::Address,
            u8,
            u8,
            u8,
            u8,
            lore_revision::event::LoreErrorCode,
        )>,
    ) {
        use lore::storage::obliterate;
        use lore::storage::obliterate::LoreStorageObliterateArgs;
        use lore::storage::obliterate::LoreStorageObliterateItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        #[allow(clippy::type_complexity)]
        let captured: Arc<
            Mutex<
                Vec<(
                    u64,
                    lore_base::types::Address,
                    u8,
                    u8,
                    u8,
                    u8,
                    LoreErrorCode,
                )>,
            >,
        > = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageObliterateItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push((
                    data.id,
                    data.address,
                    data.local_success,
                    data.remote_success,
                    data.local_skipped,
                    data.remote_skipped,
                    data.error_code,
                ));
            }
        }));
        let status = obliterate::obliterate(
            LoreGlobalArgs::default(),
            LoreStorageObliterateArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageObliterateItem {
                    id: 31,
                    partition,
                    address,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Verify obliterate runs both sides and reports each side's outcome separately. The test
    /// server is configured with no JWT verifier, so the admin obliterate path returns
    /// `Ok` — `remote_success=1` and `error_code=None` are the expected
    /// outcomes. `local_success=1` confirms the local side ran independently and succeeded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn obliterate_runs_local_and_remote_in_parallel_with_independent_outcomes() -> TestResult
    {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-obliterate".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xb5u8; 16]);

                    let handle_id = open_remote_handle(&server).await;
                    let payload_bytes = b"obliterate me everywhere".to_vec();
                    let address = put_local_and_remote_via_handle(
                        handle_id,
                        partition,
                        payload_bytes.as_slice(),
                    )
                    .await;

                    let (_status, events) =
                        obliterate_one_item(handle_id, partition, address).await;
                    assert_eq!(events.len(), 1);
                    let (id, _addr, local_success, remote_success, _ls, _rs, error_code) =
                        events[0];
                    assert_eq!(id, 31);
                    assert_eq!(local_success, 1, "local obliterate must succeed");
                    assert_eq!(remote_success, 1, "without JWT the admin path is allowed",);
                    assert_eq!(
                        error_code,
                        LoreErrorCode::None,
                        "Remote obliterate should have succeeded",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// An absent local address still reports `local_success=1` (idempotent obliterate).
    /// The remote side fails for the same JWT reason, which is independent of presence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn obliterate_absent_address_is_idempotent_on_local_side() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let execution = setup_execution("storage-remote-obliterate-absent".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xb6u8; 16]);
                    let handle_id = open_remote_handle(&server).await;
                    let address = Address {
                        hash: Hash::from([0xddu8; 32]),
                        context: Context::default(),
                    };
                    let (_status, events) =
                        obliterate_one_item(handle_id, partition, address).await;
                    let (_, _, local_success, _, _, _, _) = events[0];
                    assert_eq!(local_success, 1, "absent address still succeeds locally");

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_falls_back_to_remote_when_local_misses() -> TestResult {
        use bytes::Bytes;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-query-multiplex".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xc7u8; 16]);

                    let n = 10usize;
                    let mut payloads: Vec<Vec<u8>> = (0..n)
                        .map(|i| format!("multiplexed-query-payload-{i}").into_bytes())
                        .collect();
                    let mut addresses: Vec<Address> = Vec::with_capacity(n);
                    for payload in &payloads {
                        let bytes = Bytes::from(payload.clone());
                        let hash = lore_storage::hash_slice(bytes.as_ref());
                        let address = Address {
                            hash,
                            context: Context::default(),
                        };
                        let fragment = Fragment {
                            flags: 0,
                            size_payload: bytes.len() as u32,
                            size_content: bytes.len() as u64,
                        };
                        server
                            .backend_immutable
                            .clone()
                            .put(partition, address, fragment, Some(bytes), false)
                            .await
                            .expect("seed query item");
                        addresses.push(address);
                    }
                    payloads.clear();

                    let handle_id = open_remote_handle(&server).await;

                    let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetMetadataItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                            ));
                        }
                    }));

                    use lore::storage::get_metadata;
                    use lore::storage::get_metadata::LoreStorageGetMetadataArgs;
                    use lore::storage::get_metadata::LoreStorageGetMetadataItem;
                    let items: Vec<LoreStorageGetMetadataItem> = addresses
                        .iter()
                        .enumerate()
                        .map(|(i, addr)| LoreStorageGetMetadataItem {
                            id: i as u64,
                            partition,
                            address: *addr,
                        })
                        .collect();
                    let status = get_metadata::get_metadata(
                        LoreGlobalArgs::default(),
                        LoreStorageGetMetadataArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(items),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "all items must succeed via remote get_metadata");

                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), n);
                    let mut events = events;
                    events.sort_by_key(|(id, _, _)| *id);
                    for (i, (id, addr, code)) in events.iter().enumerate() {
                        assert_eq!(*id, i as u64);
                        assert_eq!(*addr, addresses[i]);
                        assert_eq!(*code, LoreErrorCode::None);
                    }

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    async fn upload_one_item(
        handle_id: u64,
        partition: lore_base::types::Partition,
        address: lore_base::types::Address,
    ) -> (
        i32,
        Vec<(
            u64,
            lore_base::types::Address,
            u8,
            lore_revision::event::LoreErrorCode,
        )>,
    ) {
        use lore::storage::upload;
        use lore::storage::upload::LoreStorageUploadArgs;
        use lore::storage::upload::LoreStorageUploadItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        #[allow(clippy::type_complexity)]
        let captured: Arc<Mutex<Vec<(u64, lore_base::types::Address, u8, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageUploadItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push((
                    data.id,
                    data.address,
                    data.already_durable,
                    data.error_code,
                ));
            }
        }));
        let status = upload::upload(
            LoreGlobalArgs::default(),
            LoreStorageUploadArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageUploadItem {
                    id: 51,
                    partition,
                    address,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_local_only_payload_pushes_to_remote_and_marks_durable() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-upload".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xa5u8; 16]);

                    let handle_id = open_remote_handle(&server).await;
                    let payload_bytes = b"upload deferred payload".to_vec();
                    let address =
                        put_local_via_handle(handle_id, partition, payload_bytes.as_slice()).await;

                    let (status, events) = upload_one_item(handle_id, partition, address).await;
                    assert_eq!(status, 0, "upload must succeed");
                    assert_eq!(events.len(), 1);
                    let (_, _, already_durable, error_code) = events[0];
                    assert_eq!(error_code, LoreErrorCode::None);
                    assert_eq!(already_durable, 0, "first upload was not yet durable");

                    let on_server =
                        query_one(&server.backend_immutable.clone(), partition, address)
                            .await
                            .unwrap();
                    assert_eq!(
                        on_server.match_made,
                        StoreMatch::MatchFull,
                        "server must hold the address after a successful upload",
                    );

                    let (status2, events2) = upload_one_item(handle_id, partition, address).await;
                    assert_eq!(status2, 0);
                    let (_, _, already_durable2, error_code2) = events2[0];
                    assert_eq!(error_code2, LoreErrorCode::None);
                    assert_eq!(
                        already_durable2, 1,
                        "second upload must short-circuit as already_durable=1",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_unknown_address_returns_address_not_found() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-upload-unknown".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xa6u8; 16]);
                    let handle_id = open_remote_handle(&server).await;
                    let address = Address {
                        hash: Hash::from([0xfau8; 32]),
                        context: Context::default(),
                    };
                    let (_, events) = upload_one_item(handle_id, partition, address).await;
                    let (_, _, already_durable, error_code) = events[0];
                    assert_eq!(error_code, LoreErrorCode::AddressNotFound);
                    assert_eq!(already_durable, 0);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_zero_hash_short_circuits_as_already_durable() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-upload-zero".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xa7u8; 16]);
                    let handle_id = open_remote_handle(&server).await;
                    let address = Address {
                        hash: Hash::default(),
                        context: Context::default(),
                    };
                    let (status, events) = upload_one_item(handle_id, partition, address).await;
                    assert_eq!(status, 0);
                    let (_, _, already_durable, error_code) = events[0];
                    assert_eq!(error_code, LoreErrorCode::None);
                    assert_eq!(already_durable, 1);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_handle_without_remote_fails_pre_dispatch() -> TestResult {
        use lore::storage::upload;
        use lore::storage::upload::LoreStorageUploadArgs;
        use lore::storage::upload::LoreStorageUploadItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-upload-no-remote".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (sink, callback) = make_sink();
                let status = open::open(
                    LoreGlobalArgs::default(),
                    LoreStorageOpenArgs {
                        repository_path: LoreString::default(),
                        in_memory: 1,
                        ..Default::default()
                    },
                    callback,
                )
                .await;
                assert_eq!(status, 0);
                let events = sink.lock().unwrap().clone();
                let handle_id = take_opened(&events).expect("opened");

                let item_events: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
                let item_events_for_cb = item_events.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if matches!(event, LoreEvent::StorageUploadItemComplete(_)) {
                        *item_events_for_cb.lock().unwrap() += 1;
                    }
                }));
                let status = upload::upload(
                    LoreGlobalArgs::default(),
                    LoreStorageUploadArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageUploadItem {
                            id: 1,
                            partition: Partition::from([0xa8u8; 16]),
                            address: Address {
                                hash: Hash::from([0xb0u8; 32]),
                                context: Context::default(),
                            },
                        }]),
                    },
                    callback,
                )
                .await;
                assert_ne!(
                    status, 0,
                    "upload without remote_config must fail pre-dispatch"
                );
                assert_eq!(
                    *item_events.lock().unwrap(),
                    0,
                    "no per-item events must fire on pre-dispatch refusal",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Copy idempotency: re-copying onto an already-populated target tuple succeeds with the
    /// same `error_code = None` and produces no observable change. Exercises the remote path
    /// (idempotent on both server-side `session.copy` and the local mirror).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_idempotent_when_target_already_present() -> TestResult {
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-copy-idempotent".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let source_partition = Partition::from([0xa1u8; 16]);
                    let target_partition = Partition::from([0xa2u8; 16]);

                    let handle_id = open_remote_handle(&server).await;
                    let payload_bytes = b"copy idempotency payload".to_vec();
                    let address = put_local_and_remote_via_handle(
                        handle_id,
                        source_partition,
                        payload_bytes.as_slice(),
                    )
                    .await;

                    let (status1, events1) =
                        copy_one_item(handle_id, source_partition, address, target_partition).await;
                    assert_eq!(status1, 0);
                    assert_eq!(events1[0].2, LoreErrorCode::None);
                    let target_after_first =
                        query_one(&server.backend_immutable.clone(), target_partition, address)
                            .await
                            .unwrap();
                    assert_eq!(target_after_first.match_made, StoreMatch::MatchFull);

                    let (status2, events2) =
                        copy_one_item(handle_id, source_partition, address, target_partition).await;
                    assert_eq!(status2, 0);
                    assert_eq!(events2[0].2, LoreErrorCode::None);
                    assert_eq!(events2[0].1, address);
                    let target_after_second =
                        query_one(&server.backend_immutable.clone(), target_partition, address)
                            .await
                            .unwrap();
                    assert_eq!(
                        target_after_second, target_after_first,
                        "second copy must produce no observable change",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_tier3_no_local_no_server_returns_address_not_found() -> TestResult {
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-copy-tier3".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let source_partition = Partition::from([0xe1u8; 16]);
                    let target_partition = Partition::from([0xe2u8; 16]);

                    let handle_id = open_remote_handle(&server).await;

                    let address = Address {
                        hash: Hash::from([0xfeu8; 32]),
                        context: Context::default(),
                    };
                    let (_status, events) =
                        copy_one_item(handle_id, source_partition, address, target_partition).await;
                    assert_eq!(
                        events[0].2,
                        LoreErrorCode::AddressNotFound,
                        "tier-3: no local payload + server-side copy fails ⇒ ADDRESS_NOT_FOUND",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// `put_file` with `remote_write=1` against a remote-configured handle must upload to the
    /// server. Exercises the path that previously hardcoded `None` for the `remote_session` and
    /// silently dropped the upload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_file_with_remote_write_uploads_file_to_server() -> TestResult {
        use lore::storage::put_file;
        use lore::storage::put_file::LoreStoragePutFileArgs;
        use lore::storage::put_file::LoreStoragePutFileItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_revision::interface::LoreString;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-put-file".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;

                    let payload = b"put_file remote upload payload".to_vec();
                    let mut tempfile_handle = tempfile::Builder::new()
                        .prefix("lore-put-file-remote-")
                        .tempfile()
                        .expect("create tempfile");
                    std::io::Write::write_all(&mut tempfile_handle, &payload)
                        .expect("write tempfile");
                    let path = tempfile_handle.path().to_string_lossy().into_owned();
                    let partition = Partition::from([0xb1u8; 16]);

                    let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                            ));
                        }
                    }));

                    let item = LoreStoragePutFileItem {
                        id: 7,
                        partition,
                        context: Context::default(),
                        path: LoreString::from(path.as_str()),
                        remote_write: 1,
                        local_cache: 0,
                        fixed_size_chunk: 0,
                    };
                    let status = put_file::put_file(
                        LoreGlobalArgs::default(),
                        LoreStoragePutFileArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "put_file with remote_write=1 must succeed");

                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), 1, "exactly one PUT_ITEM_COMPLETE expected");
                    let (id, address, code) = events[0];
                    assert_eq!(id, 7);
                    assert_eq!(code, LoreErrorCode::None);
                    assert_ne!(address.hash, lore_base::types::Hash::default());

                    let server_match =
                        query_one(&server.backend_immutable.clone(), partition, address)
                            .await
                            .expect("backend exist call");
                    assert_eq!(
                        server_match.match_made,
                        StoreMatch::MatchFull,
                        "remote backend must hold the address after put_file with remote_write=1",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// `get_file` against a remote-only address (not in local cache) must fetch from the
    /// remote and write the bytes to the target file. Exercises the path that previously
    /// hardcoded `no_remote()` and would have failed with `ADDRESS_NOT_FOUND`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_file_falls_back_to_remote_on_local_miss() -> TestResult {
        use lore::storage::get_file;
        use lore::storage::get_file::LoreStorageGetFileArgs;
        use lore::storage::get_file::LoreStorageGetFileItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::FragmentFlags;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_revision::interface::LoreString;

        let execution = setup_execution("storage-remote-get-file".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;

                    let payload = b"remote-only payload for get_file".to_vec();
                    let partition = Partition::from([0xb2u8; 16]);
                    let address = Address {
                        hash: lore_base::types::Hash::hash_buffer(&payload),
                        context: Context::from([0xb3u8; 16]),
                    };

                    let fragment = Fragment {
                        flags: FragmentFlags::PayloadStoredLocal.bits(),
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };
                    server
                        .backend_immutable
                        .clone()
                        .put(
                            partition,
                            address,
                            fragment,
                            Some(bytes::Bytes::from(payload.clone())),
                            false,
                        )
                        .await
                        .expect("backend seed put");

                    let target_dir = tempfile::Builder::new()
                        .prefix("lore-get-file-remote-")
                        .tempdir()
                        .expect("create tempdir");
                    let target_path_buf = target_dir.path().join("target");
                    let target_path = target_path_buf.to_string_lossy().into_owned();

                    let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                            ));
                        }
                    }));

                    let item = LoreStorageGetFileItem {
                        id: 11,
                        partition,
                        address,
                        path: LoreString::from(target_path.as_str()),
                        local_cache: 0,
                        ..Default::default()
                    };
                    let status = get_file::get_file(
                        LoreGlobalArgs::default(),
                        LoreStorageGetFileArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "get_file falling back to remote must succeed");

                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), 1, "exactly one GET_ITEM_COMPLETE expected");
                    let (id, _addr, code) = events[0];
                    assert_eq!(id, 11);
                    assert_eq!(code, LoreErrorCode::None);

                    let written = std::fs::read(&target_path).expect("read target file");
                    assert_eq!(
                        written, payload,
                        "target file must hold the bytes fetched from the remote",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Open a remote-configured handle with `globals.offline=1`, then put with
    /// `remote_write=1`. The bound `offline` flag must silently suppress the upload — the
    /// server backend stays empty even though the API call returns success.
    async fn open_remote_handle_with_globals(server: &TestServer, globals: LoreGlobalArgs) -> u64 {
        let (sink, callback) = make_sink();
        let status = open::open(
            globals,
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                remote_config: LoreStorageRemoteConfig {
                    remote_url: LoreString::from(server.url.as_str()),
                },
                has_remote_config: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "open with bound globals must succeed");
        let events = sink.lock().unwrap().clone();
        take_opened(&events).expect("STORAGE_OPENED must fire on remote-configured open")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_offline_suppresses_remote_upload_on_put() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-bound-offline".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let bound = LoreGlobalArgs {
                        offline: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    let payload = b"bound-offline must not upload".to_vec();
                    let partition = Partition::from([0xb1u8; 16]);

                    let captured: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                            ));
                        }
                    }));

                    let item = LoreStoragePutItem {
                        id: 7,
                        partition,
                        context: Context::default(),
                        data: LoreBytes {
                            ptr: payload.as_ptr().cast(),
                            len: payload.len(),
                        },
                        remote_write: 1,
                        local_cache: 0,
                        fixed_size_chunk: 0,
                    };
                    let status = put::put(
                        LoreGlobalArgs::default(),
                        LoreStoragePutArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        status, 0,
                        "put on bound-offline handle must succeed locally"
                    );

                    let events = captured.lock().unwrap().clone();
                    let (_, address, code) = events[0];
                    assert_eq!(code, LoreErrorCode::None);

                    let server_match =
                        query_one(&server.backend_immutable.clone(), partition, address)
                            .await
                            .expect("backend exist call");
                    assert_eq!(
                        server_match.match_made,
                        StoreMatch::MatchNone,
                        "bound-offline handle must NOT push to the remote even with remote_write=1",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_local_suppresses_remote_fetch_on_get_miss() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-local".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;

                    let payload_bytes = b"bound-local must miss".to_vec();
                    let payload = Bytes::from(payload_bytes.clone());
                    let hash = lore_storage::hash::hash_slice(&payload_bytes);
                    let address = Address {
                        hash,
                        context: Context::from([0xbcu8; 16]),
                    };
                    let partition = Partition::from([0xbdu8; 16]);
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };
                    server
                        .backend_immutable
                        .clone()
                        .put(partition, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("seed server backend");

                    let bound = LoreGlobalArgs {
                        local: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(data) = event {
                            captured_for_cb
                                .lock()
                                .unwrap()
                                .push((data.id, data.error_code));
                        }
                    }));

                    let item = LoreStorageGetItem {
                        id: 9,
                        partition,
                        address,
                        streaming: 0,
                        local_cache: 0,
                        ..Default::default()
                    };
                    let _ = get::get(
                        LoreGlobalArgs::default(),
                        LoreStorageGetArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        callback,
                    )
                    .await;

                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), 1, "exactly one GET_ITEM_COMPLETE expected");
                    assert_eq!(
                        events[0].1,
                        LoreErrorCode::AddressNotFound,
                        "bound-local handle must NOT fetch remote on local miss; got {:?}",
                        events[0].1,
                    );
                    assert_ne!(address.hash, Hash::default());

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_call_local_and_remote_combo_rejects_with_invalid_arguments() -> TestResult {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-percall-conflict".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;

                    let payload = b"per-call conflict".to_vec();
                    let partition = Partition::from([0xbfu8; 16]);
                    let item = LoreStoragePutItem {
                        id: 1,
                        partition,
                        context: Context::default(),
                        data: LoreBytes {
                            ptr: payload.as_ptr().cast(),
                            len: payload.len(),
                        },
                        remote_write: 1,
                        local_cache: 0,
                        fixed_size_chunk: 0,
                    };
                    let bad = LoreGlobalArgs {
                        local: 1,
                        remote: 1,
                        ..Default::default()
                    };
                    let status = put::put(
                        bad,
                        LoreStoragePutArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        None,
                    )
                    .await;
                    assert_eq!(
                        status,
                        lore_base::error::InvalidArguments::FFI_CODE,
                        "per-call local=1 && remote=1 must reject with InvalidArguments",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Helper: seed a payload on the server backend and return its `(partition, address)` so
    /// tests can drive remote-fetch behavior against a known-present remote address with no
    /// matching local entry.
    async fn seed_server_only(
        server: &TestServer,
        partition_byte: u8,
        payload_bytes: &[u8],
    ) -> (lore_base::types::Partition, lore_base::types::Address) {
        use bytes::Bytes;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        let payload = Bytes::copy_from_slice(payload_bytes);
        let hash = lore_storage::hash::hash_slice(payload_bytes);
        let address = Address {
            hash,
            context: Context::from([0xc0u8; 16]),
        };
        let partition = Partition::from([partition_byte; 16]);
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        server
            .backend_immutable
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("seed server backend");
        (partition, address)
    }

    /// Suppression-on-get: bound `offline=1` makes get against a server-only address miss
    /// rather than fetching. Mirrors the same shape of the `bound_local_*` test for
    /// completeness — `offline` and `local` produce equivalent storage-side behavior, so any
    /// future divergence between them surfaces here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_offline_suppresses_remote_fetch_on_get_miss() -> TestResult {
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-offline-get".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let (partition, address) =
                        seed_server_only(&server, 0xc1, b"bound-offline get-miss target").await;
                    let bound = LoreGlobalArgs {
                        offline: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(data) = event {
                            captured_for_cb
                                .lock()
                                .unwrap()
                                .push((data.id, data.error_code));
                        }
                    }));
                    let _ = get::get(
                        LoreGlobalArgs::default(),
                        LoreStorageGetArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageGetItem {
                                id: 21,
                                partition,
                                address,
                                streaming: 0,
                                local_cache: 0,
                                ..Default::default()
                            }]),
                        },
                        callback,
                    )
                    .await;
                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].1, LoreErrorCode::AddressNotFound);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Suppression on `get_metadata`: bound `local=1` makes `get_metadata` against a server-only
    /// address miss without consulting the remote.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_local_suppresses_remote_fetch_on_get_metadata_miss() -> TestResult {
        use lore::storage::get_metadata;
        use lore::storage::get_metadata::LoreStorageGetMetadataArgs;
        use lore::storage::get_metadata::LoreStorageGetMetadataItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-local-getmd".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let (partition, address) =
                        seed_server_only(&server, 0xc2, b"bound-local getmd-miss target").await;
                    let bound = LoreGlobalArgs {
                        local: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetMetadataItemComplete(data) = event {
                            captured_for_cb
                                .lock()
                                .unwrap()
                                .push((data.id, data.error_code));
                        }
                    }));
                    let _ = get_metadata::get_metadata(
                        LoreGlobalArgs::default(),
                        LoreStorageGetMetadataArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageGetMetadataItem {
                                id: 22,
                                partition,
                                address,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].1, LoreErrorCode::AddressNotFound);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Suppression-on-upload: bound `offline=1` rejects upload up front rather than letting
    /// the call slip through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_offline_rejects_upload_pre_dispatch() -> TestResult {
        use lore::storage::upload;
        use lore::storage::upload::LoreStorageUploadArgs;
        use lore::storage::upload::LoreStorageUploadItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-offline-upload".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let bound = LoreGlobalArgs {
                        offline: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    let item = LoreStorageUploadItem {
                        id: 23,
                        partition: Partition::from([0xc3u8; 16]),
                        address: Address {
                            hash: Hash::from([0xeeu8; 32]),
                            context: Context::default(),
                        },
                    };
                    let status = upload::upload(
                        LoreGlobalArgs::default(),
                        LoreStorageUploadArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![item]),
                        },
                        None,
                    )
                    .await;
                    assert_eq!(
                        status,
                        lore_base::error::InvalidArguments::FFI_CODE,
                        "upload on bound-offline handle must reject"
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Suppression-on-obliterate: bound `local=1` makes obliterate run only the local leg;
    /// the remote leg is reported as `remote_skipped=1` (not `remote_success`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_local_suppresses_remote_obliterate() -> TestResult {
        use lore::storage::obliterate;
        use lore::storage::obliterate::LoreStorageObliterateArgs;
        use lore::storage::obliterate::LoreStorageObliterateItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-local-obliterate".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let bound = LoreGlobalArgs {
                        local: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    #[allow(clippy::type_complexity)]
                    let captured: Arc<Mutex<Vec<(u8, u8, u8, u8)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageObliterateItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push((
                                data.local_success,
                                data.remote_success,
                                data.local_skipped,
                                data.remote_skipped,
                            ));
                        }
                    }));
                    let _ = obliterate::obliterate(
                        LoreGlobalArgs::default(),
                        LoreStorageObliterateArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageObliterateItem {
                                id: 24,
                                partition: Partition::from([0xc4u8; 16]),
                                address: Address {
                                    hash: Hash::from([0x01u8; 32]),
                                    context: Context::default(),
                                },
                            }]),
                        },
                        callback,
                    )
                    .await;
                    let events = captured.lock().unwrap().clone();
                    assert_eq!(events.len(), 1);
                    let (local_success, remote_success, local_skipped, remote_skipped) = events[0];
                    assert_eq!(local_success, 1);
                    assert_eq!(local_skipped, 0);
                    assert_eq!(remote_success, 0);
                    assert_eq!(remote_skipped, 1);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Suppression-on-copy: bound `offline=1` degrades copy to local-only — the destination
    /// is not durably confirmed on the peer, but the call succeeds locally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_offline_degrades_copy_to_local_only() -> TestResult {
        use lore::storage::copy;
        use lore::storage::copy::LoreStorageCopyArgs;
        use lore::storage::copy::LoreStorageCopyItem;
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;
        use lore_storage::immutable_store::query_one;
        use lore_storage::store_types::StoreMatch;

        let execution = setup_execution("storage-remote-bound-offline-copy".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let bound = LoreGlobalArgs {
                        offline: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    let payload = b"bound-offline copy source".to_vec();
                    let source_partition = Partition::from([0xc5u8; 16]);
                    let source_context = Context::from([0xa5u8; 16]);
                    let source_address = put_local_with_context(
                        handle_id,
                        source_partition,
                        source_context,
                        payload.as_slice(),
                    )
                    .await;

                    let target_partition = Partition::from([0xc6u8; 16]);
                    let target_context = Context::from([0xa6u8; 16]);

                    let captured: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageCopyItemComplete(data) = event {
                            captured_for_cb.lock().unwrap().push(data.error_code);
                        }
                    }));
                    let status = copy::copy(
                        LoreGlobalArgs::default(),
                        LoreStorageCopyArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageCopyItem {
                                id: 25,
                                source_partition,
                                source_address,
                                target_partition,
                                target_context,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0);
                    assert_eq!(captured.lock().unwrap()[0], LoreErrorCode::None);
                    drop(payload);

                    let server_match = query_one(
                        &server.backend_immutable.clone(),
                        target_partition,
                        lore_base::types::Address {
                            hash: source_address.hash,
                            context: target_context,
                        },
                    )
                    .await
                    .expect("backend exist");
                    assert_eq!(
                        server_match.match_made,
                        StoreMatch::MatchNone,
                        "bound-offline copy must not reach the remote",
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Bound `remote=1`: read bypasses the local cache and reaches the remote even when the
    /// local side has the address. Seed both sides with the same address but different
    /// payloads so we can distinguish which side served the read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_remote_bypasses_local_cache_on_get() -> TestResult {
        use bytes::Bytes;
        use lore::storage::get;
        use lore::storage::get::LoreStorageGetArgs;
        use lore::storage::get::LoreStorageGetItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-remote-get".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;

                    let payload_bytes = b"served-from-remote".to_vec();
                    let payload = Bytes::from(payload_bytes.clone());
                    let hash = lore_storage::hash::hash_slice(&payload_bytes);
                    let address = Address {
                        hash,
                        context: Context::from([0xd0u8; 16]),
                    };
                    let partition = Partition::from([0xd1u8; 16]);
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };
                    server
                        .backend_immutable
                        .clone()
                        .put(partition, address, fragment, Some(payload.clone()), false)
                        .await
                        .expect("seed server");

                    let bound = LoreGlobalArgs {
                        remote: 1,
                        ..Default::default()
                    };
                    let handle_id = open_remote_handle_with_globals(&server, bound).await;

                    let received: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                    let received_for_cb = received.clone();
                    let outcomes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let callback: LoreEventCallback =
                        Some(Box::new(move |event: &LoreEvent| match event {
                            LoreEvent::StorageGetData(data) => {
                                let slice = unsafe {
                                    std::slice::from_raw_parts(
                                        data.bytes.ptr.cast::<u8>(),
                                        data.bytes.len,
                                    )
                                };
                                received_for_cb.lock().unwrap().extend_from_slice(slice);
                            }
                            LoreEvent::StorageGetItemComplete(data) => {
                                outcomes_for_cb.lock().unwrap().push(data.error_code);
                            }
                            _ => {}
                        }));
                    let status = get::get(
                        LoreGlobalArgs::default(),
                        LoreStorageGetArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageGetItem {
                                id: 26,
                                partition,
                                address,
                                streaming: 0,
                                local_cache: 0,
                                ..Default::default()
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0);
                    assert_eq!(outcomes.lock().unwrap()[0], LoreErrorCode::None);
                    assert_eq!(*received.lock().unwrap(), payload_bytes);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Bound `remote=1`: copy items only attempt server-side. With a destination tuple that
    /// the server has NOT seen, tier-1 returns `NotFound` and the upload-fallback (tier 2)
    /// is suppressed — the result is `AddressNotFound` rather than a fallback success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bound_remote_skips_copy_upload_fallback() -> TestResult {
        use lore::storage::copy;
        use lore::storage::copy::LoreStorageCopyArgs;
        use lore::storage::copy::LoreStorageCopyItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-bound-remote-copy".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                let server = start_server(transport).await;
                let bound = LoreGlobalArgs {
                    remote: 1,
                    ..Default::default()
                };
                let handle_id = open_remote_handle_with_globals(&server, bound).await;

                let captured: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                let captured_for_cb = captured.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageCopyItemComplete(data) = event {
                        captured_for_cb.lock().unwrap().push(data.error_code);
                    }
                }));
                let _ = copy::copy(
                    LoreGlobalArgs::default(),
                    LoreStorageCopyArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageCopyItem {
                            id: 27,
                            source_partition: Partition::from([0xd2u8; 16]),
                            source_address: Address {
                                hash: Hash::from([0xefu8; 32]),
                                context: Context::default(),
                            },
                            target_partition: Partition::from([0xd3u8; 16]),
                            target_context: Context::from([0xa7u8; 16]),
                        }]),
                    },
                    callback,
                )
                .await;
                let events = captured.lock().unwrap().clone();
                assert_eq!(events.len(), 1);
                assert_eq!(
                    events[0],
                    LoreErrorCode::AddressNotFound,
                    "bound-remote copy must surface NotFound rather than fall through to upload",
                );

                close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Helper for the copy-suppression test: drive a local-only put against the handle with
    /// a caller-supplied context (the existing `put_local_via_handle` uses `Context::default`),
    /// then pull the resulting address out of the per-item event so subsequent ops can target
    /// it.
    async fn put_local_with_context(
        handle_id: u64,
        partition: lore_base::types::Partition,
        context: lore_base::types::Context,
        payload: &[u8],
    ) -> lore_base::types::Address {
        use lore::storage::put;
        use lore::storage::put::LoreStoragePutArgs;
        use lore::storage::put::LoreStoragePutItem;
        use lore_revision::event::LoreBytes;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<lore_base::types::Address>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                captured_for_cb.lock().unwrap().push(data.address);
            }
        }));
        let item = LoreStoragePutItem {
            id: 0,
            partition,
            context,
            data: LoreBytes {
                ptr: payload.as_ptr().cast(),
                len: payload.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let status = put::put(
            LoreGlobalArgs::default(),
            LoreStoragePutArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "put failed");
        captured.lock().unwrap()[0]
    }

    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_revision::event::LoreErrorCode;

    const REMOTE_KEY_TYPE: KeyType = KeyType::BranchLatestPointer;

    fn remote_globals() -> LoreGlobalArgs {
        LoreGlobalArgs {
            remote: 1,
            ..Default::default()
        }
    }

    /// Store a key-value pair through the handle, routed by `globals`. Returns the per-item
    /// `(id, error_code)` outcomes.
    async fn mutable_store_via_handle(
        handle_id: u64,
        globals: LoreGlobalArgs,
        partition: lore_base::types::Partition,
        key: Hash,
        value: Hash,
    ) -> (i32, Vec<(u64, lore_revision::event::LoreErrorCode)>) {
        use lore::storage::mutable_store;
        use lore::storage::mutable_store::LoreStorageMutableStoreArgs;
        use lore::storage::mutable_store::LoreStorageMutableStoreItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageMutableStoreItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.error_code));
            }
        }));
        let status = mutable_store::mutable_store(
            globals,
            LoreStorageMutableStoreArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageMutableStoreItem {
                    id: 1,
                    partition,
                    key,
                    value,
                    key_type: REMOTE_KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Load a key through the handle, routed by `globals`. Returns the per-item `(value,
    /// error_code)`.
    async fn mutable_load_via_handle(
        handle_id: u64,
        globals: LoreGlobalArgs,
        partition: lore_base::types::Partition,
        key: Hash,
    ) -> (i32, Hash, lore_revision::event::LoreErrorCode) {
        use lore::storage::mutable_load;
        use lore::storage::mutable_load::LoreStorageMutableLoadArgs;
        use lore::storage::mutable_load::LoreStorageMutableLoadItem;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let captured: Arc<Mutex<Vec<(Hash, LoreErrorCode)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageMutableLoadItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.value, data.error_code));
            }
        }));
        let status = mutable_load::mutable_load(
            globals,
            LoreStorageMutableLoadArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(vec![LoreStorageMutableLoadItem {
                    id: 7,
                    partition,
                    key,
                    key_type: REMOTE_KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        assert_eq!(events.len(), 1, "exactly one load complete event");
        (status, events[0].0, events[0].1)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutable_store_and_load_via_remote_round_trips() -> TestResult {
        let execution = setup_execution("storage-remote-mutable-roundtrip".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;
                    let partition = lore_base::types::Partition::from([0xe1u8; 16]);
                    let key = Hash::from([0xe2u8; 32]);
                    let value = Hash::from([0xe3u8; 32]);

                    let (status, completes) = mutable_store_via_handle(
                        handle_id,
                        remote_globals(),
                        partition,
                        key,
                        value,
                    )
                    .await;
                    assert_eq!(status, 0, "remote mutable store must succeed");
                    assert_eq!(completes, vec![(1, LoreErrorCode::None)]);

                    let on_server = server
                        .backend_mutable
                        .clone()
                        .load(partition, key, REMOTE_KEY_TYPE)
                        .await
                        .expect("server backend must hold the stored key");
                    assert_eq!(on_server, value, "server value must match the stored value");

                    let (load_status, loaded, code) =
                        mutable_load_via_handle(handle_id, remote_globals(), partition, key).await;
                    assert_eq!(load_status, 0);
                    assert_eq!(code, LoreErrorCode::None);
                    assert_eq!(loaded, value);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutable_compare_and_swap_via_remote() -> TestResult {
        use lore::storage::mutable_compare_and_swap;
        use lore::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapArgs;
        use lore::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapItem;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-mutable-cas".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;
                    let partition = lore_base::types::Partition::from([0xf1u8; 16]);
                    let key = Hash::from([0xf2u8; 32]);
                    let current = Hash::from([0xf3u8; 32]);
                    let next = Hash::from([0xf4u8; 32]);

                    let (status, _) = mutable_store_via_handle(
                        handle_id,
                        remote_globals(),
                        partition,
                        key,
                        current,
                    )
                    .await;
                    assert_eq!(status, 0);

                    let captured: Arc<Mutex<Vec<(Hash, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let captured_for_cb = captured.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageMutableCompareAndSwapItemComplete(data) = event {
                            captured_for_cb
                                .lock()
                                .unwrap()
                                .push((data.previous, data.error_code));
                        }
                    }));
                    let cas_status = mutable_compare_and_swap::mutable_compare_and_swap(
                        remote_globals(),
                        LoreStorageMutableCompareAndSwapArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![
                                LoreStorageMutableCompareAndSwapItem {
                                    id: 1,
                                    partition,
                                    key,
                                    expected: current,
                                    value: next,
                                    key_type: REMOTE_KEY_TYPE,
                                },
                            ]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(cas_status, 0, "remote CAS must succeed");
                    let (previous, code) = captured.lock().unwrap()[0];
                    assert_eq!(code, LoreErrorCode::None);
                    assert_eq!(
                        previous, current,
                        "previous must equal the matched expected"
                    );

                    let on_server = server
                        .backend_mutable
                        .clone()
                        .load(partition, key, REMOTE_KEY_TYPE)
                        .await
                        .expect("server must hold the swapped value");
                    assert_eq!(on_server, next, "server value must reflect the swap");

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutable_load_via_remote_missing_returns_not_found() -> TestResult {
        let execution = setup_execution("storage-remote-mutable-miss".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;
                    let partition = lore_base::types::Partition::from([0xa1u8; 16]);
                    let key = Hash::from([0xa2u8; 32]);

                    let (status, value, code) =
                        mutable_load_via_handle(handle_id, remote_globals(), partition, key).await;
                    assert_ne!(status, 0);
                    assert_eq!(code, LoreErrorCode::AddressNotFound);
                    assert_eq!(value, Hash::default());

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutable_default_routing_is_local_not_remote() -> TestResult {
        let execution = setup_execution("storage-remote-mutable-localdefault".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;
                    let partition = lore_base::types::Partition::from([0xb1u8; 16]);
                    let key = Hash::from([0xb2u8; 32]);
                    let value = Hash::from([0xb3u8; 32]);

                    let (status, completes) = mutable_store_via_handle(
                        handle_id,
                        LoreGlobalArgs::default(),
                        partition,
                        key,
                        value,
                    )
                    .await;
                    assert_eq!(status, 0);
                    assert_eq!(completes, vec![(1, LoreErrorCode::None)]);

                    let server_result = server
                        .backend_mutable
                        .clone()
                        .load(partition, key, REMOTE_KEY_TYPE)
                        .await;
                    assert!(
                        server_result.is_err(),
                        "default-routed store must stay local, not reach the server",
                    );

                    let (_s, local_value, local_code) = mutable_load_via_handle(
                        handle_id,
                        LoreGlobalArgs::default(),
                        partition,
                        key,
                    )
                    .await;
                    assert_eq!(local_code, LoreErrorCode::None);
                    assert_eq!(local_value, value);
                    let (_s2, _v, remote_code) =
                        mutable_load_via_handle(handle_id, remote_globals(), partition, key).await;
                    assert_eq!(remote_code, LoreErrorCode::AddressNotFound);

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutable_list_via_remote_returns_error() -> TestResult {
        use lore::storage::mutable_list;
        use lore::storage::mutable_list::LoreStorageMutableListArgs;
        use lore::storage::mutable_list::LoreStorageMutableListItem;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-mutable-list".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let handle_id = open_remote_handle(&server).await;
                    let partition = lore_base::types::Partition::from([0xc1u8; 16]);

                    let entries: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
                    let complete: Arc<Mutex<Option<LoreErrorCode>>> = Arc::new(Mutex::new(None));
                    let entries_for_cb = entries.clone();
                    let complete_for_cb = complete.clone();
                    let callback: LoreEventCallback =
                        Some(Box::new(move |event: &LoreEvent| match event {
                            LoreEvent::StorageMutableListEntry(_) => {
                                *entries_for_cb.lock().unwrap() += 1;
                            }
                            LoreEvent::StorageMutableListItemComplete(data) => {
                                *complete_for_cb.lock().unwrap() = Some(data.error_code);
                            }
                            _ => {}
                        }));
                    let status = mutable_list::mutable_list(
                        remote_globals(),
                        LoreStorageMutableListArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStorageMutableListItem {
                                id: 5,
                                partition,
                                key_type: REMOTE_KEY_TYPE,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        status,
                        lore_base::error::InvalidArguments::FFI_CODE,
                        "remote list must fail the call"
                    );
                    assert_eq!(
                        *entries.lock().unwrap(),
                        0,
                        "no entries on a rejected remote list"
                    );
                    assert!(
                        complete.lock().unwrap().is_none(),
                        "no per-item terminal event on a pre-dispatch rejection"
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Seed one `Resolve` mapping and the blob it names on the server, returning the key and the
    /// payload bytes. The blob's hash is the real hash of the payload, so the server accepts the
    /// put and the client's verification passes.
    async fn seed_resolvable_key(
        server: &TestServer,
        partition: lore_base::types::Partition,
        label: &[u8],
    ) -> (lore_base::types::Hash, Vec<u8>) {
        use bytes::Bytes;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Fragment;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;

        let payload_bytes = label.to_vec();
        let payload = Bytes::from(payload_bytes.clone());
        let resolved = lore_storage::hash_slice(payload.as_ref());
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        server
            .backend_immutable
            .clone()
            .put(
                partition,
                Address {
                    hash: resolved,
                    context: Context::default(),
                },
                fragment,
                Some(payload),
                false,
            )
            .await
            .expect("seed server with resolved blob");

        let key = Hash::hash_buffer(label);
        server
            .backend_mutable
            .clone()
            .store(partition, key, resolved, KeyType::Resolve)
            .await
            .expect("seed server with resolve mapping");

        (key, payload_bytes)
    }

    /// A missing key must not take the rest of the batch down with it.
    ///
    /// The per-item failure travels in the response's `status` field. Were it an `Err(Status)`
    /// stream item instead, tonic would convert the first one to HTTP/2 trailers and end the
    /// stream, so `present` would never be answered — silently, as an absent event rather than an
    /// error. A miss is the expected outcome for `get_resolved`, which is what makes this the
    /// regression worth pinning.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_resolved_miss_does_not_end_the_stream() -> TestResult {
        use lore::storage::get_resolved;
        use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
        use lore::storage::get_resolved::LoreStorageGetResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-get-resolved-miss".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                let server = start_server(transport).await;
                let partition = Partition::from([0xd1u8; 16]);
                let (present, payload_bytes) =
                    seed_resolvable_key(&server, partition, b"resolved-present").await;
                let missing = Hash::hash_buffer(b"resolved-missing-never-seeded");

                let handle_id = open_remote_handle(&server).await;

                let received: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                let received_for_cb = received.clone();
                let outcomes: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let outcomes_for_cb = outcomes.clone();
                let callback: LoreEventCallback =
                    Some(Box::new(move |event: &LoreEvent| match event {
                        LoreEvent::StorageGetData(data) => {
                            let slice = unsafe {
                                std::slice::from_raw_parts(
                                    data.bytes.ptr.cast::<u8>(),
                                    data.bytes.len,
                                )
                            };
                            received_for_cb.lock().unwrap().extend_from_slice(slice);
                        }
                        LoreEvent::StorageGetItemComplete(data) => {
                            outcomes_for_cb
                                .lock()
                                .unwrap()
                                .push((data.id, data.error_code));
                        }
                        _ => {}
                    }));

                let items = vec![
                    LoreStorageGetResolvedItem {
                        id: 1,
                        partition,
                        key: missing,
                        context: Context::default(),
                        local_cache: 0,
                        streaming: 0,
                    },
                    LoreStorageGetResolvedItem {
                        id: 2,
                        partition,
                        key: present,
                        context: Context::default(),
                        local_cache: 0,
                        streaming: 0,
                    },
                ];
                get_resolved::get_resolved(
                    LoreGlobalArgs::default(),
                    LoreStorageGetResolvedArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(items),
                    },
                    callback,
                )
                .await;

                let mut outcomes = outcomes.lock().unwrap().clone();
                outcomes.sort_by_key(|(id, _)| *id);
                assert_eq!(
                    outcomes.len(),
                    2,
                    "both items must report a terminal event; a missing one means the stream died"
                );
                assert_eq!(
                    outcomes[0],
                    (1, LoreErrorCode::AddressNotFound),
                    "the unseeded key must report a miss"
                );
                assert_eq!(
                    outcomes[1],
                    (2, LoreErrorCode::None),
                    "the seeded key must still be served after the miss"
                );

                let received = received.lock().unwrap().clone();
                assert_eq!(
                    received, payload_bytes,
                    "the surviving item must deliver its payload"
                );

                let followup: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let followup_for_cb = followup.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetItemComplete(data) = event {
                        followup_for_cb
                            .lock()
                            .unwrap()
                            .push((data.id, data.error_code));
                    }
                }));
                get_resolved::get_resolved(
                    LoreGlobalArgs::default(),
                    LoreStorageGetResolvedArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStorageGetResolvedItem {
                            id: 3,
                            partition,
                            key: present,
                            context: Context::default(),
                            local_cache: 0,
                            streaming: 0,
                        }]),
                    },
                    callback,
                )
                .await;

                let followup = followup.lock().unwrap().clone();
                assert_eq!(
                    followup,
                    vec![(3, LoreErrorCode::None)],
                    "the session must still serve resolves after an earlier miss"
                );

                close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Publish through `put_resolved`, read back through `get_resolved`, both via the public API
    /// against a real server. The only test where the key under test has a real producer; the
    /// rest seed the server's mutable store directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_resolved_then_get_resolved_round_trips_via_remote() -> TestResult {
        use lore::storage::get_resolved;
        use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
        use lore::storage::get_resolved::LoreStorageGetResolvedItem;
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-put-get-resolved".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd3u8; 16]);
                    let key = Hash::hash_buffer(b"round-trip-key");
                    let payload = b"published through put_resolved".to_vec();

                    let handle_id = open_remote_handle(&server).await;

                    let put_outcomes: PutOutcomes = Arc::new(Mutex::new(Vec::new()));
                    let put_for_cb = put_outcomes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            put_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                                data.stored_local,
                                data.stored_remote,
                            ));
                        }
                    }));

                    let status = put_resolved::put_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                data: LoreBytes {
                                    ptr: payload.as_ptr().cast(),
                                    len: payload.len(),
                                },
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "put_resolved must succeed");

                    let put_outcomes = put_outcomes.lock().unwrap().clone();
                    assert_eq!(put_outcomes.len(), 1);
                    let (_, published_address, code, stored_local, stored_remote) = put_outcomes[0];
                    assert_eq!(code, LoreErrorCode::None);
                    assert_eq!(
                        stored_remote, 1,
                        "remote_write=1 against a live server must report remote placement"
                    );
                    assert_eq!(
                        stored_local, 0,
                        "local_cache=0 must not retain the payload once it is durable remotely"
                    );
                    assert_eq!(
                        published_address.hash,
                        lore_storage::hash_slice(payload.as_slice()),
                        "the reported address must be the content the key now resolves to"
                    );

                    let server_mapping = server
                        .backend_mutable
                        .clone()
                        .load(partition, key, KeyType::Resolve)
                        .await
                        .expect("put_resolved must publish the key on the server");
                    assert_eq!(server_mapping, published_address.hash);

                    let reader_handle = open_remote_handle(&server).await;
                    let received: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                    let received_for_cb = received.clone();
                    let outcomes: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let callback: LoreEventCallback =
                        Some(Box::new(move |event: &LoreEvent| match event {
                            LoreEvent::StorageGetData(data) => {
                                let slice = unsafe {
                                    std::slice::from_raw_parts(
                                        data.bytes.ptr.cast::<u8>(),
                                        data.bytes.len,
                                    )
                                };
                                received_for_cb.lock().unwrap().extend_from_slice(slice);
                            }
                            LoreEvent::StorageGetItemComplete(data) => {
                                outcomes_for_cb.lock().unwrap().push((
                                    data.id,
                                    data.address,
                                    data.error_code,
                                ));
                            }
                            _ => {}
                        }));

                    get_resolved::get_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetResolvedArgs {
                            handle: lore::storage::handle::LoreStore {
                                handle_id: reader_handle,
                            },
                            items: LoreArray::from_vec(vec![LoreStorageGetResolvedItem {
                                id: 2,
                                partition,
                                key,
                                context: Context::default(),
                                local_cache: 0,
                                streaming: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;

                    let outcomes = outcomes.lock().unwrap().clone();
                    assert_eq!(outcomes.len(), 1);
                    assert_eq!(outcomes[0].2, LoreErrorCode::None, "resolve must succeed");
                    assert_eq!(
                        outcomes[0].1, published_address,
                        "get_resolved must report the address put_resolved published"
                    );
                    assert_eq!(
                        received.lock().unwrap().clone(),
                        payload,
                        "the bytes read back must be the bytes published"
                    );

                    close_handle(reader_handle).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// An empty buffer removes the mapping, so a key's whole lifecycle — publish, resolve,
    /// delete, resolve again — runs through `put_resolved` and `get_resolved` alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_resolved_with_empty_buffer_deletes_the_mapping() -> TestResult {
        use lore::storage::get_resolved;
        use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
        use lore::storage::get_resolved::LoreStorageGetResolvedItem;
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-put-resolved-delete".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd5u8; 16]);
                    let key = Hash::hash_buffer(b"lifecycle-key");
                    let payload = b"published then deleted".to_vec();
                    let handle_id = open_remote_handle(&server).await;

                    let publish = |data: LoreBytes| LoreStoragePutResolvedItem {
                        id: 1,
                        partition,
                        key,
                        context: Context::default(),
                        data,
                        remote_write: 1,
                        local_cache: 0,
                        fixed_size_chunk: 0,
                    };

                    let put_once = async |item: LoreStoragePutResolvedItem| {
                        let codes: Arc<Mutex<Vec<LoreErrorCode>>> =
                            Arc::new(Mutex::new(Vec::new()));
                        let codes_for_cb = codes.clone();
                        let callback: LoreEventCallback =
                            Some(Box::new(move |event: &LoreEvent| {
                                if let LoreEvent::StoragePutItemComplete(data) = event {
                                    codes_for_cb.lock().unwrap().push(data.error_code);
                                }
                            }));
                        put_resolved::put_resolved(
                            LoreGlobalArgs::default(),
                            LoreStoragePutResolvedArgs {
                                handle: lore::storage::handle::LoreStore { handle_id },
                                items: LoreArray::from_vec(vec![item]),
                            },
                            callback,
                        )
                        .await;
                        let codes = codes.lock().unwrap().clone();
                        assert_eq!(codes, vec![LoreErrorCode::None]);
                    };

                    let resolve_once = async || {
                        let codes: Arc<Mutex<Vec<LoreErrorCode>>> =
                            Arc::new(Mutex::new(Vec::new()));
                        let codes_for_cb = codes.clone();
                        let callback: LoreEventCallback =
                            Some(Box::new(move |event: &LoreEvent| {
                                if let LoreEvent::StorageGetItemComplete(data) = event {
                                    codes_for_cb.lock().unwrap().push(data.error_code);
                                }
                            }));
                        get_resolved::get_resolved(
                            LoreGlobalArgs::default(),
                            LoreStorageGetResolvedArgs {
                                handle: lore::storage::handle::LoreStore { handle_id },
                                items: LoreArray::from_vec(vec![LoreStorageGetResolvedItem {
                                    id: 2,
                                    partition,
                                    key,
                                    context: Context::default(),
                                    local_cache: 0,
                                    streaming: 0,
                                }]),
                            },
                            callback,
                        )
                        .await;
                        let codes = codes.lock().unwrap().clone();
                        assert_eq!(codes.len(), 1);
                        codes[0]
                    };

                    put_once(publish(LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    }))
                    .await;
                    assert_eq!(
                        resolve_once().await,
                        LoreErrorCode::None,
                        "the key resolves once published"
                    );

                    put_once(publish(LoreBytes {
                        ptr: std::ptr::null(),
                        len: 0,
                    }))
                    .await;

                    assert_eq!(
                        resolve_once().await,
                        LoreErrorCode::AddressNotFound,
                        "the key must stop resolving once deleted"
                    );
                    assert!(
                        server
                            .backend_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_err(),
                        "the deletion must reach the server, not just the local store"
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// A publish that never reached the remote must not be resolvable there. Pins that
    /// `remote_write` actually gates the remote half rather than everything going up regardless.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_resolved_without_remote_write_stays_local() -> TestResult {
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-put-resolved-local".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd4u8; 16]);
                    let key = Hash::hash_buffer(b"local-only-key");
                    let payload = b"never leaves this machine".to_vec();

                    let handle_id = open_remote_handle(&server).await;

                    let outcomes: Arc<Mutex<Vec<(LoreErrorCode, u8, u8)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            outcomes_for_cb.lock().unwrap().push((
                                data.error_code,
                                data.stored_local,
                                data.stored_remote,
                            ));
                        }
                    }));

                    put_resolved::put_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                data: LoreBytes {
                                    ptr: payload.as_ptr().cast(),
                                    len: payload.len(),
                                },
                                remote_write: 0,
                                local_cache: 0,
                                fixed_size_chunk: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;

                    assert_eq!(
                        outcomes.lock().unwrap().clone(),
                        vec![(LoreErrorCode::None, 1, 0)],
                        "a local-only publish must report local placement and not remote"
                    );

                    let local_mutable =
                        lore::storage::handle::mutable_for_test(lore::storage::handle::LoreStore {
                            handle_id,
                        })
                        .expect("handle still registered");
                    assert!(
                        local_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_ok(),
                        "the local store must always receive the mapping"
                    );
                    assert!(
                        server
                            .backend_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_err(),
                        "remote_write=0 must not publish the key to the server"
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// `local_cache=1` writes the key->hash mapping back to the local mutable store, so a later
    /// resolve can be served without the network. Default (`local_cache=0`) leaves no mapping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_resolved_caches_mapping_only_when_local_cache_is_set() -> TestResult {
        use lore::storage::get_resolved;
        use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
        use lore::storage::get_resolved::LoreStorageGetResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-get-resolved-cache".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd2u8; 16]);
                    let (cached_key, cached_payload) =
                        seed_resolvable_key(&server, partition, b"resolved-cache-me").await;
                    let (uncached_key, _) =
                        seed_resolvable_key(&server, partition, b"resolved-do-not-cache").await;

                    let handle_id = open_remote_handle(&server).await;

                    let outcomes: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(data) = event {
                            outcomes_for_cb
                                .lock()
                                .unwrap()
                                .push((data.id, data.error_code));
                        }
                    }));

                    let items = vec![
                        LoreStorageGetResolvedItem {
                            id: 1,
                            partition,
                            key: cached_key,
                            context: Context::default(),
                            local_cache: 1,
                            streaming: 0,
                        },
                        LoreStorageGetResolvedItem {
                            id: 2,
                            partition,
                            key: uncached_key,
                            context: Context::default(),
                            local_cache: 0,
                            streaming: 0,
                        },
                    ];
                    get_resolved::get_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(items),
                        },
                        callback,
                    )
                    .await;

                    let outcomes = outcomes.lock().unwrap().clone();
                    assert_eq!(outcomes.len(), 2);
                    assert!(
                        outcomes
                            .iter()
                            .all(|(_, code)| *code == LoreErrorCode::None),
                        "both resolves must succeed against the remote: {outcomes:?}"
                    );

                    let local_mutable =
                        lore::storage::handle::mutable_for_test(lore::storage::handle::LoreStore {
                            handle_id,
                        })
                        .expect("handle still registered");

                    let cached = local_mutable
                        .clone()
                        .load(partition, cached_key, KeyType::Resolve)
                        .await
                        .expect("local_cache=1 must leave a Resolve mapping behind");
                    assert_eq!(
                        cached,
                        lore_storage::hash_slice(cached_payload.as_slice()),
                        "the cached mapping must name the hash the server resolved to"
                    );

                    assert!(
                        local_mutable
                            .clone()
                            .load(partition, uncached_key, KeyType::Resolve)
                            .await
                            .is_err(),
                        "local_cache=0 must not write a mapping to the local mutable store"
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Two keys naming the same content. The second publish finds the content already durable, so
    /// the upload — and with it the fused publish — is skipped; the key must still be written.
    /// Deduplicated content under several keys is the ordinary case for a foreign-keyed cache.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_resolved_publishes_every_key_naming_the_same_content() -> TestResult {
        use lore::storage::get_resolved;
        use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
        use lore::storage::get_resolved::LoreStorageGetResolvedItem;
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-shared-content".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd6u8; 16]);
                    let payload = b"one blob, many keys".to_vec();
                    let expected = lore_storage::hash_slice(payload.as_slice());
                    let keys = [
                        Hash::hash_buffer(b"shared-a"),
                        Hash::hash_buffer(b"shared-b"),
                    ];
                    let handle_id = open_remote_handle(&server).await;

                    for (n, key) in keys.iter().enumerate() {
                        let outs: Arc<Mutex<Vec<(LoreErrorCode, u8)>>> =
                            Arc::new(Mutex::new(Vec::new()));
                        let cb = outs.clone();
                        let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                            if let LoreEvent::StoragePutItemComplete(d) = e {
                                cb.lock().unwrap().push((d.error_code, d.stored_remote));
                            }
                        }));
                        put_resolved::put_resolved(
                            LoreGlobalArgs::default(),
                            LoreStoragePutResolvedArgs {
                                handle: lore::storage::handle::LoreStore { handle_id },
                                items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                                    id: n as u64,
                                    partition,
                                    key: *key,
                                    context: Context::default(),
                                    data: LoreBytes {
                                        ptr: payload.as_ptr().cast(),
                                        len: payload.len(),
                                    },
                                    remote_write: 1,
                                    local_cache: 0,
                                    fixed_size_chunk: 0,
                                }]),
                            },
                            callback,
                        )
                        .await;
                        assert_eq!(
                            outs.lock().unwrap().clone(),
                            vec![(LoreErrorCode::None, 1)],
                            "publish {n} must succeed and report remote placement"
                        );

                        let mapped = server
                            .backend_mutable
                            .clone()
                            .load(partition, *key, KeyType::Resolve)
                            .await
                            .unwrap_or_else(|e| {
                                panic!("key {n} must be published on the server: {e:?}")
                            });
                        assert_eq!(mapped, expected, "key {n} must name the stored content");
                    }

                    let reader = open_remote_handle(&server).await;
                    let codes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let cb = codes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(d) = e {
                            cb.lock().unwrap().push(d.error_code);
                        }
                    }));
                    get_resolved::get_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id: reader },
                            items: LoreArray::from_vec(vec![LoreStorageGetResolvedItem {
                                id: 9,
                                partition,
                                key: keys[1],
                                context: Context::default(),
                                local_cache: 0,
                                streaming: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        codes.lock().unwrap().clone(),
                        vec![LoreErrorCode::None],
                        "the second key must resolve for a client that never published it"
                    );

                    close_handle(reader).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// A remote upload that fails still succeeds locally, and the placement flags are the only
    /// thing that says so — `error_code` stays `NONE`, per `put`'s best-effort remote contract.
    /// The key must be withheld too, since publishing it would name content the server does not
    /// hold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_upload_reports_local_only_and_withholds_the_key() -> TestResult {
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-failed-upload".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd7u8; 16]);
                    let key = Hash::hash_buffer(b"unreachable-server-key");
                    let payload = b"never reaches the server".to_vec();
                    let handle_id = open_remote_handle(&server).await;

                    let backend_mutable = server.backend_mutable.clone();
                    // Awaited, not dropped: the upload below has to meet a server that is gone,
                    // and a signalled-but-still-listening one answers it.
                    server.shutdown().await;

                    let outs: Arc<Mutex<Vec<(LoreErrorCode, u8, u8)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let cb = outs.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(d) = e {
                            cb.lock().unwrap().push((
                                d.error_code,
                                d.stored_local,
                                d.stored_remote,
                            ));
                        }
                    }));
                    put_resolved::put_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                data: LoreBytes {
                                    ptr: payload.as_ptr().cast(),
                                    len: payload.len(),
                                },
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;

                    let outs = outs.lock().unwrap().clone();
                    assert_eq!(outs.len(), 1);
                    let (code, stored_local, stored_remote) = outs[0];
                    assert_eq!(
                        stored_remote, 0,
                        "an upload that could not reach the server must not report remote placement"
                    );
                    assert_eq!(
                        stored_local, 1,
                        "the local write still succeeded, so the content is held locally"
                    );
                    assert_eq!(
                        code,
                        LoreErrorCode::None,
                        "a failed upload is not an error; `put`'s remote write is best-effort"
                    );
                    assert!(
                        backend_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_err(),
                        "the key must not be published when its content never reached the server"
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Content large enough to fragment takes a different route: the leaves upload through the
    /// ordinary write path and the key follows as a separate mapping write, gated on every
    /// fragment having reached the remote. `fixed_size_chunk` forces many small leaves so the
    /// One leaf that fails to upload makes the whole tree not-remote, and the key stays
    /// unpublished.
    ///
    /// `write_fragmented` folds each leaf's placement with `&=`, and `write_resolved` gates the
    /// remote publish on the result. Fold with a union instead and this tree reports remote, the
    /// key is published, and it names content the server only partly holds — a reader resolves
    /// it, gets the root, and dies on the missing leaf.
    ///
    /// Every other placement test has all leaves succeed or all fail, and those two cases give
    /// the same answer under either fold. This is the only mixed tree in the suite, so it is the
    /// only test that can tell them apart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_failed_leaf_makes_the_tree_not_remote_and_withholds_the_key() -> TestResult {
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        const CHUNK: u64 = 64 * 1024;

        let execution = setup_execution("storage-remote-partial-tree".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server_rejecting_one_leaf(CHUNK).await;
                let partition = Partition::from([0xe4u8; 16]);
                let key = Hash::hash_buffer(b"partial-tree-key");
                let payload: Vec<u8> = (0..(CHUNK as u32 * 6)).map(|i| (i % 251) as u8).collect();
                let handle_id = open_remote_handle(&server).await;

                let outs: Arc<Mutex<Vec<(LoreErrorCode, u8)>>> = Arc::new(Mutex::new(Vec::new()));
                let cb = outs.clone();
                let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                    if let LoreEvent::StoragePutItemComplete(d) = e {
                        cb.lock().unwrap().push((d.error_code, d.stored_remote));
                    }
                }));
                put_resolved::put_resolved(
                    LoreGlobalArgs::default(),
                    LoreStoragePutResolvedArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                            id: 1,
                            partition,
                            key,
                            context: Context::default(),
                            data: LoreBytes {
                                ptr: payload.as_ptr().cast(),
                                len: payload.len(),
                            },
                            remote_write: 1,
                            local_cache: 0,
                            fixed_size_chunk: CHUNK,
                        }]),
                    },
                    callback,
                )
                .await;

                let outcomes = outs.lock().unwrap().clone();
                assert_eq!(outcomes.len(), 1, "one item, one completion");
                let (code, stored_remote) = outcomes[0];
                assert_eq!(
                    code,
                    LoreErrorCode::None,
                    "a failed upload still leaves a good local write, as `put` contracts",
                );
                assert_eq!(
                    stored_remote, 0,
                    "a tree missing one leaf on the remote must not report remote placement",
                );

                let resolved = server
                    .backend_mutable
                    .clone()
                    .load(partition, key, KeyType::Resolve)
                    .await;
                let published = match resolved {
                    Ok(hash) => !hash.is_zero(),
                    Err(_) => false,
                };
                assert!(
                    !published,
                    "the key must not be published while its content is only partly on the \
                     server; found {resolved:?}",
                );

                Ok(())
            })
            .await
    }

    /// aggregate is over a real tree rather than a single fragment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_resolved_publishes_fragmented_content() -> TestResult {
        use lore::storage::get_resolved;
        use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
        use lore::storage::get_resolved::LoreStorageGetResolvedItem;
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-fragmented-resolve".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd8u8; 16]);
                    let key = Hash::hash_buffer(b"fragmented-key");
                    let payload: Vec<u8> = (0..(512 * 1024u32)).map(|i| (i % 251) as u8).collect();
                    let handle_id = open_remote_handle(&server).await;

                    let outs: Arc<Mutex<Vec<(LoreErrorCode, u8)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let cb = outs.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(d) = e {
                            cb.lock().unwrap().push((d.error_code, d.stored_remote));
                        }
                    }));
                    put_resolved::put_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                data: LoreBytes {
                                    ptr: payload.as_ptr().cast(),
                                    len: payload.len(),
                                },
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: 64 * 1024,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        outs.lock().unwrap().clone(),
                        vec![(LoreErrorCode::None, 1)],
                        "a fragmented publish must succeed and report the whole tree remote"
                    );

                    assert!(
                        server
                            .backend_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_ok(),
                        "the key must be published for fragmented content too"
                    );

                    let reader = open_remote_handle(&server).await;
                    let got: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                    let got_cb = got.clone();
                    let codes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let codes_cb = codes.clone();
                    let callback: LoreEventCallback =
                        Some(Box::new(move |e: &LoreEvent| match e {
                            LoreEvent::StorageGetData(d) => {
                                let slice = unsafe {
                                    std::slice::from_raw_parts(
                                        d.bytes.ptr.cast::<u8>(),
                                        d.bytes.len,
                                    )
                                };
                                got_cb.lock().unwrap().extend_from_slice(slice);
                            }
                            LoreEvent::StorageGetItemComplete(d) => {
                                codes_cb.lock().unwrap().push(d.error_code);
                            }
                            _ => {}
                        }));
                    get_resolved::get_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id: reader },
                            items: LoreArray::from_vec(vec![LoreStorageGetResolvedItem {
                                id: 2,
                                partition,
                                key,
                                context: Context::default(),
                                local_cache: 0,
                                streaming: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(codes.lock().unwrap().clone(), vec![LoreErrorCode::None]);
                    assert_eq!(
                        got.lock().unwrap().len(),
                        payload.len(),
                        "the reassembled content must match what was published"
                    );
                    assert_eq!(got.lock().unwrap().clone(), payload);

                    close_handle(reader).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// `streaming` delivers the content one leaf at a time rather than as a single buffer, so a
    /// key naming something large does not have to be materialised whole. The bytes and their
    /// order must match what the buffered mode returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_resolved_streams_content_one_fragment_at_a_time() -> TestResult {
        use lore::storage::get_resolved;
        use lore::storage::get_resolved::LoreStorageGetResolvedArgs;
        use lore::storage::get_resolved::LoreStorageGetResolvedItem;
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreBytes;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-resolve-streaming".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xd9u8; 16]);
                    let key = Hash::hash_buffer(b"streamed-key");
                    let payload: Vec<u8> = (0..(512 * 1024u32)).map(|i| (i % 251) as u8).collect();
                    let handle_id = open_remote_handle(&server).await;

                    let put_codes: Arc<Mutex<Vec<LoreErrorCode>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let cb = put_codes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(d) = e {
                            cb.lock().unwrap().push(d.error_code);
                        }
                    }));
                    put_resolved::put_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                data: LoreBytes {
                                    ptr: payload.as_ptr().cast(),
                                    len: payload.len(),
                                },
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: 64 * 1024,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(put_codes.lock().unwrap().clone(), vec![LoreErrorCode::None]);

                    let reader = open_remote_handle(&server).await;
                    let chunks: StreamChunks = Arc::new(Mutex::new(Vec::new()));
                    let chunks_cb = chunks.clone();
                    let header: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
                    let header_cb = header.clone();
                    let codes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let codes_cb = codes.clone();
                    let callback: LoreEventCallback =
                        Some(Box::new(move |e: &LoreEvent| match e {
                            LoreEvent::StorageGetHeader(d) => {
                                header_cb.lock().unwrap().push(d.size_content);
                            }
                            LoreEvent::StorageGetData(d) => {
                                let slice = unsafe {
                                    std::slice::from_raw_parts(
                                        d.bytes.ptr.cast::<u8>(),
                                        d.bytes.len,
                                    )
                                };
                                chunks_cb.lock().unwrap().push((d.offset, slice.to_vec()));
                            }
                            LoreEvent::StorageGetItemComplete(d) => {
                                codes_cb.lock().unwrap().push(d.error_code);
                            }
                            _ => {}
                        }));
                    get_resolved::get_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id: reader },
                            items: LoreArray::from_vec(vec![LoreStorageGetResolvedItem {
                                id: 2,
                                partition,
                                key,
                                context: Context::default(),
                                local_cache: 0,
                                streaming: 1,
                            }]),
                        },
                        callback,
                    )
                    .await;

                    assert_eq!(codes.lock().unwrap().clone(), vec![LoreErrorCode::None]);
                    assert_eq!(
                        header.lock().unwrap().clone(),
                        vec![payload.len() as u64],
                        "the header must carry the whole content size before any data"
                    );

                    let chunks = chunks.lock().unwrap().clone();
                    assert!(
                        chunks.len() > 1,
                        "streaming must deliver more than one event for fragmented content, got {}",
                        chunks.len()
                    );
                    let mut expected_offset = 0u64;
                    let mut assembled = Vec::with_capacity(payload.len());
                    for (offset, bytes) in &chunks {
                        assert_eq!(*offset, expected_offset, "leaf offsets must be contiguous");
                        expected_offset += bytes.len() as u64;
                        assembled.extend_from_slice(bytes);
                    }
                    assert_eq!(
                        assembled, payload,
                        "streamed bytes must match what was published"
                    );

                    close_handle(reader).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// A path inside a fresh `TempDir`, holding `payload` when one is given and not created at all
    /// when none is — a read target must not exist before the op writes it. The directory cleans
    /// its whole tree on Drop, so the caller holds the guard for the scope it needs the path in.
    fn temp_file(tag: &str, payload: Option<&[u8]>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("lore-file-resolved-{tag}-"))
            .tempdir()
            .expect("create tempdir");
        let path = dir.path().join("content");
        if let Some(payload) = payload {
            std::fs::write(&path, payload).expect("write source file");
        }
        (dir, path)
    }

    /// The file-backed pair round-tripping through a real server: a file goes up under a key and
    /// comes back down into another file for a client that never saw the hash. Neither side holds
    /// the content, which is the whole reason these exist next to `put_resolved` / `get_resolved`.
    ///
    /// The reader is a second handle, so the resolve and the read cannot be served from the
    /// writer's local store — the round trip under test is the remote one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_file_resolved_then_get_file_resolved_round_trips_via_remote() -> TestResult {
        use lore::storage::get_file_resolved;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedArgs;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedItem;
        use lore::storage::put_file_resolved;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-file-resolved-round-trip".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xdau8; 16]);
                    let key = Hash::hash_buffer(b"file-round-trip-key");
                    let payload = b"published from a file through the wire".to_vec();
                    let (_source_guard, source) = temp_file("source", Some(&payload));

                    let handle_id = open_remote_handle(&server).await;

                    let put_outcomes: PutOutcomes = Arc::new(Mutex::new(Vec::new()));
                    let put_for_cb = put_outcomes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(data) = event {
                            put_for_cb.lock().unwrap().push((
                                data.id,
                                data.address,
                                data.error_code,
                                data.stored_local,
                                data.stored_remote,
                            ));
                        }
                    }));

                    let status = put_file_resolved::put_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutFileResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(source.display().to_string().as_str()),
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "put_file_resolved must succeed");

                    let put_outcomes = put_outcomes.lock().unwrap().clone();
                    assert_eq!(put_outcomes.len(), 1);
                    let (_, published_address, code, _stored_local, stored_remote) =
                        put_outcomes[0];
                    assert_eq!(code, LoreErrorCode::None);
                    assert_eq!(
                        stored_remote, 1,
                        "remote_write=1 against a live server must report remote placement"
                    );
                    assert_eq!(
                        published_address.hash,
                        lore_storage::hash_slice(payload.as_slice()),
                        "the reported address must be the content the key now resolves to"
                    );

                    let server_mapping = server
                        .backend_mutable
                        .clone()
                        .load(partition, key, KeyType::Resolve)
                        .await
                        .expect("put_file_resolved must publish the key on the server");
                    assert_eq!(server_mapping, published_address.hash);

                    let reader_handle = open_remote_handle(&server).await;
                    let (_target_guard, target) = temp_file("target", None);
                    let outcomes: Arc<Mutex<Vec<(u64, Address, LoreErrorCode)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let outcomes_for_cb = outcomes.clone();
                    let data_events = Arc::new(Mutex::new(0usize));
                    let data_for_cb = data_events.clone();
                    let callback: LoreEventCallback =
                        Some(Box::new(move |event: &LoreEvent| match event {
                            LoreEvent::StorageGetData(_) | LoreEvent::StorageGetHeader(_) => {
                                *data_for_cb.lock().unwrap() += 1;
                            }
                            LoreEvent::StorageGetItemComplete(data) => {
                                outcomes_for_cb.lock().unwrap().push((
                                    data.id,
                                    data.address,
                                    data.error_code,
                                ));
                            }
                            _ => {}
                        }));

                    let status = get_file_resolved::get_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore {
                                handle_id: reader_handle,
                            },
                            items: LoreArray::from_vec(vec![LoreStorageGetFileResolvedItem {
                                id: 2,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(target.display().to_string().as_str()),
                                offset: 0,
                                length: 0,
                                local_cache: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(status, 0, "get_file_resolved must succeed");

                    let outcomes = outcomes.lock().unwrap().clone();
                    assert_eq!(outcomes.len(), 1);
                    assert_eq!(outcomes[0].2, LoreErrorCode::None, "resolve must succeed");
                    assert_eq!(
                        outcomes[0].1, published_address,
                        "get_file_resolved must report the address put_file_resolved published"
                    );
                    assert_eq!(
                        *data_events.lock().unwrap(),
                        0,
                        "the content goes to the file, so no HEADER or DATA event may be emitted"
                    );
                    assert_eq!(
                        std::fs::read(&target).expect("read target file"),
                        payload,
                        "the file written must hold the bytes published"
                    );

                    close_handle(reader_handle).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// A file too large for one fragment. On the way up it chunks straight off disk, on the way
    /// down it is written leaf by leaf into the target, and the key is published only once the
    /// whole tree is remote. Nothing on either side is ever content-sized, which is what makes
    /// this pair usable for the payloads `put_resolved`'s buffer cannot carry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_resolved_round_trips_fragmented_content_via_remote() -> TestResult {
        use lore::storage::get_file_resolved;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedArgs;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedItem;
        use lore::storage::put_file_resolved;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-file-resolved-fragmented".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xdbu8; 16]);
                    let key = Hash::hash_buffer(b"file-fragmented-key");
                    let payload: Vec<u8> = (0..(512 * 1024u32)).map(|i| (i % 251) as u8).collect();
                    let (_source_guard, source) = temp_file("fragmented", Some(&payload));

                    let handle_id = open_remote_handle(&server).await;

                    let outs: Arc<Mutex<Vec<(LoreErrorCode, u8)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let cb = outs.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(d) = e {
                            cb.lock().unwrap().push((d.error_code, d.stored_remote));
                        }
                    }));
                    put_file_resolved::put_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutFileResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(source.display().to_string().as_str()),
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: 64 * 1024,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        outs.lock().unwrap().clone(),
                        vec![(LoreErrorCode::None, 1)],
                        "a fragmented file publish must succeed and report the whole tree remote"
                    );

                    assert!(
                        server
                            .backend_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_ok(),
                        "the key must be published for a fragmented file too"
                    );

                    let reader = open_remote_handle(&server).await;
                    let (_target_guard, target) = temp_file("fragmented-target", None);
                    let codes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let codes_cb = codes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(d) = e {
                            codes_cb.lock().unwrap().push(d.error_code);
                        }
                    }));
                    get_file_resolved::get_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id: reader },
                            items: LoreArray::from_vec(vec![LoreStorageGetFileResolvedItem {
                                id: 2,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(target.display().to_string().as_str()),
                                offset: 0,
                                length: 0,
                                local_cache: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(codes.lock().unwrap().clone(), vec![LoreErrorCode::None]);

                    let written = std::fs::read(&target).expect("read target file");
                    assert_eq!(
                        written.len(),
                        payload.len(),
                        "the file must hold the whole content the key names"
                    );
                    assert_eq!(written, payload);

                    close_handle(reader).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// A range of a fragmented key straight into a file, fetched from a server that holds it and
    /// a client that does not. Only the leaves the range covers are fetched, so the file is the
    /// range and nothing else — a partial read of a large blob without a buffer for the blob.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_file_resolved_writes_only_the_range_from_a_remote_key() -> TestResult {
        use lore::storage::get_file_resolved;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedArgs;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedItem;
        use lore::storage::put_file_resolved;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-file-resolved-range".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xdcu8; 16]);
                    let key = Hash::hash_buffer(b"file-range-key");
                    let payload: Vec<u8> = (0..(512 * 1024u32)).map(|i| (i % 241) as u8).collect();
                    let (_source_guard, source) = temp_file("range", Some(&payload));

                    let handle_id = open_remote_handle(&server).await;
                    let codes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let codes_cb = codes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(d) = e {
                            codes_cb.lock().unwrap().push(d.error_code);
                        }
                    }));
                    put_file_resolved::put_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutFileResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(source.display().to_string().as_str()),
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: 64 * 1024,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(codes.lock().unwrap().clone(), vec![LoreErrorCode::None]);

                    let reader = open_remote_handle(&server).await;
                    let (_target_guard, target) = temp_file("range-target", None);
                    let start = 100_000usize;
                    let length = 300_000usize;
                    let codes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let codes_cb = codes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(d) = e {
                            codes_cb.lock().unwrap().push(d.error_code);
                        }
                    }));
                    get_file_resolved::get_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id: reader },
                            items: LoreArray::from_vec(vec![LoreStorageGetFileResolvedItem {
                                id: 2,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(target.display().to_string().as_str()),
                                offset: start as u64,
                                length: length as u64,
                                local_cache: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(codes.lock().unwrap().clone(), vec![LoreErrorCode::None]);

                    let written = std::fs::read(&target).expect("read target file");
                    assert_eq!(written.len(), length, "the file must be sized to the range");
                    assert_eq!(written, payload[start..start + length]);

                    close_handle(reader).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// An empty file retracts the key on the server as well as locally, so the whole lifecycle —
    /// publish from a file, resolve into a file, retract, resolve again — runs through the file
    /// pair alone. The retraction has to reach the remote: with only the local mapping cleared,
    /// the next resolve would fall through and find the server's live mapping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_file_resolved_with_an_empty_file_deletes_the_mapping_remotely() -> TestResult {
        use lore::storage::get_file_resolved;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedArgs;
        use lore::storage::get_file_resolved::LoreStorageGetFileResolvedItem;
        use lore::storage::put_file_resolved;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        let execution = setup_execution("storage-remote-file-resolved-delete".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xddu8; 16]);
                    let key = Hash::hash_buffer(b"file-delete-key");
                    let (_source_guard, source) = temp_file("delete", Some(b"live for now"));
                    let (_empty_guard, empty) = temp_file("delete-empty", Some(b""));

                    let handle_id = open_remote_handle(&server).await;

                    for (id, path) in [(1u64, &source), (2u64, &empty)] {
                        let codes: Arc<Mutex<Vec<LoreErrorCode>>> =
                            Arc::new(Mutex::new(Vec::new()));
                        let codes_cb = codes.clone();
                        let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                            if let LoreEvent::StoragePutItemComplete(d) = e {
                                codes_cb.lock().unwrap().push(d.error_code);
                            }
                        }));
                        put_file_resolved::put_file_resolved(
                            LoreGlobalArgs::default(),
                            LoreStoragePutFileResolvedArgs {
                                handle: lore::storage::handle::LoreStore { handle_id },
                                items: LoreArray::from_vec(vec![LoreStoragePutFileResolvedItem {
                                    id,
                                    partition,
                                    key,
                                    context: Context::default(),
                                    path: LoreString::from(path.display().to_string().as_str()),
                                    remote_write: 1,
                                    local_cache: 0,
                                    fixed_size_chunk: 0,
                                }]),
                            },
                            callback,
                        )
                        .await;
                        assert_eq!(
                            codes.lock().unwrap().clone(),
                            vec![LoreErrorCode::None],
                            "publish {id} must succeed"
                        );
                    }

                    let mapping = server
                        .backend_mutable
                        .clone()
                        .load(partition, key, KeyType::Resolve)
                        .await;
                    assert!(
                        mapping.is_err() || mapping.as_ref().is_ok_and(|hash| hash.is_zero()),
                        "the retraction must have reached the server; found {mapping:?}"
                    );

                    let reader = open_remote_handle(&server).await;
                    let (_target_guard, target) = temp_file("delete-target", None);
                    let codes: Arc<Mutex<Vec<LoreErrorCode>>> = Arc::new(Mutex::new(Vec::new()));
                    let codes_cb = codes.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StorageGetItemComplete(d) = e {
                            codes_cb.lock().unwrap().push(d.error_code);
                        }
                    }));
                    get_file_resolved::get_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStorageGetFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id: reader },
                            items: LoreArray::from_vec(vec![LoreStorageGetFileResolvedItem {
                                id: 3,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(target.display().to_string().as_str()),
                                offset: 0,
                                length: 0,
                                local_cache: 0,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        codes.lock().unwrap().clone(),
                        vec![LoreErrorCode::AddressNotFound],
                        "a retracted key must resolve to nothing for a fresh client"
                    );
                    assert!(
                        !target.exists(),
                        "a miss must leave the target alone rather than create it empty"
                    );

                    close_handle(reader).await;
                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// The publish rides on the upload of the content's top-level fragment rather than costing a
    /// `mutable_store` of its own, at both content sizes and from either source.
    ///
    /// `StoreResult::published` is the only place that distinction surfaces: both routes leave the
    /// key naming the content, so nothing about the resulting store state tells them apart. This
    /// drives `lore_storage` directly against the same server the C API reaches, because the
    /// per-item event carries the placement flags but not this.
    ///
    /// Every case writes distinct content. Content the server already holds uploads nothing, so
    /// there is no command for its key to ride on and the publish falls back to a `mutable_store`
    /// — the case this asserts against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resolved_publish_rides_on_the_top_level_fragment_upload() -> TestResult {
        use lore_base::types::Context;
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_storage::WriteContext;
        use lore_storage::WriteOptions;

        let execution = setup_execution("storage-remote-fused-publish".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xdeu8; 16]);
                    let handle_id = open_remote_handle(&server).await;
                    let handle = lore::storage::handle::LoreStore { handle_id };
                    let immutable = lore::storage::handle::immutable_for_test(handle)
                        .expect("handle still registered");
                    let mutable = lore::storage::handle::mutable_for_test(handle)
                        .expect("handle still registered");
                    let session = lore::storage::handle::session_for_test(handle, partition)
                        .expect("a remote-configured handle resolves a session");

                    // Period 251 against a 64 KiB cut, so no two leaves are the same fragment.
                    let fragmented = |seed: u8| -> Vec<u8> {
                        (0..(4 * FRAGMENT_SIZE_THRESHOLD))
                            .map(|i| ((i % 251) as u8).wrapping_add(seed))
                            .collect()
                    };
                    let options = WriteOptions::default().with_fixed_size_chunk(64 * 1024);

                    for (label, payload) in [
                        ("small", b"one fragment from a buffer".to_vec()),
                        ("fragmented", fragmented(11)),
                    ] {
                        let written = lore_storage::write_resolved(
                            immutable.clone(),
                            mutable.clone(),
                            partition,
                            Hash::hash_buffer(format!("fused-buffer-{label}").as_bytes()),
                            Context::default(),
                            bytes::Bytes::from(payload),
                            options,
                            Some(session.clone()),
                            WriteContext::none(),
                        )
                        .await?;
                        assert!(
                            written.stored_durable,
                            "{label} buffer must reach the remote for the key to be publishable"
                        );
                        assert!(
                            written.published,
                            "{label} buffer: the mapping must ride on the upload, not cost a \
                             mutable_store of its own"
                        );
                    }

                    for (label, payload) in [
                        ("small", b"one fragment from a file".to_vec()),
                        ("fragmented", fragmented(13)),
                    ] {
                        let (_guard, path) = temp_file(label, Some(&payload));
                        let written = lore_storage::write_resolved_from_file(
                            immutable.clone(),
                            mutable.clone(),
                            partition,
                            Hash::hash_buffer(format!("fused-file-{label}").as_bytes()),
                            Context::default(),
                            &path,
                            options,
                            Some(session.clone()),
                            WriteContext::none(),
                        )
                        .await?;
                        assert!(
                            written.stored_durable,
                            "{label} file must reach the remote for the key to be publishable"
                        );
                        assert!(
                            written.published,
                            "{label} file: the mapping must ride on the upload, not cost a \
                             mutable_store of its own"
                        );
                    }

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// Publish `payload` under each of `keys` in one batch, so the items run concurrently against
    /// the same content address and one of them meets the other's in-flight guard.
    async fn publish_keys_concurrently(
        handle_id: u64,
        partition: lore_base::types::Partition,
        keys: &[lore_base::types::Hash],
        payload: &[u8],
    ) -> Vec<(lore_revision::event::LoreErrorCode, u8)> {
        use lore::storage::put_resolved;
        use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
        use lore::storage::put_resolved::LoreStoragePutResolvedItem;
        use lore_base::types::Context;
        use lore_revision::event::LoreBytes;
        use lore_revision::interface::LoreArray;

        let outs: Arc<Mutex<Vec<(lore_revision::event::LoreErrorCode, u8)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let cb = outs.clone();
        let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(d) = e {
                cb.lock().unwrap().push((d.error_code, d.stored_remote));
            }
        }));
        let items = keys
            .iter()
            .enumerate()
            .map(|(n, key)| LoreStoragePutResolvedItem {
                id: n as u64,
                partition,
                key: *key,
                context: Context::default(),
                data: LoreBytes {
                    ptr: payload.as_ptr().cast(),
                    len: payload.len(),
                },
                remote_write: 1,
                local_cache: 0,
                fixed_size_chunk: 0,
            })
            .collect();
        put_resolved::put_resolved(
            LoreGlobalArgs::default(),
            LoreStoragePutResolvedArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        outs.lock().unwrap().clone()
    }

    /// Two keys naming one content, published at the same time. The second writer meets the
    /// first's in-flight guard, and inherits its upload instead of sending the payload again: the
    /// key it owes follows as a `mutable_store`, which costs the round trip its own upload would
    /// have cost and none of the bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_publishes_of_one_content_upload_it_once() -> TestResult {
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-publish-dedup".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let payload = b"one content, two keys, one upload".to_vec();
                let (server, intercept) = start_server_intercepting_puts(|inner| {
                    InterceptPutStore::new(
                        inner,
                        payload.len() as u64,
                        0,
                        Duration::from_millis(300),
                    )
                })
                .await;
                let partition = Partition::from([0xe1u8; 16]);
                let keys = [
                    Hash::hash_buffer(b"dedup-key-a"),
                    Hash::hash_buffer(b"dedup-key-b"),
                ];
                let handle_id = open_remote_handle(&server).await;

                let outs = publish_keys_concurrently(handle_id, partition, &keys, &payload).await;
                assert_eq!(outs.len(), 2);
                for (code, stored_remote) in &outs {
                    assert_eq!(*code, LoreErrorCode::None);
                    assert_eq!(*stored_remote, 1, "both must report the content remote");
                }

                for key in keys {
                    assert!(
                        server
                            .backend_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_ok(),
                        "every key must be published, whichever writer led"
                    );
                }
                assert_eq!(
                    intercept.forwarded(),
                    1,
                    "the follower must inherit the leader's upload rather than send the payload again"
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// A leader that leaves the content *not* durable cannot satisfy a publish, so the writer that
    /// met its guard uploads for itself rather than inheriting a placement it needs and the leader
    /// never achieved. The store rejects the first upload, so the leader fails and the second
    /// writer has to carry the content.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_publish_does_not_inherit_a_leader_that_left_content_unstored() -> TestResult {
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-publish-no-inherit".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let payload = b"the first upload of this is refused".to_vec();
                let (server, intercept) = start_server_intercepting_puts(|inner| {
                    InterceptPutStore::new(
                        inner,
                        payload.len() as u64,
                        1,
                        Duration::from_millis(300),
                    )
                })
                .await;
                let partition = Partition::from([0xe2u8; 16]);
                let keys = [
                    Hash::hash_buffer(b"no-inherit-key-a"),
                    Hash::hash_buffer(b"no-inherit-key-b"),
                ];
                let handle_id = open_remote_handle(&server).await;

                let outs = publish_keys_concurrently(handle_id, partition, &keys, &payload).await;
                assert_eq!(outs.len(), 2);
                for (code, _) in &outs {
                    assert_eq!(
                        *code,
                        LoreErrorCode::None,
                        "a refused upload is not an error; the local write stands"
                    );
                }
                assert_eq!(
                    outs.iter().filter(|(_, remote)| *remote == 1).count(),
                    1,
                    "the writer that carried the content reports it remote, the refused one does not"
                );
                assert_eq!(
                    intercept.forwarded(),
                    1,
                    "the second writer uploaded rather than inheriting the refused leader"
                );

                let mut published = 0;
                for key in keys {
                    if server
                        .backend_mutable
                        .clone()
                        .load(partition, key, KeyType::Resolve)
                        .await
                        .is_ok_and(|hash| !hash.is_zero())
                    {
                        published += 1;
                    }
                }
                assert_eq!(
                    published, 1,
                    "only the key whose own upload carried the content may be published"
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// A file whose blocks repeat produces a tree of identical leaves, so every leaf but one is a
    /// follower on the in-flight guard rather than an uploader. A follower reports the placement
    /// the store settled at, not the one it read before waiting — otherwise the aggregate would
    /// call the tree partly absent and withhold a key naming content the server holds whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_file_resolved_publishes_a_file_whose_leaves_are_identical() -> TestResult {
        use lore::storage::put_file_resolved;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        const CHUNK: u64 = 64 * 1024;

        let execution = setup_execution("storage-remote-identical-leaves".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                for transport in TRANSPORTS {
                    let server = start_server(transport).await;
                    let partition = Partition::from([0xe0u8; 16]);
                    let key = Hash::hash_buffer(b"identical-leaves-key");
                    let payload = vec![0xABu8; CHUNK as usize * 8];
                    let (_source_guard, source) = temp_file("identical", Some(&payload));
                    let handle_id = open_remote_handle(&server).await;

                    let outs: Arc<Mutex<Vec<(LoreErrorCode, u8)>>> =
                        Arc::new(Mutex::new(Vec::new()));
                    let cb = outs.clone();
                    let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                        if let LoreEvent::StoragePutItemComplete(d) = e {
                            cb.lock().unwrap().push((d.error_code, d.stored_remote));
                        }
                    }));
                    put_file_resolved::put_file_resolved(
                        LoreGlobalArgs::default(),
                        LoreStoragePutFileResolvedArgs {
                            handle: lore::storage::handle::LoreStore { handle_id },
                            items: LoreArray::from_vec(vec![LoreStoragePutFileResolvedItem {
                                id: 1,
                                partition,
                                key,
                                context: Context::default(),
                                path: LoreString::from(source.display().to_string().as_str()),
                                remote_write: 1,
                                local_cache: 0,
                                fixed_size_chunk: CHUNK,
                            }]),
                        },
                        callback,
                    )
                    .await;
                    assert_eq!(
                        outs.lock().unwrap().clone(),
                        vec![(LoreErrorCode::None, 1)],
                        "repeated leaves are one upload and many followers, and the tree is still \
                         whole on the remote"
                    );

                    assert!(
                        server
                            .backend_mutable
                            .clone()
                            .load(partition, key, KeyType::Resolve)
                            .await
                            .is_ok(),
                        "the key must be published for a file of repeated blocks"
                    );

                    close_handle(handle_id).await;
                }
                Ok(())
            })
            .await
    }

    /// The withdrawal gate, reached through the file path: `write_fragmented_from_file` has its own
    /// chunking loop, and the key must be withheld when one of the leaves it produced never
    /// reached the remote.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_file_resolved_withholds_the_key_when_a_leaf_fails_to_upload() -> TestResult {
        use lore::storage::put_file_resolved;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
        use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::KeyType;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;
        use lore_revision::interface::LoreArray;

        const CHUNK: u64 = 64 * 1024;

        let execution = setup_execution("storage-remote-file-partial-tree".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server_rejecting_one_leaf(CHUNK).await;
                let partition = Partition::from([0xdfu8; 16]);
                let key = Hash::hash_buffer(b"file-partial-tree-key");
                let payload: Vec<u8> = (0..(CHUNK as u32 * 6)).map(|i| (i % 251) as u8).collect();
                let (_source_guard, source) = temp_file("partial-tree", Some(&payload));
                let handle_id = open_remote_handle(&server).await;

                let outs: Arc<Mutex<Vec<(LoreErrorCode, u8)>>> = Arc::new(Mutex::new(Vec::new()));
                let cb = outs.clone();
                let callback: LoreEventCallback = Some(Box::new(move |e: &LoreEvent| {
                    if let LoreEvent::StoragePutItemComplete(d) = e {
                        cb.lock().unwrap().push((d.error_code, d.stored_remote));
                    }
                }));
                put_file_resolved::put_file_resolved(
                    LoreGlobalArgs::default(),
                    LoreStoragePutFileResolvedArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: LoreArray::from_vec(vec![LoreStoragePutFileResolvedItem {
                            id: 1,
                            partition,
                            key,
                            context: Context::default(),
                            path: LoreString::from(source.display().to_string().as_str()),
                            remote_write: 1,
                            local_cache: 0,
                            fixed_size_chunk: CHUNK,
                        }]),
                    },
                    callback,
                )
                .await;

                assert_eq!(
                    outs.lock().unwrap().clone(),
                    vec![(LoreErrorCode::None, 0)],
                    "a failed leaf leaves a good local write and no remote placement",
                );

                let resolved = server
                    .backend_mutable
                    .clone()
                    .load(partition, key, KeyType::Resolve)
                    .await;
                let published = resolved.as_ref().is_ok_and(|hash| !hash.is_zero());
                assert!(
                    !published,
                    "the key must not be published while its content is only partly on the \
                     server; found {resolved:?}",
                );

                Ok(())
            })
            .await
    }

    // ---------------------------------------------------------------------------
    // Streaming resilience
    //
    // `Get` / `GetMetadata` multiplex every address in a batch onto one bidirectional
    // stream, and a batch's items share a session, so they share that stream. A per-item
    // failure is reported in-band on the item's own response, which is what lets the
    // stream keep serving its siblings. The assertion that discriminates the in-band
    // behaviour from the old terminal-status behaviour is always the *siblings*: when a
    // status ended the stream, they failed with a transport error instead of resolving.
    // ---------------------------------------------------------------------------

    async fn seed_server(
        server: &TestServer,
        partition: lore_base::types::Partition,
        payload: &bytes::Bytes,
    ) -> lore_base::types::Address {
        let address = lore_base::types::Address {
            hash: lore_storage::hash_slice(payload.as_ref()),
            context: lore_base::types::Context::default(),
        };
        let fragment = lore_base::types::Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        server
            .backend_immutable
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("seed server with payload");
        address
    }

    struct GetBatchResults {
        codes: HashMap<u64, lore_revision::event::LoreErrorCode>,
        bytes: HashMap<u64, Vec<u8>>,
        /// `GET_HEADER`'s `size_content` per item — the whole content's size, which a ranged
        /// read cannot infer from the bytes it got back.
        headers: HashMap<u64, u64>,
        /// The content offset each item's first `GET_DATA` carried.
        first_offsets: HashMap<u64, u64>,
    }

    async fn run_get_batch(
        handle_id: u64,
        items: Vec<lore::storage::get::LoreStorageGetItem>,
    ) -> GetBatchResults {
        let codes: Arc<Mutex<HashMap<u64, lore_revision::event::LoreErrorCode>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let bytes: Arc<Mutex<HashMap<u64, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let headers: Arc<Mutex<HashMap<u64, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let first_offsets: Arc<Mutex<HashMap<u64, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let codes_for_cb = codes.clone();
        let bytes_for_cb = bytes.clone();
        let headers_for_cb = headers.clone();
        let offsets_for_cb = first_offsets.clone();

        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::StorageGetHeader(data) => {
                headers_for_cb
                    .lock()
                    .unwrap()
                    .insert(data.id, data.size_content);
            }
            LoreEvent::StorageGetData(data) => {
                let slice = unsafe {
                    std::slice::from_raw_parts(data.bytes.ptr.cast::<u8>(), data.bytes.len)
                };
                offsets_for_cb
                    .lock()
                    .unwrap()
                    .entry(data.id)
                    .or_insert(data.offset);
                bytes_for_cb
                    .lock()
                    .unwrap()
                    .entry(data.id)
                    .or_default()
                    .extend_from_slice(slice);
            }
            LoreEvent::StorageGetItemComplete(data) => {
                codes_for_cb
                    .lock()
                    .unwrap()
                    .insert(data.id, data.error_code);
            }
            _ => {}
        }));

        lore::storage::get::get(
            LoreGlobalArgs::default(),
            lore::storage::get::LoreStorageGetArgs {
                handle: lore::storage::handle::LoreStore { handle_id },
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;

        let codes = codes.lock().unwrap().clone();
        let bytes = bytes.lock().unwrap().clone();
        let headers = headers.lock().unwrap().clone();
        let first_offsets = first_offsets.lock().unwrap().clone();
        GetBatchResults {
            codes,
            bytes,
            headers,
            first_offsets,
        }
    }

    /// A get item asking for `offset..offset + length` of the content.
    fn ranged_get_item(
        id: u64,
        partition: lore_base::types::Partition,
        address: lore_base::types::Address,
        offset: u64,
        length: u64,
        streaming: u8,
    ) -> lore::storage::get::LoreStorageGetItem {
        lore::storage::get::LoreStorageGetItem {
            id,
            partition,
            address,
            offset,
            length,
            streaming,
            ..Default::default()
        }
    }

    fn get_item(
        id: u64,
        partition: lore_base::types::Partition,
        address: lore_base::types::Address,
    ) -> lore::storage::get::LoreStorageGetItem {
        lore::storage::get::LoreStorageGetItem {
            id,
            partition,
            address,
            streaming: 0,
            local_cache: 0,
            ..Default::default()
        }
    }

    /// A remote miss must not disturb the other addresses sharing the Get stream.
    ///
    /// Previously the miss was a stream trailer, which ended the stream and failed the siblings
    /// with an opaque transport error. The absent address is never written anywhere, so it
    /// misses both the handle's local store and the remote, and it is dispatched first so it is
    /// on the wire before any sibling can have been answered — ordering it last would leave the
    /// scheduler to decide whether the siblings were ever exposed to it.
    #[tokio::test]
    async fn get_batch_survives_missing_address() -> TestResult {
        use bytes::Bytes;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-get-miss-batch".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xc1u8; 16]);

                let payloads: Vec<Bytes> = (0..3u8)
                    .map(|i| Bytes::from(format!("in-band miss sibling payload {i}").into_bytes()))
                    .collect();
                let missing = Address {
                    hash: Hash::from([0xdeu8; 32]),
                    context: Context::default(),
                };
                let mut items = vec![get_item(99, partition, missing)];
                for (index, payload) in payloads.iter().enumerate() {
                    let address = seed_server(&server, partition, payload).await;
                    items.push(get_item(index as u64, partition, address));
                }

                let handle_id = open_remote_handle(&server).await;
                let results = run_get_batch(handle_id, items).await;

                assert_eq!(
                    results.codes.get(&99),
                    Some(&LoreErrorCode::AddressNotFound),
                    "the absent address must report AddressNotFound, not a transport error",
                );
                for (index, payload) in payloads.iter().enumerate() {
                    let id = index as u64;
                    assert_eq!(
                        results.codes.get(&id),
                        Some(&LoreErrorCode::None),
                        "item {id} shares the stream with a miss and must still succeed",
                    );
                    assert_eq!(
                        results.bytes.get(&id).map(Vec::as_slice),
                        Some(payload.as_ref()),
                        "item {id} must return its full payload",
                    );
                }

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// A range on content the handle does not hold locally: the fragment crosses the wire whole
    /// — no protocol carries a range — and the range is applied to what came back. The header
    /// still describes the whole content, which is the only way a caller can tell a short read
    /// from a complete one.
    #[tokio::test]
    async fn get_with_a_range_falls_back_to_remote_and_returns_only_the_range() -> TestResult {
        use bytes::Bytes;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-get-range".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xd1u8; 16]);

                let payload = Bytes::from((0..200u32).map(|b| b as u8).collect::<Vec<u8>>());
                let address = seed_server(&server, partition, &payload).await;

                let handle_id = open_remote_handle(&server).await;
                let results = run_get_batch(
                    handle_id,
                    vec![ranged_get_item(1, partition, address, 40, 60, 0)],
                )
                .await;

                assert_eq!(results.codes.get(&1), Some(&LoreErrorCode::None));
                assert_eq!(results.headers.get(&1), Some(&(payload.len() as u64)));
                assert_eq!(results.first_offsets.get(&1), Some(&40));
                assert_eq!(
                    results.bytes.get(&1).map(Vec::as_slice),
                    Some(&payload[40..100]),
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// The same over a fragment tree, which is where a range earns its keep remotely: the
    /// content is uploaded chunked, then read back through a handle whose local store is empty,
    /// so every fragment the range needs is a round trip and every fragment it does not need is
    /// one that never happens.
    ///
    /// Buffered and streaming are driven against the same uploaded content, because they prune
    /// through different code — `read_defragment` and the tree walker — and a range has to mean
    /// the same thing through both.
    #[tokio::test]
    async fn get_with_a_range_over_a_remote_fragment_tree_returns_only_the_range() -> TestResult {
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-get-range-tree".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xd2u8; 16]);

                let content: Vec<u8> = (0..4 * FRAGMENT_SIZE_THRESHOLD)
                    .map(|byte| (byte as u8).wrapping_mul(31))
                    .collect();

                let upload_handle = open_remote_handle(&server).await;
                let address =
                    put_local_and_remote_chunked(upload_handle, partition, &content, 64 * 1024)
                        .await;
                close_handle(upload_handle).await;

                // Starts and ends inside a chunk, so both ends are clipped.
                let start = 100_000u64;
                let length = 300_000u64;
                let expected = &content[start as usize..(start + length) as usize];

                let handle_id = open_remote_handle(&server).await;
                let results = run_get_batch(
                    handle_id,
                    vec![
                        ranged_get_item(1, partition, address, start, length, 0),
                        ranged_get_item(2, partition, address, start, length, 1),
                    ],
                )
                .await;

                for id in [1u64, 2u64] {
                    assert_eq!(
                        results.codes.get(&id),
                        Some(&LoreErrorCode::None),
                        "item {id}"
                    );
                    assert_eq!(
                        results.headers.get(&id),
                        Some(&(content.len() as u64)),
                        "item {id} header must describe the whole content",
                    );
                    assert_eq!(
                        results.first_offsets.get(&id),
                        Some(&start),
                        "item {id} must place its first chunk at the range start",
                    );
                    assert_eq!(
                        results.bytes.get(&id).map(Vec::as_slice),
                        Some(expected),
                        "item {id} must return exactly the range",
                    );
                }

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// The range is validated against the content the remote actually holds, so a start past
    /// the end is rejected on the same terms it would be locally.
    #[tokio::test]
    async fn get_with_an_offset_past_the_end_rejects_over_remote() -> TestResult {
        use bytes::Bytes;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-get-range-past-end".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xd3u8; 16]);

                let payload = Bytes::from_static(b"a short remote payload");
                let address = seed_server(&server, partition, &payload).await;

                let handle_id = open_remote_handle(&server).await;
                let past_end = payload.len() as u64 + 1;
                let results = run_get_batch(
                    handle_id,
                    vec![ranged_get_item(1, partition, address, past_end, 10, 0)],
                )
                .await;

                assert_eq!(
                    results.codes.get(&1),
                    Some(&LoreErrorCode::InvalidArguments),
                );
                assert!(
                    !results.bytes.contains_key(&1),
                    "a rejected range must emit no data",
                );

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// The same guarantee on the `GetMetadata` stream, where a miss is most clearly routine:
    /// `RemoteImmutableStore::get_metadata` maps `NotFound` to `MatchNone`. The miss is
    /// dispatched first for the same reason as in the Get case.
    #[tokio::test]
    async fn get_metadata_batch_survives_missing_address() -> TestResult {
        use bytes::Bytes;
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-get-metadata-miss-batch".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_test_server().await;
                let partition = Partition::from([0xc2u8; 16]);

                let payloads: Vec<Bytes> = (0..3u8)
                    .map(|i| Bytes::from(format!("metadata sibling payload {i}").into_bytes()))
                    .collect();
                let mut items = vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                    id: 99,
                    partition,
                    address: Address {
                        hash: Hash::from([0xadu8; 32]),
                        context: Context::default(),
                    },
                }];
                for (index, payload) in payloads.iter().enumerate() {
                    let address = seed_server(&server, partition, payload).await;
                    items.push(lore::storage::get_metadata::LoreStorageGetMetadataItem {
                        id: index as u64,
                        partition,
                        address,
                    });
                }

                let handle_id = open_remote_handle(&server).await;

                let outcomes: Arc<Mutex<HashMap<u64, (LoreErrorCode, u32)>>> =
                    Arc::new(Mutex::new(HashMap::new()));
                let outcomes_for_cb = outcomes.clone();
                let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::StorageGetMetadataItemComplete(data) = event {
                        outcomes_for_cb
                            .lock()
                            .unwrap()
                            .insert(data.id, (data.error_code, data.fragment.size_payload));
                    }
                }));

                lore::storage::get_metadata::get_metadata(
                    LoreGlobalArgs::default(),
                    lore::storage::get_metadata::LoreStorageGetMetadataArgs {
                        handle: lore::storage::handle::LoreStore { handle_id },
                        items: lore_revision::interface::LoreArray::from_vec(items),
                    },
                    callback,
                )
                .await;

                let outcomes = outcomes.lock().unwrap().clone();
                assert_eq!(
                    outcomes.get(&99).map(|(code, _)| *code),
                    Some(LoreErrorCode::AddressNotFound),
                    "the absent address must report AddressNotFound",
                );
                for (index, payload) in payloads.iter().enumerate() {
                    let id = index as u64;
                    assert_eq!(
                        outcomes.get(&id).map(|(code, _)| *code),
                        Some(LoreErrorCode::None),
                        "metadata item {id} shares the stream with a miss and must still succeed",
                    );
                    assert_eq!(
                        outcomes.get(&id).map(|(_, size)| *size),
                        Some(payload.len() as u32),
                        "metadata item {id} must report the real payload size",
                    );
                }

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }

    /// Answers `get` with `SlowDown` a bounded number of times for one address, then delegates.
    ///
    /// Backpressure is the other routine per-item failure — it maps to `ResourceExhausted`,
    /// also a stream trailer under the old behaviour. Failing only a bounded number of times
    /// keeps the test fast, since the client's retry then succeeds.
    ///
    /// Seeding goes straight into the backing store, which is then wrapped so the server's own
    /// `handle_get` observes the injected `SlowDown` for exactly one of the addresses.
    struct SlowDownOnce {
        inner: Arc<dyn lore_storage::ImmutableStore>,
        target: lore_base::types::Address,
        remaining: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl lore_storage::ImmutableStore for SlowDownOnce {
        // Answered from the wrapped store rather than the trait defaults: these describe how widely
        // the store behind this one reads and reports, and the `query` contract is written against
        // them. A decorator that let them default would misdescribe whatever it wraps.
        fn is_local(&self) -> bool {
            self.inner.is_local()
        }

        fn isolates_partitions(&self) -> bool {
            self.inner.isolates_partitions()
        }

        fn read_scope(&self) -> lore_storage::StoreMatch {
            self.inner.read_scope()
        }

        fn query_scope(&self) -> lore_storage::StoreMatch {
            self.inner.query_scope()
        }

        async fn get(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
        ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
            if address == self.target
                && self
                    .remaining
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |n| n.checked_sub(1),
                    )
                    .is_ok()
            {
                return Err(lore_storage::StoreError::from(lore_base::error::SlowDown));
            }
            self.inner.clone().get(partition, address).await
        }

        async fn query(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            addresses: &[lore_base::types::Address],
            results: &mut [lore_storage::StoreMatchResult],
        ) -> Result<(), lore_storage::StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        /// Forwarded explicitly, as the trait requires: delegating `query` alone would leave the
        /// inner store's own override unused.
        async fn get_metadata(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
        ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        async fn put(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
            fragment: lore_base::types::Fragment,
            payload: Option<bytes::Bytes>,
            force: bool,
        ) -> Result<(), lore_storage::StoreError> {
            self.inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await
        }

        async fn obliterate(
            self: Arc<Self>,
            partition: lore_base::types::Partition,
            address: lore_base::types::Address,
            stats: Arc<lore_storage::StoreObliterateStats>,
        ) -> Result<(), lore_storage::StoreError> {
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
        ) -> Result<usize, lore_storage::StoreError> {
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
        ) -> Result<Option<usize>, lore_storage::StoreError> {
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

        async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), lore_storage::StoreError> {
            self.inner.clone().flush(sync_data).await
        }

        async fn verify(self: Arc<Self>, heal: bool) -> Result<(), lore_storage::StoreError> {
            self.inner.clone().verify(heal).await
        }

        async fn copy(
            self: Arc<Self>,
            source_partition: lore_base::types::Partition,
            source_address: lore_base::types::Address,
            destination_partition: lore_base::types::Partition,
            destination_context: lore_base::types::Context,
            durable: bool,
        ) -> Result<(), lore_storage::StoreError> {
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

    /// A per-item `SlowDown` must not disturb the other addresses sharing the Get stream.
    #[tokio::test]
    async fn get_batch_survives_server_side_slow_down() -> TestResult {
        use bytes::Bytes;
        use lore_base::types::Partition;
        use lore_revision::event::LoreErrorCode;

        let execution = setup_execution("storage-remote-get-slowdown-batch".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let partition = Partition::from([0xc3u8; 16]);
                let (backend_immutable, backend_mutable) = make_backends().await;

                let payloads: Vec<Bytes> = (0..3u8)
                    .map(|i| Bytes::from(format!("slowdown sibling payload {i}").into_bytes()))
                    .collect();
                let slow_payload = Bytes::from_static(b"slowdown target payload");

                let mut addresses = Vec::new();
                for payload in payloads.iter().chain(std::iter::once(&slow_payload)) {
                    let address = lore_base::types::Address {
                        hash: lore_storage::hash_slice(payload.as_ref()),
                        context: lore_base::types::Context::default(),
                    };
                    backend_immutable
                        .clone()
                        .put(
                            partition,
                            address,
                            lore_base::types::Fragment {
                                flags: 0,
                                size_payload: payload.len() as u32,
                                size_content: payload.len() as u64,
                            },
                            Some(payload.clone()),
                            false,
                        )
                        .await
                        .expect("seed backing store");
                    addresses.push(address);
                }
                let slow_address = *addresses.last().unwrap();

                let served: Arc<dyn lore_storage::ImmutableStore> = Arc::new(SlowDownOnce {
                    inner: backend_immutable,
                    target: slow_address,
                    remaining: std::sync::atomic::AtomicUsize::new(1),
                });
                let server = start_test_server_with(served, backend_mutable).await;
                let handle_id = open_remote_handle(&server).await;

                let items = addresses
                    .iter()
                    .enumerate()
                    .map(|(index, address)| get_item(index as u64, partition, *address))
                    .collect();
                let results = run_get_batch(handle_id, items).await;

                for (index, payload) in payloads
                    .iter()
                    .chain(std::iter::once(&slow_payload))
                    .enumerate()
                {
                    let id = index as u64;
                    assert_eq!(
                        results.codes.get(&id),
                        Some(&LoreErrorCode::None),
                        "item {id} must succeed — a per-item SlowDown is retryable and must \
                         leave the stream serving its siblings",
                    );
                    assert_eq!(
                        results.bytes.get(&id).map(Vec::as_slice),
                        Some(payload.as_ref()),
                        "item {id} must return its full payload",
                    );
                }

                close_handle(handle_id).await;
                Ok(())
            })
            .await
    }
}
