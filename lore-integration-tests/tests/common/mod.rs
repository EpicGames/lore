// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
pub(crate) mod net_common {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use bytes::Bytes;
    use lore::storage::close::LoreStorageCloseArgs;
    use lore::storage::handle::LoreStore;
    use lore::storage::open::LoreStorageOpenArgs;
    use lore::storage::open::LoreStorageRemoteConfig;
    use lore::storage::put::LoreStoragePutArgs;
    use lore::storage::put::LoreStoragePutItem;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::HealResult;
    use lore_base::types::Partition;
    use lore_revision::environment::EnvironmentConfig;
    use lore_revision::event::LoreBytes;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
    use lore_server::authnz::repository_catalog::BaselineRepositoryCatalog;
    use lore_server::grpc::server::FeatureSettings;
    use lore_server::grpc::server::GrpcServerBuilder;
    use lore_server::grpc::server::GrpcTimeouts;
    use lore_server::hooks::HookDispatcher;
    use lore_server::settings::BaselineAccess;
    use lore_storage::CorrelationHold;
    use lore_storage::FlushOptions;
    use lore_storage::ImmutableStore;
    use lore_storage::StoreError;
    use lore_storage::StoreGetData;
    use lore_storage::StoreMatch;
    use lore_storage::StoreMatchResult;
    use lore_storage::StoreObliterateStats;
    use lore_storage::gc_event::GcEventSinkRef;
    use lore_storage::immutable_store::CopyBehavior;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::store_lock::StoreGuard;
    use lore_storage::local::store_lock::StoreLock;

    /// How many numbers [`bind_matched_pair`] tries before giving up. Each attempt is a fresh port
    /// from the OS, so this only runs out if UDP is congested across the whole ephemeral range.
    pub(crate) const PORT_PAIR_ATTEMPTS: usize = 100;

    /// A TCP listener and a UDP socket on the same port, both held exclusively.
    ///
    /// A `lore://` server serves gRPC on TCP and QUIC on UDP at one number, and no bind can reserve
    /// a number for the other protocol. So the port is not chosen and then bound twice — it is
    /// taken from the OS on TCP, matched on UDP, and both sockets are handed to the servers already
    /// bound. Nothing can take either between choosing and serving, because there is no such gap.
    ///
    /// Neither socket sets a reuse option, so losing the UDP half is an error rather than a silent
    /// share of somebody else's port; the pair is released and the OS asked for a different number.
    pub(crate) fn bind_matched_pair() -> (std::net::TcpListener, std::net::UdpSocket) {
        for _ in 0..PORT_PAIR_ATTEMPTS {
            let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tcp");
            let port = tcp.local_addr().expect("tcp local addr").port();
            match std::net::UdpSocket::bind(("127.0.0.1", port)) {
                Ok(udp) => return (tcp, udp),
                // Free on TCP, taken on UDP. Drop the listener too: keeping it would only make the
                // OS hand out a different number next time while this one stayed half-held.
                Err(_) => drop(tcp),
            }
        }
        panic!("no port free on both TCP and UDP after {PORT_PAIR_ATTEMPTS} attempts");
    }

    /// Waits until a connect's first back-off, its jitter included, has passed.
    pub(crate) async fn wait_out_connect_back_off() {
        tokio::time::sleep(std::time::Duration::from_millis(
            lore_transport::connection::RECONNECT_START_DELAY
                + lore_base::retry::DEFAULT_JITTER_CLAMP_MS,
        ))
        .await;
    }

    /// The connection to the remote at `url`, as the process-wide connection cache holds it.
    /// `None` once no live connection to it is cached.
    pub(crate) fn cached_connection(
        url: &str,
    ) -> Option<std::sync::Arc<lore_transport::Connection>> {
        let (url, _protocol) = lore_transport::parse(url).expect("server url parses");
        lore_transport::find_connection(
            url.as_str(),
            "",
            false,
            lore_revision::interface::LoreGlobalArgs::default().auth_mode,
        )
    }

    /// Binds a port on the loopback interface and serves gRPC on it as [`spawn_grpc_server`]
    /// does. Returns the server's `grpc://` URL, and what stops it.
    pub(crate) fn start_grpc_server(
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
    ) -> (String, TestServerStop) {
        start_grpc_server_with(
            backend_immutable,
            backend_mutable,
            Arc::new(AllowAllRepositoryAuthorizer),
        )
    }

    /// [`start_grpc_server`] with `authorizer` deciding each request's access.
    pub(crate) fn start_grpc_server_with(
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
        authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> (String, TestServerStop) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binds a port to serve on");
        let addr = listener.local_addr().expect("the listener has an address");
        let stop = spawn_grpc_server_with(listener, backend_immutable, backend_mutable, authorizer);
        (format!("grpc://{addr}"), stop)
    }

    /// Serves gRPC on `listener` until the returned [`TestServerStop`] stops it or is dropped.
    pub(crate) fn spawn_grpc_server(
        listener: std::net::TcpListener,
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
    ) -> TestServerStop {
        spawn_grpc_server_with(
            listener,
            backend_immutable,
            backend_mutable,
            Arc::new(AllowAllRepositoryAuthorizer),
        )
    }

    /// [`spawn_grpc_server`] with `authorizer` deciding each request's access.
    pub(crate) fn spawn_grpc_server_with(
        listener: std::net::TcpListener,
        backend_immutable: Arc<dyn lore_storage::ImmutableStore>,
        backend_mutable: Arc<dyn lore_storage::MutableStore>,
        authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> TestServerStop {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        let serve_ended = Arc::new(AtomicBool::new(false));
        let ended = serve_ended.clone();

        let notification_sender: Arc<dyn lore_revision::notification::NotificationSender> =
            Arc::new(lore_server::notification::local::NotificationSender::default());
        let hook_dispatcher = Arc::new(HookDispatcher::empty());

        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            let repository_catalog = Arc::new(BaselineRepositoryCatalog::new(
                BaselineAccess::Reachable,
                backend_immutable.clone(),
                backend_mutable.clone(),
            ));
            let outcome = GrpcServerBuilder::new()
                .with_environment(EnvironmentConfig::default())
                .with_feature(FeatureSettings::default())
                .with_immutable_store(backend_immutable.clone(), backend_immutable, None)
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
                    GrpcTimeouts {
                        request_handler: Duration::from_secs(30),
                        authorization: Duration::from_secs(30),
                    },
                    Default::default(),
                    Default::default(),
                    None,
                )
                .with_token_verification(
                    lore_server::auth::jwt::TokenVerification::Ignore,
                    authorizer,
                    repository_catalog,
                )
                .unwrap()
                .serve_with_listener(listener, async {
                    shutdown_rx.await.ok();
                })
                .await;
            ended.store(true, Ordering::Release);
            let _ = stopped_tx.send(outcome.map_err(|error| format!("{error:#}")));
        });

        TestServerStop {
            quic: None,
            shutdown: Some(shutdown_tx),
            stopped: Some(stopped_rx),
            serve_ended,
        }
    }

    /// Bound on the wait for a test gRPC server's stop report.
    const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

    /// What stops a test server: the QUIC server of a `lore://` server, the sender whose drop
    /// stops the gRPC server [`spawn_grpc_server`] started, and the receiver of its stop report.
    /// Dropped, it signals the stop and waits for none of it.
    pub(crate) struct TestServerStop {
        quic: Option<lore_server::quic::quinn::QuinnServer>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        stopped: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
        /// Set by the gRPC serve task once it has returned, and with it dropped its listener,
        /// before it reports how it stopped.
        serve_ended: Arc<AtomicBool>,
    }

    impl TestServerStop {
        /// Adds `quic`, the QUIC server of a `lore://` server, to what [`Self::stop`] stops.
        pub(crate) fn with_quic(mut self, quic: lore_server::quic::quinn::QuinnServer) -> Self {
            self.quic = Some(quic);
            self
        }

        /// Whether the gRPC serve task has returned, and with it dropped its listener, so the
        /// server accepts no connection.
        pub(crate) fn serve_ended(&self) -> bool {
            self.serve_ended.load(Ordering::Acquire)
        }

        /// Closes the QUIC server's endpoints and waits until they are idle, then stops the
        /// gRPC server and waits for its report of a clean stop, sent once the serve task holding
        /// the TCP listener has returned. Panics unless the report comes within
        /// [`SHUTDOWN_WAIT`]. A part already stopped is skipped.
        pub(crate) async fn stop(&mut self) {
            if let Some(quic) = self.quic.take() {
                quic.close().await;
            }
            drop(self.shutdown.take());
            if let Some(stopped) = self.stopped.take() {
                tokio::time::timeout(SHUTDOWN_WAIT, stopped)
                    .await
                    .expect("the gRPC server stops")
                    .expect("the gRPC server reports how it stopped")
                    .expect("the gRPC server stops cleanly");
            }
        }
    }

    /// Whether a test server's immutable store serves a payload under a partition it doesn't hold
    /// the payload in.
    pub(crate) enum Partitions {
        /// Serves a payload only under a partition it holds it in, as Lore Server's store does.
        Isolated,
        /// Serves a payload it holds under any partition, as a client's store does.
        Shared,
    }

    /// A fresh in-memory store pair for a test server to serve. The immutable store treats every
    /// fragment it holds as durably stored and protects none from eviction, as Lore Server's does,
    /// and isolates partitions as `partitions` says.
    pub(crate) async fn make_backends(
        partitions: Partitions,
    ) -> (
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
                isolate_partitions: matches!(partitions, Partitions::Isolated),
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

    /// Opens an in-memory storage handle, bound to the remote at `remote_url` when one is given.
    pub(crate) async fn open_handle(remote_url: Option<&str>) -> LoreStore {
        open_handle_with_globals(LoreGlobalArgs::default(), remote_url).await
    }

    /// Opens a storage handle as [`open_handle`] does, with `globals` for the open call. The
    /// handle keeps the flags `globals` sets.
    pub(crate) async fn open_handle_with_globals(
        globals: LoreGlobalArgs,
        remote_url: Option<&str>,
    ) -> LoreStore {
        open_handle_with_args(
            globals,
            LoreStorageOpenArgs {
                in_memory: 1,
                remote_config: LoreStorageRemoteConfig {
                    remote_url: LoreString::from(remote_url.unwrap_or_default()),
                },
                has_remote_config: u8::from(remote_url.is_some()),
                ..Default::default()
            },
        )
        .await
    }

    /// Opens a storage handle with `args`, and `globals` for the open call. Panics unless the open
    /// succeeds and reports its handle.
    pub(crate) async fn open_handle_with_args(
        globals: LoreGlobalArgs,
        args: LoreStorageOpenArgs,
    ) -> LoreStore {
        let opened: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let sink = opened.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageOpened(data) = event {
                *sink.lock().unwrap() = Some(data.handle_id);
            }
        }));
        let status = lore::storage::open::open(globals, args, callback).await;
        assert_eq!(status, 0, "opening the storage handle must succeed");
        let handle_id = opened.lock().unwrap().expect("STORAGE_OPENED must fire");
        LoreStore { handle_id }
    }

    /// Closes the storage handle `handle`. Panics unless the close succeeds.
    pub(crate) async fn close_handle(handle: LoreStore) {
        let status = lore::storage::close::close(
            LoreGlobalArgs::default(),
            LoreStorageCloseArgs {
                handle,
                ..Default::default()
            },
            None,
        )
        .await;
        assert_eq!(status, 0, "close must succeed");
    }

    /// Stores `content` under `context` in `partition` through the storage handle `handle`, as one
    /// put item with the given `remote_write` and `fixed_size_chunk`. Returns the content's
    /// address. Panics unless the put succeeds.
    pub(crate) async fn put_content(
        handle: LoreStore,
        partition: Partition,
        context: Context,
        content: &[u8],
        remote_write: u8,
        fixed_size_chunk: u64,
    ) -> Address {
        let completed: Arc<Mutex<Vec<(Address, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = completed.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                sink.lock()
                    .unwrap()
                    .push((data.address, data.error.error_code));
            }
        }));
        let status = lore::storage::put::put(
            LoreGlobalArgs::default(),
            LoreStoragePutArgs {
                handle,
                items: LoreArray::from_vec(vec![LoreStoragePutItem {
                    id: 1,
                    partition,
                    context,
                    data: LoreBytes {
                        ptr: content.as_ptr().cast(),
                        len: content.len(),
                    },
                    remote_write,
                    local_cache: 0,
                    remote_query: 0,
                    fixed_size_chunk,
                }]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "put must succeed");
        let completed = completed.lock().unwrap();
        assert_eq!(completed.len(), 1, "the put completes its one item once");
        assert_eq!(completed[0].1, 0, "the item must succeed");
        completed[0].0
    }

    /// The counts of the puts with and without a payload, and of the copies, that reached a
    /// [`CountingStore`].
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(crate) struct Traffic {
        pub payload_puts: usize,
        pub empty_puts: usize,
        pub copies: usize,
    }

    /// An immutable store that forwards every method but `get_into` to the store it wraps and
    /// counts what reaches it: the gets, the metadata reads, the puts with and without a payload,
    /// and the copies, with each copy's source.
    /// One built with [`refusing_copy`](Self::refusing_copy) refuses every copy, as a peer that
    /// won't answer one does. Its `get_into` is the trait's default, which reads through `get`, so
    /// every payload read counts as a get.
    pub(crate) struct CountingStore {
        inner: Arc<dyn ImmutableStore>,
        gets: Arc<AtomicUsize>,
        metadata_reads: AtomicUsize,
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
                gets: Arc::default(),
                metadata_reads: AtomicUsize::new(0),
                payload_puts: AtomicUsize::new(0),
                empty_puts: AtomicUsize::new(0),
                copies: AtomicUsize::new(0),
                copy_sources: Mutex::new(Vec::new()),
                refuse_copy,
            })
        }

        /// A counting store over `inner`.
        pub(crate) fn new(inner: Arc<dyn ImmutableStore>) -> Arc<Self> {
            Self::wrapping(inner, false)
        }

        /// A counting store over `inner` that refuses every copy with `AddressNotFound`.
        pub(crate) fn refusing_copy(inner: Arc<dyn ImmutableStore>) -> Arc<Self> {
            Self::wrapping(inner, true)
        }

        /// The counter of the gets that reached the store, shared with the caller.
        pub(crate) fn gets(&self) -> Arc<AtomicUsize> {
            self.gets.clone()
        }

        /// The metadata reads that reached the store since it was built or last reset.
        pub(crate) fn metadata_reads(&self) -> usize {
            self.metadata_reads.load(Ordering::SeqCst)
        }

        /// The puts and copies counted since the store was built or last reset.
        pub(crate) fn traffic(&self) -> Traffic {
            Traffic {
                payload_puts: self.payload_puts.load(Ordering::SeqCst),
                empty_puts: self.empty_puts.load(Ordering::SeqCst),
                copies: self.copies.load(Ordering::SeqCst),
            }
        }

        /// Zeroes the metadata read, put and copy counts and clears the recorded copy sources. The
        /// get count is kept.
        pub(crate) fn reset(&self) {
            self.metadata_reads.store(0, Ordering::SeqCst);
            self.payload_puts.store(0, Ordering::SeqCst);
            self.empty_puts.store(0, Ordering::SeqCst);
            self.copies.store(0, Ordering::SeqCst);
            self.copy_sources.lock().unwrap().clear();
        }

        /// The source of each copy that reached the store since it was built or last reset, in
        /// order.
        pub(crate) fn copy_sources(&self) -> Vec<(Partition, Address)> {
            self.copy_sources.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ImmutableStore for CountingStore {
        async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
            self.inner.clone().hold_for_command().await
        }

        fn try_join_claim(&self) -> Option<StoreGuard> {
            self.inner.try_join_claim()
        }

        fn store_lock(&self) -> Option<Arc<StoreLock>> {
            self.inner.store_lock()
        }

        fn hold_correlation(&self, correlation_id: &str) -> Option<CorrelationHold> {
            self.inner.hold_correlation(correlation_id)
        }

        async fn hold_for_task(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
            self.inner.clone().hold_for_task().await
        }

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
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.inner.clone().get(partition, address).await
        }

        async fn is_available(self: Arc<Self>, timeout: Duration) -> bool {
            self.inner.clone().is_available(timeout).await
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

        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.metadata_reads.fetch_add(1, Ordering::SeqCst);
            self.inner.clone().get_metadata(partition, address).await
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
            sink: Option<GcEventSinkRef>,
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
            sink: Option<GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            self.inner
                .clone()
                .compact(max_size, at, sync_data, sink)
                .await
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            self.inner.clone().compact_resume_at().await
        }

        async fn load_index(self: Arc<Self>) -> Result<(), StoreError> {
            self.inner.clone().load_index().await
        }

        async fn stop_gc(self: Arc<Self>, terminate: bool) {
            self.inner.clone().stop_gc(terminate).await;
        }

        fn max_query_batch(&self) -> Option<usize> {
            self.inner.max_query_batch()
        }

        async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
            self.inner.clone().flush(options).await
        }

        async fn fragment_count(self: Arc<Self>) -> Option<usize> {
            self.inner.clone().fragment_count().await
        }

        async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
            self.inner.clone().verify(heal).await
        }

        async fn heal(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<HealResult, StoreError> {
            self.inner.clone().heal(partition, address).await
        }

        async fn settle(self: Arc<Self>, partition: Partition, address: Address) {
            self.inner.clone().settle(partition, address).await;
        }

        async fn copy(
            self: Arc<Self>,
            source_partition: Partition,
            source_address: Address,
            destination_partition: Partition,
            destination_context: Context,
            behavior: CopyBehavior,
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
                    behavior,
                )
                .await
        }
    }
}

#[cfg(all(test, feature = "integration_tests"))]
pub(crate) mod aws_common {
    use std::error::Error;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use http::HeaderValue;
    use http::Method;
    use lore_aws::aws::ClientCore;
    use lore_aws::aws::creds::CachedCredentials;
    use lore_aws::aws::creds::Credentials;
    use lore_aws::aws::creds::StaticProvider;
    use lore_aws::aws::dynamodb::DynamoProtocol;
    use lore_aws::aws::error::AwsCallError;
    use lore_aws::aws::http::HttpClient;
    use lore_aws::aws::http::HttpRequest;
    use lore_aws::aws::item::AttributeValue;
    use lore_aws::aws::item::ByteStr;
    use lore_aws::aws::item::Item;
    use lore_aws::aws::s3::S3Protocol;
    use lore_aws::clients::AwsClientBuilder;
    use lore_aws::clients::HttpClientSettings;
    use lore_aws::dynamodb::DynamoDb;
    use lore_aws::s3::S3;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE;
    use lore_aws::store::lock_store::*;
    use lore_aws::store::mutable_store::MUTABLE_STORE_DYNAMO_PARTITION_KEY_ATTRIBUTE;
    use lore_aws::store::mutable_store::MUTABLE_STORE_DYNAMO_SORT_KEY_ATTRIBUTE;
    use lore_revision::util::time::RetryPolicy;
    use serde_json::Value;
    use serde_json::json;
    use tracing::info;
    use tracing::warn;
    use url::Url;

    pub const LOCKS_TABLE_NAME: &str = "locks-local";
    pub const STORE_BUCKET_NAME: &str = "lore-immutable-store-local";
    pub const VERSIONED_BUCKET_NAME: &str = "lore-versioned-store-local";
    pub const MUTABLE_STORE_TABLE_NAME: &str = "lore-mutable-store-local";
    pub const FRAGMENTS_TABLE_NAME: &str = "lore-fragments-local";
    pub const FRAGMENT_STATE_TABLE_NAME: &str = "lore-fragment-state-local";
    pub const FRAGMENT_METADATA_TABLE_NAME: &str = "lore-fragment-metadata-local";

    pub(crate) const S3_ENDPOINT: &str = "http://127.0.0.1:9000";
    pub(crate) const DYNAMODB_ENDPOINT: &str = "http://127.0.0.1:9090";

    // NOTE: these credentials are just hardcoded in lore-integration-tests/compose.yaml
    const AWS_ACCESS_KEY_ID: &str = "lorelocal";
    const AWS_SECRET_ACCESS_KEY: &str = "lorelocal";

    /// The region the fixtures and the clients under test sign for.
    ///
    /// Without `-sharedDb`, `DynamoDB` Local keeps a separate database for each access key and
    /// region, so a client signing for another region than the fixture that created a table does
    /// not see the table.
    const REGION: &str = "us-east-1";

    /// The whole-operation budget of a client core built here, retries included.
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Read and write capacity every fixture table is created with. `DynamoDB` Local ignores the
    /// numbers, and provisioned mode requires them.
    const FIXTURE_CAPACITY: u32 = 5000;

    /// Credentials for the lore-native client under test.
    fn credentials() -> Credentials {
        Credentials::new(AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, None, None)
    }

    /// A bare client core pointed at `endpoint_url`, signing for [`REGION`] with the stack's
    /// credentials. `capture_request_id` keeps the request id of a successful response.
    ///
    /// `CreateTable`, `CreateBucket` and `PutBucketVersioning` are not operations the lore-native
    /// client has, so the fixtures send them through a core like this one, with the request bodies
    /// written out by hand.
    pub(crate) fn client_core(
        endpoint_url: &str,
        capture_request_id: bool,
    ) -> Result<Arc<ClientCore>, Box<dyn Error + 'static>> {
        let http = HttpClient::new(&HttpClientSettings::default())?;
        let credentials = Arc::new(CachedCredentials::new(Arc::new(StaticProvider::new(
            credentials(),
        ))));

        Ok(Arc::new(ClientCore::new(
            http,
            credentials,
            Arc::from(REGION),
            Url::parse(endpoint_url)?,
            CLIENT_TIMEOUT,
            capture_request_id,
        )))
    }

    /// A fragment-association row, shaped as the immutable store writes one.
    pub(crate) fn fragments_item(hash: &[u8], repository_context: &[u8]) -> Item {
        let mut item = Item::new();
        item.insert(
            ByteStr::from_static(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE),
            AttributeValue::blob(hash),
        );
        item.insert(
            ByteStr::from_static(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE),
            AttributeValue::blob(repository_context),
        );
        item
    }

    fn throughput() -> Value {
        json!({
            "ReadCapacityUnits": FIXTURE_CAPACITY,
            "WriteCapacityUnits": FIXTURE_CAPACITY,
        })
    }

    /// One `DynamoDB` attribute definition. Every key this suite defines is binary except the two
    /// the lock table sorts and filters as text.
    fn attribute(name: &str, kind: &str) -> Value {
        json!({ "AttributeName": name, "AttributeType": kind })
    }

    fn key(name: &str, role: &str) -> Value {
        json!({ "AttributeName": name, "KeyType": role })
    }

    /// Send a `CreateTable` built by the caller.
    ///
    /// Losing the race to create a table is success: the tests run in parallel against one stack, and
    /// all the caller wanted was for the table to exist.
    async fn create_table(core: &ClientCore, definition: Value) -> Result<(), Box<dyn Error>> {
        let body = Bytes::from(serde_json::to_vec(&definition)?);
        let send = core.send::<DynamoProtocol>(|| {
            HttpRequest::new(Method::POST, core.url("/", None))
                .header(
                    "content-type",
                    HeaderValue::from_static("application/x-amz-json-1.0"),
                )
                .header(
                    "x-amz-target",
                    HeaderValue::from_static("DynamoDB_20120810.CreateTable"),
                )
                .body(body.clone())
        });

        match send.await {
            Ok(_) => Ok(()),
            Err(AwsCallError::Service(error)) if error.code_str() == "ResourceInUseException" => {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Delete the table `name`.
    pub(crate) async fn delete_table(core: &ClientCore, name: &str) -> Result<(), Box<dyn Error>> {
        let body = Bytes::from(serde_json::to_vec(&json!({ "TableName": name }))?);
        core.send::<DynamoProtocol>(|| {
            HttpRequest::new(Method::POST, core.url("/", None))
                .header(
                    "content-type",
                    HeaderValue::from_static("application/x-amz-json-1.0"),
                )
                .header(
                    "x-amz-target",
                    HeaderValue::from_static("DynamoDB_20120810.DeleteTable"),
                )
                .body(body.clone())
        })
        .await?;

        Ok(())
    }

    /// Create a bucket, path-style, tolerating one that is already there.
    async fn create_bucket(core: &ClientCore, bucket: &str) -> Result<(), Box<dyn Error>> {
        let send = core.send::<S3Protocol>(|| {
            HttpRequest::new(Method::PUT, core.url(&format!("/{bucket}"), None))
        });

        match send.await {
            Ok(_) => Ok(()),
            Err(AwsCallError::Service(error))
                if matches!(
                    error.code_str(),
                    "BucketAlreadyOwnedByYou" | "BucketAlreadyExists"
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn setup(
        tables: Vec<&str>,
    ) -> Result<(S3, DynamoDb, DynamoDb), Box<dyn Error + 'static>> {
        let _ = tracing_subscriber::fmt::try_init();

        Ok((
            s3_client(S3_ENDPOINT.to_string()).await?,
            dynamodb_client(DYNAMODB_ENDPOINT.to_string(), tables.clone()).await?,
            dynamodb_client(DYNAMODB_ENDPOINT.to_string(), tables).await?,
        ))
    }

    /// A client of `DynamoDB` Local alone, holding `tables`, with the tracing [`setup`] starts.
    pub async fn dynamodb_setup(tables: Vec<&str>) -> Result<DynamoDb, Box<dyn Error + 'static>> {
        let _ = tracing_subscriber::fmt::try_init();
        dynamodb_client(DYNAMODB_ENDPOINT.to_string(), tables).await
    }

    /// Make sure the store bucket exists, for a test that does not want the facade.
    pub(crate) async fn s3_bucket(endpoint_url: String) -> Result<(), Box<dyn Error + 'static>> {
        s3_client(endpoint_url).await.map(|_client| ())
    }

    /// Make sure [`VERSIONED_BUCKET_NAME`] exists with versioning enabled.
    pub(crate) async fn versioned_bucket() -> Result<(), Box<dyn Error + 'static>> {
        let core = client_core(S3_ENDPOINT, false)?;
        create_bucket(&core, VERSIONED_BUCKET_NAME).await?;

        let body = Bytes::from_static(
            b"<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
              <Status>Enabled</Status></VersioningConfiguration>",
        );
        core.send::<S3Protocol>(|| {
            HttpRequest::new(
                Method::PUT,
                core.url(&format!("/{VERSIONED_BUCKET_NAME}"), Some("versioning")),
            )
            .body(body.clone())
        })
        .await?;

        Ok(())
    }

    async fn s3_client(endpoint_url: String) -> Result<S3, Box<dyn Error + 'static>> {
        let http_settings = HttpClientSettings::default();

        let client = AwsClientBuilder::builder()
            .with_http_settings(&http_settings)
            .with_static_credentials(credentials())
            .region(REGION)
            .endpoint(endpoint_url.clone())
            .build_config()
            .s3()
            .build()
            .await?;

        match client.bucket_exists(STORE_BUCKET_NAME).await {
            Ok(exists) => {
                if !exists {
                    info!("Bucket {STORE_BUCKET_NAME} does not exist, creating...");
                    let core = client_core(&endpoint_url, false)?;
                    create_bucket(&core, STORE_BUCKET_NAME).await?;
                }

                Ok(client)
            }
            Err(e) => {
                warn!("Failed to check if bucket exists: {e:?}");
                Err(e.into())
            }
        }
    }

    async fn create_locks_table(core: &ClientCore) -> Result<(), Box<dyn Error>> {
        // Every index projects everything: the lock store reads whole rows off each of them.
        let index = |name: &str, partition: &str, sort: &str| {
            json!({
                "IndexName": name,
                "KeySchema": [key(partition, "HASH"), key(sort, "RANGE")],
                "Projection": { "ProjectionType": "ALL" },
                "ProvisionedThroughput": throughput(),
            })
        };

        create_table(
            core,
            json!({
                "TableName": LOCKS_TABLE_NAME,
                "AttributeDefinitions": [
                    attribute(HASH_KEY, "B"),
                    attribute(REPO_KEY, "B"),
                    attribute(BRANCH_KEY, "B"),
                    attribute(REPO_BRANCH_KEY, "B"),
                    attribute(OWNER_KEY, "S"),
                    attribute(DESC_KEY, "S"),
                ],
                "KeySchema": [key(HASH_KEY, "HASH"), key(REPO_BRANCH_KEY, "RANGE")],
                "GlobalSecondaryIndexes": [
                    index(OWNER_REPO_BRANCH_GSI, OWNER_KEY, REPO_BRANCH_KEY),
                    index(REPO_BRANCH_GSI, REPO_KEY, BRANCH_KEY),
                    index(REPO_BRANCH_DESC_GSI, REPO_BRANCH_KEY, DESC_KEY),
                ],
                "ProvisionedThroughput": throughput(),
            }),
        )
        .await
    }

    async fn create_store_table(core: &ClientCore) -> Result<(), Box<dyn Error>> {
        create_table(
            core,
            json!({
                "TableName": MUTABLE_STORE_TABLE_NAME,
                "AttributeDefinitions": [
                    attribute(MUTABLE_STORE_DYNAMO_PARTITION_KEY_ATTRIBUTE, "B"),
                    attribute(MUTABLE_STORE_DYNAMO_SORT_KEY_ATTRIBUTE, "B"),
                ],
                "KeySchema": [
                    key(MUTABLE_STORE_DYNAMO_PARTITION_KEY_ATTRIBUTE, "HASH"),
                    key(MUTABLE_STORE_DYNAMO_SORT_KEY_ATTRIBUTE, "RANGE"),
                ],
                "ProvisionedThroughput": throughput(),
            }),
        )
        .await
    }

    async fn create_fragments_table(core: &ClientCore) -> Result<(), Box<dyn Error>> {
        create_table(
            core,
            json!({
                "TableName": FRAGMENTS_TABLE_NAME,
                "AttributeDefinitions": [
                    attribute(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE, "B"),
                    attribute(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE, "B"),
                ],
                "KeySchema": [
                    key(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE, "HASH"),
                    key(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE, "RANGE"),
                ],
                "ProvisionedThroughput": throughput(),
            }),
        )
        .await
    }

    /// The state table and the legacy metadata table have the same shape: one binary hash.
    pub(crate) async fn create_hash_keyed_table(
        core: &ClientCore,
        name: &str,
    ) -> Result<(), Box<dyn Error>> {
        create_table(
            core,
            json!({
                "TableName": name,
                "AttributeDefinitions": [attribute("hash", "B")],
                "KeySchema": [key("hash", "HASH")],
                "ProvisionedThroughput": throughput(),
            }),
        )
        .await
    }

    pub(crate) async fn dynamodb_client(
        endpoint_url: String,
        tables: Vec<&str>,
    ) -> Result<DynamoDb, Box<dyn Error + 'static>> {
        dynamodb_client_with_unprocessed_keys_retry(endpoint_url, tables, None).await
    }

    /// Connects to `DynamoDB` at `endpoint_url`, creating any of `tables` that is missing. The
    /// client resends unprocessed `BatchGetItem` keys with `unprocessed_keys_retry`, or with the
    /// client default when it is `None`.
    pub(crate) async fn dynamodb_client_with_unprocessed_keys_retry(
        endpoint_url: String,
        tables: Vec<&str>,
        unprocessed_keys_retry: Option<RetryPolicy>,
    ) -> Result<DynamoDb, Box<dyn Error + 'static>> {
        let http_settings = HttpClientSettings::default();

        let client = AwsClientBuilder::builder()
            .with_http_settings(&http_settings)
            .with_static_credentials(credentials())
            .region(REGION)
            .endpoint(endpoint_url.clone())
            .build_config()
            .maybe_unprocessed_keys_retry(unprocessed_keys_retry)
            .dynamodb()
            .build()
            .await?;

        for table_name in tables {
            match client.table_exists(&Arc::from(table_name)).await {
                Ok(exists) => {
                    if !exists {
                        let core = client_core(&endpoint_url, false)?;
                        match table_name {
                            MUTABLE_STORE_TABLE_NAME => create_store_table(&core).await?,
                            FRAGMENTS_TABLE_NAME => create_fragments_table(&core).await?,
                            FRAGMENT_STATE_TABLE_NAME => {
                                create_hash_keyed_table(&core, FRAGMENT_STATE_TABLE_NAME).await?;
                            }
                            FRAGMENT_METADATA_TABLE_NAME => {
                                create_hash_keyed_table(&core, FRAGMENT_METADATA_TABLE_NAME)
                                    .await?;
                            }
                            LOCKS_TABLE_NAME => create_locks_table(&core).await?,
                            _ => {
                                return Err(
                                    anyhow::anyhow!("Invalid table name: {table_name}").into()
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to check if table exists: {e:?}");
                    return Err(e.into());
                }
            }
        }

        Ok(client)
    }
}
