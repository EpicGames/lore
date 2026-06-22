// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Fixed topology implementation for static peer lists.
//!
//! This module provides a built-in fixed/static topology discovery mechanism:
//! - [`FixedTopology`] - A topology with statically configured peers
//!
//! This is a core/built-in feature (not a plugin) and is useful for:
//! - Development and testing environments
//! - Small deployments with known, static peer configurations
//! - Environments where service discovery is not available
//!
//! # Configuration
//!
//! Fixed topology is configured using the predefined type format:
//!
//! ```toml
//! [topology]
//! provider = "fixed"
//!
//! [topology.fixed]
//! peers = [
//!     { address = "192.168.1.10", port = 9090, locality = "SameRegion" },
//!     { address = "192.168.1.11", port = 9090, locality = "OtherRegion" },
//! ]
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use lore_revision::cluster::peer::PeerInfo;
use lore_revision::cluster::topology::RefreshLoopError;
use lore_revision::cluster::topology::Topology;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Receiver;
use tracing::info;

use crate::topology::FixedTopologySettings;

/// Buffer capacity for peer update notifications.
const PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY: usize = 10;

/// A topology implementation with a fixed, static list of peers.
///
/// This topology is useful for development, testing, and small deployments
/// where the peer list is known at configuration time and does not change.
///
/// Unlike dynamic topology implementations (like Consul), this topology
/// does not support refresh loops - the peer list is set once at creation
/// and remains constant for the lifetime of the topology instance.
#[derive(Debug)]
pub struct FixedTopology {
    /// The set of configured peers.
    peers: HashSet<PeerInfo>,

    /// Broadcaster for peer update notifications.
    ///
    /// Subscribers receive the peer list immediately upon subscription
    /// since this topology is typically used in testing scenarios where
    /// immediate peer availability is desired.
    peers_updated_broadcaster: broadcast::Sender<HashSet<PeerInfo>>,
}

impl FixedTopology {
    /// Creates a new fixed topology from configuration.
    pub fn from_settings(config: &FixedTopologySettings) -> Arc<Self> {
        info!(peer_count = config.peers.len(), "Creating fixed topology");

        let (peers_updated_broadcaster, _) =
            broadcast::channel::<HashSet<PeerInfo>>(PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY);

        let peers: HashSet<PeerInfo> = config
            .peers
            .iter()
            .map(|peer| PeerInfo {
                id: format!(
                    "FixedPeer ({}) {}:{}",
                    peer.locality, peer.address, peer.port
                ),
                address: peer.address.clone(),
                port: peer.port,
                locality: peer.locality,
                // peer list is static, so address is safe to use as a metric label
                metric_id: peer.address.clone(),
            })
            .collect();

        Arc::new(Self {
            peers,
            peers_updated_broadcaster,
        })
    }
}

#[async_trait]
impl Topology for FixedTopology {
    fn supports_refresh_loop(&self) -> bool {
        false
    }

    async fn refresh_loop(self: Arc<Self>) -> Result<(), RefreshLoopError> {
        Err(RefreshLoopError::internal("not supported"))
    }

    fn subscribe_to_peer_refreshes(self: Arc<Self>) -> Receiver<HashSet<PeerInfo>> {
        let subscriber = self.peers_updated_broadcaster.subscribe();
        // This topology is typically used in testing frameworks where we want an immediate
        // update to be done, so broadcast an update straight away for the receiver to get.
        if let Err(error) = self.peers_updated_broadcaster.send(self.peers.clone()) {
            tracing::error!(?error, "failed to send peers to recent subscriber");
        }
        subscriber
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lore_revision::cluster::peer::Locality;

    use super::*;
    use crate::topology::PeerSettings;

    #[test]
    fn test_fixed_topology_from_config() {
        let config = FixedTopologySettings {
            peers: vec![
                PeerSettings {
                    address: "192.168.1.10".to_string(),
                    port: 9090,
                    locality: Locality::SameRegion,
                },
                PeerSettings {
                    address: "192.168.1.11".to_string(),
                    port: 9091,
                    locality: Locality::SameRegion,
                },
            ],
        };

        let topology = FixedTopology::from_settings(&config);
        assert!(!topology.supports_refresh_loop());
    }

    #[test]
    fn test_fixed_topology_empty_peers_succeeds() {
        // Empty peers should succeed - an empty topology is valid
        // (though perhaps not useful in practice)
        let config = FixedTopologySettings { peers: vec![] };

        let _ = FixedTopology::from_settings(&config);
    }

    #[tokio::test]
    async fn test_fixed_topology_does_not_support_refresh_loop() {
        let config = FixedTopologySettings {
            peers: vec![PeerSettings {
                address: "localhost".to_string(),
                port: 9090,
                locality: Locality::SameRegion,
            }],
        };

        let topology = FixedTopology::from_settings(&config);

        assert!(!topology.supports_refresh_loop());

        // refresh loop is not supported and should result in an error
        let result = topology.refresh_loop().await;
        assert!(result.is_err(), "Expected error, got: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_fixed_topology_subscribe_receives_peers_immediately() {
        let config = FixedTopologySettings {
            peers: vec![
                PeerSettings {
                    address: "192.168.1.10".to_string(),
                    port: 9090,
                    locality: Locality::SameRegion,
                },
                PeerSettings {
                    address: "192.168.1.11".to_string(),
                    port: 9091,
                    locality: Locality::SameRegion,
                },
            ],
        };
        let topology = FixedTopology::from_settings(&config);

        let mut receiver = topology.subscribe_to_peer_refreshes();

        // Should receive the peers immediately with a reasonable timeout
        let result = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("Timeout waiting for peers - should receive immediately");

        match result {
            Ok(peers) => {
                assert_eq!(peers.len(), 2);

                let peer1 = peers.iter().find(|p| p.address == "192.168.1.10");
                let peer2 = peers.iter().find(|p| p.address == "192.168.1.11");

                assert!(peer1.is_some(), "Expected peer with address 192.168.1.10");
                assert!(peer2.is_some(), "Expected peer with address 192.168.1.11");

                let peer1 = peer1.expect("peer1 should exist");
                let peer2 = peer2.expect("peer2 should exist");

                assert_eq!(peer1.port, 9090);
                assert_eq!(peer2.port, 9091);
                assert!(peer1.id.contains("192.168.1.10:9090"));
                assert!(peer2.id.contains("192.168.1.11:9091"));
            }
            Err(e) => panic!("Broadcast receive error: {e:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_fixed_topology_peer_info_format() {
        let config = FixedTopologySettings {
            peers: vec![PeerSettings {
                address: "test-host".to_string(),
                port: 1234,
                locality: Locality::SameRegion,
            }],
        };
        let topology = FixedTopology::from_settings(&config);

        let mut receiver = topology.subscribe_to_peer_refreshes();

        let result = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("Timeout waiting for peers");

        match result {
            Ok(peers) => {
                assert_eq!(peers.len(), 1);
                let peer = peers.iter().next().expect("Should have one peer");

                assert_eq!(peer.id, "FixedPeer (SameRegion) test-host:1234");
                assert_eq!(peer.address, "test-host");
                assert_eq!(peer.port, 1234);
            }
            Err(e) => panic!("Broadcast receive error: {e:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_fixed_topology_multiple_subscribers() {
        let config = FixedTopologySettings {
            peers: vec![PeerSettings {
                address: "multi-test".to_string(),
                port: 5000,
                locality: Locality::SameRegion,
            }],
        };
        let topology = FixedTopology::from_settings(&config);

        // Create multiple subscribers
        let mut receiver1 = topology.clone().subscribe_to_peer_refreshes();
        let mut receiver2 = topology.subscribe_to_peer_refreshes();

        // Both should receive peers
        let result1 = tokio::time::timeout(Duration::from_secs(1), receiver1.recv()).await;
        let result2 = tokio::time::timeout(Duration::from_secs(1), receiver2.recv()).await;

        match (result1, result2) {
            (Ok(Ok(peers1)), Ok(Ok(peers2))) => {
                assert_eq!(peers1.len(), 1);
                assert_eq!(peers2.len(), 1);
                assert_eq!(peers1, peers2);
            }
            _ => panic!("Both receivers should receive peers"),
        }
    }
}
