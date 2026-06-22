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
    async fn make_client(&self) -> Result<Self::Output, ProtocolError>;
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

    async fn make_client(&self) -> Result<Self::Output, ProtocolError> {
        let client = ReplicationStoreClient::connect(
            &self.remote_url,
            self.certs.clone(),
            self.sni_override.clone(),
            self.transport_config.clone(),
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
        let client = client_factory.make_client().await?;
        let container = Self {
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

        async move {
            match reason {
                GenerateClientReason::PeriodicRefresh => {}
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
                    .make_client()
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
