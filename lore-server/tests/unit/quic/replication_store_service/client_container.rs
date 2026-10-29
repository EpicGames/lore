// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lore_revision::util::time::RetryPolicy;
use lore_server::protocol::replication_store::copy::ImmutableCopy;
use lore_server::protocol::replication_store::get::Get;
use lore_server::protocol::replication_store::get_metadata::GetMetadata;
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
