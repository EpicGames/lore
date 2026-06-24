// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use lore_revision::util::time::RetryPolicy;
use lore_telemetry::LabelArray;
use lore_telemetry::observe::observe_result;
use lore_transport::ProtocolError;
use lore_transport::quic::client::CertificateSettings;
use lore_transport::quic::client::CongestionAlgorithm;
use lore_transport::quic::client::ConnectionStats;
use lore_transport::quic::client::DEFAULT_EXPECTED_RTT_MS;
use lore_transport::quic::client::TransportConfig;
use opentelemetry::KeyValue;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tracing::error;

use crate::quic::replication_store_service::DEFAULT_CLIENT_MESSAGE_LIMIT;
use crate::quic::replication_store_service::client::CommandBehavior;
use crate::quic::replication_store_service::client::DEFAULT_MAX_BYTES_BANDWIDTH_PER_SEC;
use crate::quic::replication_store_service::client::ReplicationStoreClient;
use crate::quic::replication_store_service::client::StoreClient;

pub enum GenerateClientReason {
    PeriodicRefresh,
    ConnectionFailed,
}

#[async_trait]
pub trait ClientFactory: Send + Sync + 'static {
    type Output: StoreClient;
    async fn make_client(&self, initial_cwnd: Option<u64>) -> Result<Self::Output, ProtocolError>;
}

pub struct QuicClientFactory {
    remote_url: String,
    certs: CertificateSettings,
    pub command_behavior: CommandBehavior,
    pub transport_config: TransportConfig,
    pub quic_max_reconnects: Option<u32>,
    pub sni_override: Option<String>,
}

impl QuicClientFactory {
    pub fn new(remote_url: String, certs: CertificateSettings) -> Self {
        Self {
            remote_url,
            certs,
            transport_config: TransportConfig {
                max_bytes_bandwidth_per_second: DEFAULT_MAX_BYTES_BANDWIDTH_PER_SEC,
                expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                congestion_algorithm: CongestionAlgorithm::Bbr,
                initial_cwnd: None,
            },
            command_behavior: CommandBehavior {
                message_limit: DEFAULT_CLIENT_MESSAGE_LIMIT,
                should_await_command_permit: true,
            },
            quic_max_reconnects: None,
            sni_override: None,
        }
    }
}

#[async_trait]
impl ClientFactory for QuicClientFactory {
    type Output = ReplicationStoreClient;

    async fn make_client(&self, initial_cwnd: Option<u64>) -> Result<Self::Output, ProtocolError> {
        let mut transport_config = self.transport_config.clone();
        if let Some(initial_cwnd) = initial_cwnd {
            transport_config.initial_cwnd = Some(initial_cwnd);
        }

        let client = ReplicationStoreClient::connect(
            &self.remote_url,
            self.certs.clone(),
            self.sni_override.clone(),
            transport_config,
            self.command_behavior.clone(),
            self.quic_max_reconnects,
        )
        .await?;
        Ok(client)
    }
}

pub struct ClientContainer<ClientType: StoreClient> {
    client_factory: Arc<dyn ClientFactory<Output = ClientType>>,
    generate_client_semaphore: Semaphore,
    regenerate_retry_policy: RetryPolicy,

    client: RwLock<ClientType>,
    client_epoch: AtomicU64,
    is_client_healthy: AtomicBool,

    connection_lost_sleep: Duration,
}

pub struct ClientContainerConfig {
    pub regenerate_retry_policy: RetryPolicy,
    pub connection_lost_sleep: Duration,
}

impl<ClientType> ClientContainer<ClientType>
where
    ClientType: StoreClient,
{
    pub async fn new(
        client_factory: Arc<dyn ClientFactory<Output = ClientType>>,
        config: ClientContainerConfig,
    ) -> Result<Self, ProtocolError> {
        let client = client_factory.make_client(None).await?;
        let container = ClientContainer {
            client_factory,
            generate_client_semaphore: Semaphore::new(1),
            client: client.into(),
            client_epoch: 0.into(),
            is_client_healthy: true.into(),
            regenerate_retry_policy: config.regenerate_retry_policy,
            connection_lost_sleep: config.connection_lost_sleep,
        };
        Ok(container)
    }

    pub fn epoch(&self) -> u64 {
        self.client_epoch.load(Ordering::Relaxed)
    }

    pub fn is_healthy(&self) -> bool {
        self.is_client_healthy.load(Ordering::Relaxed)
    }

    pub fn client(&self) -> &RwLock<ClientType> {
        &self.client
    }

    pub async fn connection_stats(&self) -> Option<ConnectionStats> {
        self.client.read().await.connection_stats().await
    }

    /// Caution: the concrete QUIC client requires an execution context
    pub async fn regenerate_client(
        &self,
        expected_epoch: u64,
        reason: GenerateClientReason,
    ) -> Result<bool, ProtocolError> {
        // multiple tasks might enter here at the same time to reconnect, but only 1 should
        // be responsible for doing the reconnect
        let Ok(_permit) = self.generate_client_semaphore.try_acquire() else {
            return Ok(false);
        };

        // depending on task scheduling, someone might have already reconnected a client
        // by the time our task got scheduled to do its reconnect, so guard against that
        if self.client_epoch.load(Ordering::Relaxed) != expected_epoch {
            return Ok(false);
        }

        let mut initial_cwnd = None;
        async move {
            match reason {
                GenerateClientReason::PeriodicRefresh => {
                    let stats = self.connection_stats().await;
                    if let Some(stats) = stats {
                        initial_cwnd = Some(stats.path.cwnd);
                    }
                }
                GenerateClientReason::ConnectionFailed => {
                    self.is_client_healthy.store(false, Ordering::Relaxed);
                    // the QUIC client itself already has some reconnect logic, so if it eventually
                    // gave up, and we ended up trying to make a new client, give it some time
                    // as it could be a server restart is occurring or the server might be in
                    // trouble, and we don't want to hammer it
                    tokio::time::sleep(self.connection_lost_sleep).await;
                }
            }

            // without a working QUIC client the store won't work,
            // so aggressively retry
            let mut retry = self.regenerate_retry_policy.retry();
            let new_client = loop {
                let make_result = self
                    .client_factory
                    .make_client(initial_cwnd)
                    .await
                    .inspect_err(|error| {
                        error!(?error, "Failed to regenerate client");
                    });

                if let Ok(client) = make_result {
                    break client;
                }

                if !retry.wait().await {
                    let _ = make_result?;
                }
            };

            let mut client_write = self.client.write().await;
            // The concrete QUIC client has some drop logic that blocks the current task on an
            // async function - draining connections and other slow operations.
            // We don't want this delay to hold back releasing the write lock,
            // so avoid dropping the old client until after we have dropped the write lock
            let _old_client = std::mem::replace(&mut *client_write, new_client);
            drop(client_write);

            self.client_epoch.fetch_add(1, Ordering::Relaxed);
            self.is_client_healthy.store(true, Ordering::Relaxed);

            Ok(true)
        }
        .await
    }
}

pub fn observe_regenerate()
-> impl Fn(&Result<bool, ProtocolError>, &Duration, &mut LabelArray) + Copy {
    move |result: &Result<bool, ProtocolError>, elapsed: &Duration, labels: &mut LabelArray| {
        // base observability
        observe_result(result, elapsed, labels);

        if let Ok(did_regenerate) = result {
            let label_value = if *did_regenerate {
                "regenerated"
            } else {
                "skipped"
            };

            labels.push(KeyValue::new("regeneration", label_value));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use lore_revision::util::time::RetryPolicy;
    use lore_transport::ProtocolError;
    use lore_transport::quic::client::ConnectionStats;
    use lore_transport::quic::client::FrameStats;
    use lore_transport::quic::client::PathStats;
    use lore_transport::quic::client::UdpStats;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::Receiver;

    use super::*;
    use crate::protocol::replication_store::exists_batch::ExistsBatch;
    use crate::protocol::replication_store::exists_batch::ExistsBatchResponse;
    use crate::protocol::replication_store::get::Get;
    use crate::protocol::replication_store::get::GetResponse;
    use crate::protocol::replication_store::obliterate::Obliterate;
    use crate::protocol::replication_store::obliterate::ObliterateResponse;
    use crate::protocol::replication_store::put::Put;
    use crate::protocol::replication_store::query::Query;
    use crate::protocol::replication_store::query::QueryResponse;
    use crate::quic::replication_store_service::client::ReplicationStoreClientError;
    use crate::quic::replication_store_service::client::StoreClient;

    mockall::mock! {
        pub Client {}

        #[async_trait]
        impl StoreClient for Client {
            async fn connection_stats(&self) -> Option<ConnectionStats>;
            async fn put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;
            async fn exists_batch(&self, request: ExistsBatch) -> Result<ExistsBatchResponse, ReplicationStoreClientError>;
            async fn obliterate(&self, request: Obliterate) -> Result<ObliterateResponse, ReplicationStoreClientError>;
            async fn get(&self, request: Get) -> Result<GetResponse, ReplicationStoreClientError>;
            async fn query(&self, request: Query) -> Result<QueryResponse, ReplicationStoreClientError>;
            async fn local_put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;
            async fn local_exists_batch(&self, request: ExistsBatch) -> Result<ExistsBatchResponse, ReplicationStoreClientError>;
            async fn local_get(&self, request: Get) -> Result<GetResponse, ReplicationStoreClientError>;
            async fn local_query(&self, request: Query) -> Result<QueryResponse, ReplicationStoreClientError>;
        }
    }

    struct CapturingFactory {
        rx: tokio::sync::Mutex<Receiver<Result<MockClient, ProtocolError>>>,
        captured_cwnd: Arc<Mutex<Vec<Option<u64>>>>,
    }

    #[async_trait]
    impl ClientFactory for CapturingFactory {
        type Output = MockClient;

        async fn make_client(
            &self,
            initial_cwnd: Option<u64>,
        ) -> Result<MockClient, ProtocolError> {
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

        use lore_base::runtime::LORE_CONTEXT;
        use lore_base::runtime::runtime;
        use lore_storage::ImmutableStore;
        use lore_storage::MutableStore;
        use lore_transport::quic::client::CertificateSettings;

        use super::super::*;
        use crate::quic::quinn::QuinnConfigBuilder;
        use crate::quic::quinn::QuinnServer;
        use crate::quic::tests::TestHandlerFactory;
        use crate::quic::tests::server_certs;
        use crate::store::test_store_create;

        /// BBR's default initial window is ~14 KB (10 × MTU).
        /// 64 MB is large enough that it cannot be reached by BBR startup
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

            runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
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

        /// Connects without an `initial_cwnd` hint and verifies the resulting window
        /// is smaller than `LARGE_CWND`, confirming that the previous test's large
        /// value comes from the hint and not from BBR startup growth over loopback.
        #[tokio::test]
        async fn default_initial_cwnd_is_smaller_than_large_initial_cwnd() {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create store");

            runtime()
                .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
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
                         if flaky, BBR grew too fast — increase LARGE_CWND",
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
}
