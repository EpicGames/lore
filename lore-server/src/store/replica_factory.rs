// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::error::Error;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::Uri;
use lore_base::lore_spawn_net;
use lore_proto::rpc::replication_service_client::ReplicationServiceClient;
use lore_revision::cluster::peer::Locality;
use lore_revision::cluster::peer::PeerInfo;
use lore_revision::store::composite::ReplicationTarget;
use lore_revision::store::composite::replica_factory::ReplicaFactory;
use lore_revision::store::composite::replica_factory::ReplicaTargets;
use lore_revision::util::time::RetryPolicy;
use lore_transport::quic::client::CertificateSettings as QuicCertificateSettings;
use lore_transport::quic::client::CongestionAlgorithm;
use lore_transport::quic::client::DEFAULT_EXPECTED_RTT_MS;
use opentelemetry::KeyValue;
use serde::Deserialize;
use smallvec::smallvec;
use tonic::transport::Channel;
use tonic::transport::ClientTlsConfig;

use crate::quic::client_monitor::default_quic_client_monitor_interval_secs;
use crate::quic::replication_store_service::client_container;
use crate::quic::replication_store_service::client_container::ClientContainerConfig;
use crate::store::grpc_replica::GrpcReplica;
use crate::store::grpc_replica::ReplicationClient;
use crate::store::replica::Replica;
use crate::tls::CertificateSettings;

pub static METRICS_PEER_ID_LABEL: &str = "peer_id";
pub static METRICS_PEER_LOCALITY_LABEL: &str = "peer_locality";

#[derive(Clone, Debug, Deserialize)]
pub struct ReplicaFactoryTlsSettings {
    pub client_certs: CertificateSettings,
    /// Server SNI (typically injected via environment variables at deployment time)
    pub server_sni: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReplicaFactorySettings {
    pub tls: Option<ReplicaFactoryTlsSettings>,
    pub client_message_buffer: usize,
    /// Enable QUIC-backed read replicas alongside write replicas.
    #[serde(default = "default_read_replicas_enabled")]
    pub read_replicas_enabled: bool,
    /// Use gRPC for write replication instead of QUIC. Defaults to true.
    #[serde(default = "default_use_grpc_write_replication")]
    pub use_grpc_write_replication: bool,
    #[serde(default = "default_quic_client_monitor_interval_secs")]
    pub quic_client_monitor_interval_seconds: u64,
    /// Flag to dictate whether write replication should be enabled
    /// for peers with the `SameRegion` locality.
    #[serde(default = "default_enable_same_region_write")]
    pub enable_same_region_write: bool,
}

fn default_read_replicas_enabled() -> bool {
    true
}

fn default_use_grpc_write_replication() -> bool {
    false
}

fn default_enable_same_region_write() -> bool {
    true
}

#[derive(Debug)]
pub struct ReplicationStoreTargetFactory {
    grpc_tls: Option<ClientTlsConfig>,
    quic_certs: QuicCertificateSettings,
    sni_override: Option<String>,
    client_message_buffer: usize,
    read_replicas_enabled: bool,
    use_grpc_write_replication: bool,
    pub quic_monitor_interval: Duration,
    pub enable_same_region_write: bool,
}

impl ReplicationStoreTargetFactory {
    pub fn new(
        grpc_tls: Option<ClientTlsConfig>,
        quic_certs: QuicCertificateSettings,
        sni_override: Option<String>,
        client_message_buffer: usize,
        read_replicas_enabled: bool,
        use_grpc_write_replication: bool,
    ) -> Self {
        Self {
            grpc_tls,
            quic_certs,
            sni_override,
            client_message_buffer,
            read_replicas_enabled,
            use_grpc_write_replication,
            quic_monitor_interval: Duration::from_secs(default_quic_client_monitor_interval_secs()),
            enable_same_region_write: true,
        }
    }

    async fn make_grpc_write_target(
        &self,
        peer_info: &PeerInfo,
    ) -> Result<ReplicationTarget, Box<dyn Error + Send + Sync>> {
        let scheme = if self.grpc_tls.is_some() {
            "https"
        } else {
            "http"
        };
        let authority = format!("{scheme}://{}:{}", peer_info.address, peer_info.port);
        let url = Uri::from_str(&authority)?;
        let mut endpoint = Channel::builder(url);
        if let Some(tls) = &self.grpc_tls {
            endpoint = endpoint.tls_config(tls.clone())?;
        }
        // Connect from net so the hyper/h2 driver tasks this spawns bind there
        // rather than to the core runtime the caller runs on.
        let channel = lore_spawn_net!(async move { endpoint.connect().await }).await??;
        let grpc_client = ReplicationServiceClient::new(channel);

        let replication_client = ReplicationClient::new(
            grpc_client,
            self.client_message_buffer,
            RetryPolicy::builder()
                .with_initial_backoff_millis(50)
                .with_max_backoff_millis(1000)
                // do not retry
                .with_limit(0)
                .build(),
        );
        let store = GrpcReplica::new(replication_client);
        Ok(ReplicationTarget::new(peer_info.clone(), Arc::new(store)))
    }

    async fn make_quic_target(
        &self,
        peer_info: &PeerInfo,
    ) -> Result<ReplicationTarget, Box<dyn Error + Send + Sync>> {
        let scheme = if self.quic_certs.client.is_some() {
            "quics"
        } else {
            "quic"
        };
        let remote_url = format!("{scheme}://{}:{}", peer_info.address, peer_info.port);

        let sni_override = if peer_info.address.parse::<IpAddr>().is_ok() {
            self.sni_override.clone()
        } else {
            None
        };

        let rtt_ms;
        let congestion_algorithm;
        match peer_info.locality {
            Locality::SameRegion => {
                rtt_ms = 10;
                // Communication within a region will have no packet loss
                // so we don't need to worry about Cubic's aggressive ramp down
                // of cwnd in the event of packet loss - we don't see it happening.
                // The benefit of Cubic is that it only adjusts the cwnd in the event of
                // packet loss. So quiet periods of time don't inadvertently scale down
                // the cwnd then get blindsided by a large get/put message causing latency spikes.
                // We want same region replication to be as fast as possible
                congestion_algorithm = CongestionAlgorithm::Cubic;
            }
            Locality::OtherRegion => {
                // todo(plockhart) configure expected_rtt_ms based off latency to replication target
                rtt_ms = DEFAULT_EXPECTED_RTT_MS;
                // We see packet loss in cross region communication. Bbr readjusts the cwnd within
                // a few cycles of RTT, much faster than Cubic at recoverying from packet loss, at
                // the expensive that periodically the internals of the algorithm ramp down cwnd
                // based off bandwidth usage (which means quiet periods inadvertently reduce cwnd)
                congestion_algorithm = CongestionAlgorithm::Bbr;
            }
        };

        let mut factory =
            client_container::QuicClientFactory::new(remote_url, self.quic_certs.clone());
        factory.command_behavior.message_limit = self.client_message_buffer;
        factory.command_behavior.should_await_command_permit = false;
        factory.quic_max_reconnects = Some(5);
        factory.sni_override = sni_override;
        factory.transport_config.expected_rtt_ms = rtt_ms;
        factory.transport_config.congestion_algorithm = congestion_algorithm;

        let container_config = ClientContainerConfig {
            regenerate_retry_policy: RetryPolicy::builder()
                .with_initial_backoff_millis(100)
                .with_max_backoff_millis(1000)
                .with_limit(10)
                .build(),
            connection_lost_sleep: Duration::from_secs(1),
        };

        let replica = Replica::new(
            Arc::new(factory),
            container_config,
            smallvec![
                KeyValue::new(METRICS_PEER_ID_LABEL, peer_info.metric_id.clone()),
                KeyValue::new(METRICS_PEER_LOCALITY_LABEL, peer_info.locality.as_str())
            ],
        )
        .await?;
        let replica = Arc::new(replica);
        replica.setup_client_stats_monitor(self.quic_monitor_interval);

        Ok(ReplicationTarget::new(peer_info.clone(), replica))
    }
}

#[async_trait]
impl ReplicaFactory for ReplicationStoreTargetFactory {
    async fn make_replica_target(
        &self,
        peer_info: &PeerInfo,
    ) -> Result<ReplicaTargets, Box<dyn Error + Send + Sync>> {
        let appropriate_for_read: bool;
        let appropriate_for_write: bool;
        match peer_info.locality {
            Locality::SameRegion => {
                appropriate_for_read = true;
                appropriate_for_write = self.enable_same_region_write;
            }
            Locality::OtherRegion => {
                appropriate_for_read = false;
                appropriate_for_write = true;
            }
        }

        let write = if appropriate_for_write {
            // todo(plockhart) remove GRPC write replication after we gain confidence in QUIC
            // we only have the infrastructure setup for cross-region communication via UDP
            // so only use write replication where we can, if that is our preference
            if self.use_grpc_write_replication && peer_info.locality == Locality::SameRegion {
                Some(self.make_grpc_write_target(peer_info).await?)
            } else {
                Some(self.make_quic_target(peer_info).await?)
            }
        } else {
            None
        };

        let read = if appropriate_for_read && self.read_replicas_enabled {
            Some(self.make_quic_target(peer_info).await?)
        } else {
            None
        };

        Ok(ReplicaTargets { read, write })
    }
}
