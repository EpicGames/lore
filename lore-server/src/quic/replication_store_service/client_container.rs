// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::types::Address;
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
use parking_lot::RwLock;
use parking_lot::RwLockReadGuard;
use tokio::sync::Semaphore;
use tracing::error;

use crate::quic::replication_store_service::DEFAULT_CLIENT_MESSAGE_LIMIT;
use crate::quic::replication_store_service::client::CommandBehavior;
use crate::quic::replication_store_service::client::DEFAULT_MAX_BYTES_BANDWIDTH_PER_SEC;
use crate::quic::replication_store_service::client::ReplicationStoreClient;
use crate::quic::replication_store_service::client::ServiceRequestMeta;
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
    pub user_agent: Option<String>,
}

impl QuicClientFactory {
    pub fn new(remote_url: String, certs: CertificateSettings) -> Self {
        Self {
            remote_url,
            certs,
            transport_config: TransportConfig {
                max_bytes_bandwidth_per_second: DEFAULT_MAX_BYTES_BANDWIDTH_PER_SEC,
                expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
                // Cubic only shrinks the window on packet loss, so a peer that goes quiet
                // between replication bursts doesn't come back to a window BBR has drained
                // and stall the next large get or put.
                congestion_algorithm: CongestionAlgorithm::Cubic,
                initial_cwnd: None,
            },
            command_behavior: CommandBehavior {
                message_limit: DEFAULT_CLIENT_MESSAGE_LIMIT,
                should_await_command_permit: true,
            },
            quic_max_reconnects: None,
            sni_override: None,
            user_agent: None,
        }
    }
}

#[async_trait]
impl ClientFactory for QuicClientFactory {
    type Output = ReplicationStoreClient;

    async fn make_client(&self, initial_cwnd: Option<u64>) -> Result<Self::Output, ProtocolError> {
        let mut transport_config = self.transport_config.clone();
        // The configured window is a floor for the hint: a connection that sat idle can report a
        // window far below it, and seeding from that would start the new connection slower than
        // a fresh one.
        transport_config.initial_cwnd = initial_cwnd.max(self.transport_config.initial_cwnd);

        let client = ReplicationStoreClient::connect(
            &self.remote_url,
            self.certs.clone(),
            self.sni_override.clone(),
            transport_config,
            self.command_behavior.clone(),
            self.quic_max_reconnects,
            self.user_agent.clone(),
        )
        .await?;
        Ok(client)
    }
}

/// A kind of request sent to the remote store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Query,
    Get,
    GetMetadata,
    Put,
    Copy,
    Obliterate,
}

impl RequestKind {
    pub const ALL: [RequestKind; 6] = [
        RequestKind::Query,
        RequestKind::Get,
        RequestKind::GetMetadata,
        RequestKind::Put,
        RequestKind::Copy,
        RequestKind::Obliterate,
    ];

    /// The operation context this kind is reported under in metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            RequestKind::Query => "query",
            RequestKind::Get => "get",
            RequestKind::GetMetadata => "get_metadata",
            RequestKind::Put => "put",
            RequestKind::Copy => "copy",
            RequestKind::Obliterate => "obliterate",
        }
    }
}

#[derive(Default)]
struct InflightRequests([AtomicUsize; RequestKind::ALL.len()]);

impl InflightRequests {
    fn count(&self, kind: RequestKind) -> &AtomicUsize {
        &self.0[kind as usize]
    }
}

/// Counts one request as in flight on the client it was leased from, until dropped.
pub struct InflightGuard {
    requests: Arc<InflightRequests>,
    kind: RequestKind,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.requests
            .count(self.kind)
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct CurrentClient<ClientType: StoreClient> {
    /// The number of regenerations up to and including the one that swapped this client in. A
    /// failure on `client` is reported with this epoch.
    epoch: u64,
    client: Arc<ClientType>,
    /// Requests leased on `client`. A new client starts with none, whatever its predecessor
    /// still has in flight.
    inflight: Arc<InflightRequests>,
}

pub struct ClientContainer<ClientType: StoreClient> {
    client_factory: Arc<dyn ClientFactory<Output = ClientType>>,
    generate_client_semaphore: Semaphore,
    regenerate_retry_policy: RetryPolicy,

    current: RwLock<CurrentClient<ClientType>>,
    is_client_healthy: AtomicBool,

    connection_lost_sleep: Duration,
}

#[derive(Clone)]
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
            current: RwLock::new(CurrentClient {
                epoch: 0,
                client: Arc::new(client),
                inflight: Arc::default(),
            }),
            is_client_healthy: true.into(),
            regenerate_retry_policy: config.regenerate_retry_policy,
            connection_lost_sleep: config.connection_lost_sleep,
        };
        Ok(container)
    }

    pub fn epoch(&self) -> u64 {
        self.current.read().epoch
    }

    pub fn is_healthy(&self) -> bool {
        self.is_client_healthy.load(Ordering::Relaxed)
    }

    /// Marks the container unhealthy if `epoch` is the current client's, and returns whether
    /// that turned it from healthy to unhealthy. A failure reported against a replaced client
    /// changes nothing. Replacing the client is left to the caller.
    pub fn report_connection_failed(&self, epoch: u64) -> bool {
        let current = self.current.read();
        if current.epoch != epoch {
            return false;
        }
        self.is_client_healthy.swap(false, Ordering::Relaxed)
    }

    /// Returns the current client for a request and the metadata used to report a request
    /// failure. The metadata contains the returned client's epoch. Hold the client for one
    /// request only: a replaced client stays open until every clone of it is dropped.
    pub fn request_client(
        &self,
        address: Option<Address>,
    ) -> (ServiceRequestMeta, Arc<ClientType>) {
        let (current, meta) = self.read_current(address);
        (meta, current.client.clone())
    }

    /// Returns the current client for a request of `kind`, the metadata used to report its
    /// failure, and a guard that counts it as in flight on that client until dropped. Hold the
    /// client and the guard for one request only.
    pub fn lease(
        &self,
        kind: RequestKind,
        address: Option<Address>,
    ) -> (ServiceRequestMeta, Arc<ClientType>, InflightGuard) {
        let (current, meta) = self.read_current(address);
        current.inflight.count(kind).fetch_add(1, Ordering::Relaxed);
        let guard = InflightGuard {
            requests: current.inflight.clone(),
            kind,
        };
        (meta, current.client.clone(), guard)
    }

    /// The number of requests of `kind` leased on the current client and still in flight.
    pub fn inflight(&self, kind: RequestKind) -> usize {
        self.current
            .read()
            .inflight
            .count(kind)
            .load(Ordering::Relaxed)
    }

    /// The number of requests of every kind leased on the current client and still in flight.
    pub fn inflight_total(&self) -> usize {
        let current = self.current.read();
        RequestKind::ALL
            .into_iter()
            .map(|kind| current.inflight.count(kind).load(Ordering::Relaxed))
            .sum()
    }

    /// Reads the current client together with the metadata that reports a failure on it.
    fn read_current(
        &self,
        address: Option<Address>,
    ) -> (
        RwLockReadGuard<'_, CurrentClient<ClientType>>,
        ServiceRequestMeta,
    ) {
        let current = self.current.read();
        let meta = ServiceRequestMeta {
            client_epoch: current.epoch,
            address,
        };
        (current, meta)
    }

    pub async fn connection_stats(&self) -> Option<ConnectionStats> {
        let client = self.current.read().client.clone();
        client.connection_stats().await
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
        if self.epoch() != expected_epoch {
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

            let new_client = Arc::new(new_client);
            // Release the write lock before dropping the old client. If this is its last
            // reference, closing it must not block lookups.
            let _old_client = {
                let mut current = self.current.write();
                let epoch = current.epoch + 1;
                let old_client = std::mem::replace(
                    &mut *current,
                    CurrentClient {
                        epoch,
                        client: new_client,
                        inflight: Arc::default(),
                    },
                );
                // Under the write lock, so that a failure reported against the old client cannot
                // land after this and mark the new one unhealthy.
                self.is_client_healthy.store(true, Ordering::Relaxed);
                old_client
            };

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
