// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::lore_spawn;
use lore_base::types::Context;
use lore_revision::util::time::RetryPolicy;
use lore_server::protocol::replication_store::copy::ImmutableCopy;
use lore_server::protocol::replication_store::get::Get;
use lore_server::protocol::replication_store::get_metadata::GetMetadata;
use lore_server::protocol::replication_store::header::ReplicationHeader;
use lore_server::protocol::replication_store::obliterate::Obliterate;
use lore_server::protocol::replication_store::obliterate::ObliterateResponse;
use lore_server::protocol::replication_store::put::Put;
use lore_server::protocol::replication_store::query::Query;
use lore_server::protocol::replication_store::query::QueryResponse;
use lore_server::quic::replication_store_service::client::ReplicationStoreClientError;
use lore_server::quic::replication_store_service::client::StoreClient;
use lore_server::quic::replication_store_service::client_container::*;
use lore_storage::StoreGetData;
use lore_transport::ProtocolError;
use lore_transport::quic::client::ConnectionStats;
use lore_transport::quic::client::FrameStats;
use lore_transport::quic::client::PathStats;
use lore_transport::quic::client::UdpStats;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;

mockall::mock! {
    pub Client {}

    #[async_trait]
    impl StoreClient for Client {
        async fn connection_stats(&self) -> Option<ConnectionStats>;
        async fn put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;
        async fn obliterate(&self, request: Obliterate) -> Result<ObliterateResponse, ReplicationStoreClientError>;
        async fn get(&self, request: Get) -> Result<StoreGetData, ReplicationStoreClientError>;
        async fn get_metadata(&self, request: GetMetadata) -> Result<StoreGetData, ReplicationStoreClientError>;
        async fn local_put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;
        async fn local_get(&self, request: Get) -> Result<StoreGetData, ReplicationStoreClientError>;
        async fn local_get_metadata(&self, request: GetMetadata) -> Result<StoreGetData, ReplicationStoreClientError>;
        async fn query(&self, request: Query) -> Result<QueryResponse, ReplicationStoreClientError>;
        async fn local_query(&self, request: Query) -> Result<QueryResponse, ReplicationStoreClientError>;
        async fn copy(&self, request: ImmutableCopy) -> Result<(), ReplicationStoreClientError>;
    }
}

struct CapturingFactory {
    rx: tokio::sync::Mutex<Receiver<Result<MockClient, ProtocolError>>>,
    captured_cwnd: Arc<Mutex<Vec<Option<u64>>>>,
}

#[async_trait]
impl ClientFactory for CapturingFactory {
    type Output = MockClient;

    async fn make_client(&self, initial_cwnd: Option<u64>) -> Result<MockClient, ProtocolError> {
        self.captured_cwnd.lock().unwrap().push(initial_cwnd);
        self.rx.lock().await.recv().await.expect("recv should work")
    }
}

fn make_connection_stats(cwnd: u64) -> ConnectionStats {
    ConnectionStats {
        udp_tx: UdpStats {
            datagrams: 0,
            bytes: 0,
            ios: 0,
        },
        udp_rx: UdpStats {
            datagrams: 0,
            bytes: 0,
            ios: 0,
        },
        frame_tx: FrameStats {
            data_blocked: 0,
            stream_data_blocked: 0,
            streams_blocked_bidi: 0,
            streams_blocked_uni: 0,
            max_data: 0,
            max_stream_data: 0,
            max_streams_bidi: 0,
            stream: 0,
            reset_stream: 0,
        },
        frame_rx: FrameStats {
            data_blocked: 0,
            stream_data_blocked: 0,
            streams_blocked_bidi: 0,
            streams_blocked_uni: 0,
            max_data: 0,
            max_stream_data: 0,
            max_streams_bidi: 0,
            stream: 0,
            reset_stream: 0,
        },
        path: PathStats {
            rtt: Duration::ZERO,
            cwnd,
            congestion_events: 0,
            lost_packets: 0,
            lost_bytes: 0,
            sent_packets: 0,
            black_holes_detected: 0,
            current_mtu: 0,
        },
    }
}

fn make_config() -> ClientContainerConfig {
    ClientContainerConfig {
        regenerate_retry_policy: RetryPolicy::builder()
            .with_initial_backoff(Duration::ZERO)
            .with_max_backoff(Duration::ZERO)
            .with_limit(0)
            .build(),
        connection_lost_sleep: Duration::ZERO,
    }
}

mod integration {
    use std::net::SocketAddr;
    use std::net::UdpSocket;
    use std::sync::Arc;

    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_server::quic::quinn::QuinnConfigBuilder;
    use lore_server::quic::quinn::QuinnServer;
    use lore_server::quic::tests::TestHandlerFactory;
    use lore_server::quic::tests::server_certs;
    use lore_storage::ImmutableStore;
    use lore_storage::MutableStore;
    use lore_transport::quic::client::CertificateSettings;

    use super::*;
    use crate::store::test_support::test_store_create;

    /// The default initial window is ~12 KB (10 × 1,200-byte datagrams).
    /// 64 MB is large enough that it cannot be reached by slow-start
    /// growth alone over the handful of packets exchanged during connection.
    const LARGE_CWND: u64 = 64 * 1024 * 1024;

    fn start_test_server(
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
    ) -> (SocketAddr, QuinnServer) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().unwrap();
        drop(socket);

        let (cert_path, key_path, _ca) = server_certs().expect("Bad server cert paths");
        let server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    immutable_store,
                    mutable_store,
                )))
                .build()
                .unwrap(),
        )
        .expect("Failed to start test QUIC server");

        (server_addr, server)
    }

    fn no_tls_factory(server_addr: SocketAddr) -> QuicClientFactory {
        // quic:// (not quics://) skips certificate verification on the client side
        QuicClientFactory::new(
            format!("quic://{server_addr}"),
            CertificateSettings {
                custom_ca: None,
                client: None,
            },
        )
    }

    /// When `make_client` is called with `Some(cwnd)`, the resulting connection's
    /// reported congestion window must be at least that value.
    ///
    /// BBR (and Cubic) initialise the congestion window at `initial_window` and
    /// only grow from there, so `path.cwnd >= initial_cwnd` is an invariant that
    /// holds immediately after connection establishment.
    #[tokio::test]
    async fn large_initial_cwnd_is_reflected_in_connection_path_stats() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server) = start_test_server(immutable_store, mutable_store);

            let client = no_tls_factory(server_addr)
                .make_client(Some(LARGE_CWND))
                .await
                .expect("Failed to connect");

            let stats = client
                .connection_stats()
                .await
                .expect("Should always have connection stats");

            assert!(
                stats.path.cwnd >= LARGE_CWND,
                "cwnd ({}) should be >= configured initial_cwnd ({})",
                stats.path.cwnd,
                LARGE_CWND,
            );
        }))
        .await
        .expect("Test task failed");
    }

    /// Without an `initial_cwnd` hint the window stays below `LARGE_CWND`, so a
    /// window at or above it can only have come from the hint, not from slow-start
    /// growth over loopback.
    #[tokio::test]
    async fn default_initial_cwnd_is_smaller_than_large_initial_cwnd() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server) = start_test_server(immutable_store, mutable_store);

            let client = no_tls_factory(server_addr)
                .make_client(None)
                .await
                .expect("Failed to connect");

            let stats = client
                .connection_stats()
                .await
                .expect("Should always have connection stats");

            assert!(
                stats.path.cwnd < LARGE_CWND,
                "default cwnd ({}) should be < large initial_cwnd ({}); \
                         if flaky, slow start grew too fast — increase LARGE_CWND",
                stats.path.cwnd,
                LARGE_CWND,
            );
        }))
        .await
        .expect("Test task failed");
    }
}

struct GatedClient {
    id: u32,
    /// Signalled when a query reaches the client.
    started: Mutex<Option<oneshot::Sender<()>>>,
    /// A query waits for this before it returns.
    release: Mutex<Option<oneshot::Receiver<()>>>,
    served: Arc<Mutex<Vec<u32>>>,
}

#[async_trait]
impl StoreClient for GatedClient {
    async fn connection_stats(&self) -> Option<ConnectionStats> {
        None
    }

    async fn query(&self, _request: Query) -> Result<QueryResponse, ReplicationStoreClientError> {
        self.served.lock().unwrap().push(self.id);
        let started = self.started.lock().unwrap().take();
        if let Some(started) = started {
            let _ = started.send(());
        }
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            let _ = release.await;
        }
        Ok(QueryResponse {
            results: Vec::new(),
        })
    }

    async fn put(&self, _request: Put) -> Result<(), ReplicationStoreClientError> {
        unreachable!()
    }

    async fn obliterate(
        &self,
        _request: Obliterate,
    ) -> Result<ObliterateResponse, ReplicationStoreClientError> {
        unreachable!()
    }

    async fn get(&self, _request: Get) -> Result<StoreGetData, ReplicationStoreClientError> {
        unreachable!()
    }

    async fn get_metadata(
        &self,
        _request: GetMetadata,
    ) -> Result<StoreGetData, ReplicationStoreClientError> {
        unreachable!()
    }

    async fn local_put(&self, _request: Put) -> Result<(), ReplicationStoreClientError> {
        unreachable!()
    }

    async fn local_get(&self, _request: Get) -> Result<StoreGetData, ReplicationStoreClientError> {
        unreachable!()
    }

    async fn local_get_metadata(
        &self,
        _request: GetMetadata,
    ) -> Result<StoreGetData, ReplicationStoreClientError> {
        unreachable!()
    }

    async fn local_query(
        &self,
        _request: Query,
    ) -> Result<QueryResponse, ReplicationStoreClientError> {
        unreachable!()
    }

    async fn copy(&self, _request: ImmutableCopy) -> Result<(), ReplicationStoreClientError> {
        unreachable!()
    }
}

struct QueueFactory {
    clients: Mutex<VecDeque<GatedClient>>,
}

#[async_trait]
impl ClientFactory for QueueFactory {
    type Output = GatedClient;

    async fn make_client(&self, _initial_cwnd: Option<u64>) -> Result<GatedClient, ProtocolError> {
        Ok(self
            .clients
            .lock()
            .unwrap()
            .pop_front()
            .expect("the test should queue a client for every make_client call"))
    }
}

fn make_query() -> Query {
    Query {
        header: ReplicationHeader {
            correlation_id: uuid::Uuid::nil(),
            repository: Context::default(),
        },
        addresses: Vec::new(),
    }
}

mod regenerate_client {
    use super::*;

    #[tokio::test]
    async fn periodic_refresh_sets_initial_cwnd_from_connection_stats() {
        const EXPECTED_CWND: u64 = 12345;

        let (tx, rx) = mpsc::channel(2);
        let captured_cwnd = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(CapturingFactory {
            rx: rx.into(),
            captured_cwnd: captured_cwnd.clone(),
        });

        // Initial client reports the current cwnd when connection_stats is queried
        let mut initial_client = MockClient::new();
        initial_client
            .expect_connection_stats()
            .return_once(|| Some(make_connection_stats(EXPECTED_CWND)));
        tx.send(Ok(initial_client))
            .await
            .expect("should create initial client");

        // Regenerated client
        tx.send(Ok(MockClient::new()))
            .await
            .expect("should create regenerated client");

        let container = ClientContainer::new(factory, make_config())
            .await
            .expect("factory create");
        let epoch = container.epoch();

        let did_regen = container
            .regenerate_client(epoch, GenerateClientReason::PeriodicRefresh)
            .await
            .expect("regenerate should work");
        assert!(did_regen);

        let calls = captured_cwnd.lock().unwrap();
        // calls[0] is the initial ClientContainer::new() call, calls[1] is the regeneration
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], None, "initial client should not have cwnd set");
        assert_eq!(
            calls[1],
            Some(EXPECTED_CWND),
            "PeriodicRefresh should forward the current cwnd to the new client"
        );
    }

    #[tokio::test]
    async fn connection_failed_passes_none_for_initial_cwnd() {
        let (tx, rx) = mpsc::channel(2);
        let captured_cwnd = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(CapturingFactory {
            rx: rx.into(),
            captured_cwnd: captured_cwnd.clone(),
        });

        // Initial client (connection_stats not called in ConnectionFailed path)
        tx.send(Ok(MockClient::new()))
            .await
            .expect("should create initial client");

        // Regenerated client
        tx.send(Ok(MockClient::new()))
            .await
            .expect("should create regenerated client");

        let container = ClientContainer::new(factory, make_config())
            .await
            .expect("factory create");
        let epoch = container.epoch();

        let did_regen = container
            .regenerate_client(epoch, GenerateClientReason::ConnectionFailed)
            .await
            .expect("regenerate should work");
        assert!(did_regen);

        let calls = captured_cwnd.lock().unwrap();
        // calls[0] is the initial ClientContainer::new() call, calls[1] is the regeneration
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], None, "initial client should not have cwnd set");
        assert_eq!(
            calls[1], None,
            "ConnectionFailed should not forward the failed connection's cwnd"
        );
    }

    /// A regeneration swaps the new client in without waiting for requests on the old one. Those
    /// requests finish on the old client, which stays open until the last of them ends, while
    /// requests issued after the swap go to the new client.
    #[tokio::test]
    async fn regeneration_does_not_wait_for_requests_on_the_replaced_client() {
        let served = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();

        let old_client = GatedClient {
            id: 0,
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(Some(release_rx)),
            served: served.clone(),
        };
        let new_client = ungated_client(1, &served);
        let factory = Arc::new(QueueFactory {
            clients: Mutex::new(VecDeque::from([old_client, new_client])),
        });

        let container = ClientContainer::new(factory, make_config())
            .await
            .expect("factory create");

        let (_, client) = container.request_client(None);
        let replaced = Arc::downgrade(&client);
        let in_flight = lore_spawn!(async move { client.query(make_query()).await });
        started_rx
            .await
            .expect("the request should reach the old client");

        let did_regen = tokio::time::timeout(
            Duration::from_secs(5),
            container.regenerate_client(container.epoch(), GenerateClientReason::PeriodicRefresh),
        )
        .await
        .expect("regeneration should not wait for the in-flight request")
        .expect("regenerate should work");
        assert!(did_regen);

        let (_, client) = container.request_client(None);
        let response = client
            .query(make_query())
            .await
            .expect("a query after the swap should succeed");
        assert!(response.results.is_empty());
        assert_eq!(*served.lock().unwrap(), vec![0, 1]);
        assert!(
            replaced.upgrade().is_some(),
            "the old client should stay open while a request on it is in flight"
        );

        release_tx
            .send(())
            .expect("the in-flight request should still be waiting");
        let response = in_flight
            .await
            .expect("the in-flight task should not panic")
            .expect("the in-flight request should complete on the old client");
        assert!(response.results.is_empty());
        assert!(
            replaced.upgrade().is_none(),
            "the old client should be dropped once its last request ends"
        );
    }

    fn ungated_client(id: u32, served: &Arc<Mutex<Vec<u32>>>) -> GatedClient {
        GatedClient {
            id,
            started: Mutex::new(None),
            release: Mutex::new(None),
            served: served.clone(),
        }
    }

    /// A request receives the current client and the metadata used to report its failures. A
    /// failure reported with the current client's epoch triggers replacement of that client. A
    /// failure reported with an earlier client's epoch leaves the current client in place and the
    /// container healthy.
    #[tokio::test]
    async fn a_failure_replaces_only_the_client_whose_epoch_it_reports() {
        let served = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(QueueFactory {
            clients: Mutex::new(VecDeque::from([
                ungated_client(0, &served),
                ungated_client(1, &served),
                ungated_client(2, &served),
            ])),
        });
        let container = ClientContainer::new(factory, make_config())
            .await
            .expect("factory create");
        let (_, address, _) = lore_revision::fragment::generate_random();

        let (first_meta, first) = container.request_client(Some(address));
        assert_eq!(first_meta.client_epoch, 0);
        assert_eq!(first_meta.address, Some(address));
        first
            .query(make_query())
            .await
            .expect("a query on the first client should succeed");

        let did_regen = container
            .regenerate_client(
                first_meta.client_epoch,
                GenerateClientReason::ConnectionFailed,
            )
            .await
            .expect("regenerate should work");
        assert!(
            did_regen,
            "a failure reported with the first client's epoch should replace it"
        );
        assert!(container.is_healthy());

        let (second_meta, second) = container.request_client(None);
        assert_eq!(second_meta.client_epoch, 1);
        assert_eq!(second_meta.address, None);
        second
            .query(make_query())
            .await
            .expect("a query on the second client should succeed");

        let did_regen = container
            .regenerate_client(
                first_meta.client_epoch,
                GenerateClientReason::ConnectionFailed,
            )
            .await
            .expect("regenerate should work");
        assert!(
            !did_regen,
            "a failure reported with a replaced client's epoch should not replace its successor"
        );
        assert!(container.is_healthy());
        let (still_second_meta, still_second) = container.request_client(None);
        assert_eq!(still_second_meta.client_epoch, 1);
        still_second
            .query(make_query())
            .await
            .expect("a query on the second client should still succeed");

        let did_regen = container
            .regenerate_client(
                second_meta.client_epoch,
                GenerateClientReason::ConnectionFailed,
            )
            .await
            .expect("regenerate should work");
        assert!(
            did_regen,
            "a failure reported with the second client's epoch should replace it"
        );
        assert!(container.is_healthy());

        let (third_meta, third) = container.request_client(None);
        assert_eq!(third_meta.client_epoch, 2);
        third
            .query(make_query())
            .await
            .expect("a query on the third client should succeed");
        assert_eq!(*served.lock().unwrap(), vec![0, 1, 1, 2]);
    }
}

mod report_connection_failed {
    use super::*;

    async fn make_container() -> (
        ClientContainer<MockClient>,
        mpsc::Sender<Result<MockClient, ProtocolError>>,
    ) {
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(MockClient::new())).await.expect("send client");
        let factory = Arc::new(CapturingFactory {
            rx: rx.into(),
            captured_cwnd: Arc::default(),
        });
        let container = ClientContainer::new(factory, make_config())
            .await
            .expect("factory create");
        (container, tx)
    }

    #[tokio::test]
    async fn a_failure_on_the_current_client_marks_the_container_unhealthy() {
        let (container, _tx) = make_container().await;

        assert!(container.report_connection_failed(container.epoch()));
        assert!(!container.is_healthy());
    }

    #[tokio::test]
    async fn only_the_first_failure_reports_the_change() {
        let (container, _tx) = make_container().await;

        assert!(container.report_connection_failed(container.epoch()));
        assert!(!container.report_connection_failed(container.epoch()));
        assert!(!container.is_healthy());
    }

    #[tokio::test]
    async fn a_failure_on_a_replaced_client_changes_nothing() {
        let (container, tx) = make_container().await;
        tx.send(Ok(MockClient::new())).await.expect("send client");
        let replaced_epoch = container.epoch();
        let did_regen = container
            .regenerate_client(replaced_epoch, GenerateClientReason::ConnectionFailed)
            .await
            .expect("regenerate should work");
        assert!(did_regen);

        assert!(!container.report_connection_failed(replaced_epoch));
        assert!(container.is_healthy());
    }
}

mod lease {
    use super::*;

    async fn make_container(
        clients: Vec<MockClient>,
    ) -> (
        ClientContainer<MockClient>,
        mpsc::Sender<Result<MockClient, ProtocolError>>,
    ) {
        let (tx, rx) = mpsc::channel(clients.len());
        for client in clients {
            tx.send(Ok(client)).await.expect("send client");
        }
        let factory = Arc::new(CapturingFactory {
            rx: rx.into(),
            captured_cwnd: Arc::default(),
        });
        let container = ClientContainer::new(factory, make_config())
            .await
            .expect("factory create");
        (container, tx)
    }

    #[tokio::test]
    async fn a_request_is_counted_under_its_kind_until_its_guard_drops() {
        let (container, _tx) = make_container(vec![MockClient::new()]).await;

        let (_, _, first) = container.lease(RequestKind::Get, None);
        let (_, _, second) = container.lease(RequestKind::Get, None);
        let (_, _, put) = container.lease(RequestKind::Put, None);
        assert_eq!(container.inflight(RequestKind::Get), 2);
        assert_eq!(container.inflight(RequestKind::Put), 1);
        assert_eq!(container.inflight(RequestKind::Query), 0);
        assert_eq!(container.inflight_total(), 3);
        drop(put);

        drop(first);
        assert_eq!(container.inflight(RequestKind::Get), 1);
        drop(second);
        assert_eq!(container.inflight(RequestKind::Get), 0);
    }

    #[tokio::test]
    async fn a_lease_reports_the_epoch_of_the_client_it_returns() {
        let (container, tx) = make_container(vec![MockClient::new()]).await;
        tx.send(Ok(MockClient::new())).await.expect("send client");

        let (before, _, _) = container.lease(RequestKind::Get, None);
        let did_regen = container
            .regenerate_client(before.client_epoch, GenerateClientReason::ConnectionFailed)
            .await
            .expect("regenerate should work");
        assert!(did_regen);
        let (after, _, _) = container.lease(RequestKind::Get, None);

        assert_eq!(before.client_epoch, 0);
        assert_eq!(after.client_epoch, 1);
    }

    /// Requests still running on a replaced client are not counted against its successor, and
    /// finishing them does not change the successor's count.
    #[tokio::test]
    async fn a_new_client_starts_with_nothing_in_flight() {
        let (container, tx) = make_container(vec![MockClient::new()]).await;
        tx.send(Ok(MockClient::new())).await.expect("send client");

        let (_, _, on_old_client) = container.lease(RequestKind::Put, None);
        let did_regen = container
            .regenerate_client(container.epoch(), GenerateClientReason::ConnectionFailed)
            .await
            .expect("regenerate should work");
        assert!(did_regen);
        assert_eq!(container.inflight(RequestKind::Put), 0);

        let (_, _, on_new_client) = container.lease(RequestKind::Put, None);
        drop(on_old_client);
        assert_eq!(container.inflight(RequestKind::Put), 1);
        drop(on_new_client);
        assert_eq!(container.inflight(RequestKind::Put), 0);
    }
}
