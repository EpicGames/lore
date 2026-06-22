// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Rotating-ID fixed topology for cross-region connection cycling.
//!
//! [`RotatingIdFixedTopology`] wraps a static peer list but periodically
//! regenerates each peer's ID with a random suffix. Downstream consumers
//! that key connections on peer ID will tear down and re-establish
//! connections on each rotation, distributing load across remote endpoints.
//!
//! The peer addresses and ports remain constant — only the IDs change.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lore_revision::cluster::peer::PeerInfo;
use lore_revision::cluster::topology::RefreshLoopError;
use lore_revision::cluster::topology::Topology;
use rand::Rng;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Receiver;
use tokio::time::MissedTickBehavior;
use tracing::info;
use tracing::warn;

use crate::topology::RotatingIdFixedTopologySettings;

/// Buffer capacity for peer update notifications.
const PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY: usize = 10;

/// A fixed-peer topology that rotates peer IDs on a configurable interval.
///
/// Each tick of the refresh loop broadcasts the same set of peers with
/// freshly generated random IDs
#[derive(Debug)]
pub struct RotatingIdFixedTopology {
    /// The set of configured peers.
    peers: HashSet<PeerInfo>,

    /// How often the ID of Peers is rotated
    pub rotation_interval: Duration,

    peers_updated_broadcaster: broadcast::Sender<HashSet<PeerInfo>>,
}

impl RotatingIdFixedTopology {
    /// Creates a new topology from configuration.
    pub fn from_settings(config: &RotatingIdFixedTopologySettings) -> Arc<Self> {
        info!(
            peer_count = config.peers.len(),
            rotation_interval_seconds = config.rotation_interval_seconds,
            "Creating rotating id fixed topology"
        );

        let (peers_updated_broadcaster, _) =
            broadcast::channel::<HashSet<PeerInfo>>(PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY);

        let peers: HashSet<PeerInfo> = config
            .peers
            .iter()
            .map(|peer| PeerInfo {
                // will get rotated upon read
                id: "".into(),
                address: peer.address.clone(),
                port: peer.port,
                locality: peer.locality,
                // peer list is static, so address is safe to use as a metric label
                metric_id: peer.address.clone(),
            })
            .collect();

        Arc::new(Self {
            peers,
            rotation_interval: Duration::from_secs(config.rotation_interval_seconds),
            peers_updated_broadcaster,
        })
    }
}

fn rotated_peer(peer: &PeerInfo) -> PeerInfo {
    let rand_identifier: String = rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(4)
        .map(char::from)
        .collect();

    let mut new_peer = peer.clone();
    new_peer.id = format!("RotatingPeer_'{rand_identifier}'");
    new_peer
}

#[async_trait]
impl Topology for RotatingIdFixedTopology {
    fn supports_refresh_loop(&self) -> bool {
        true
    }

    async fn refresh_loop(self: Arc<Self>) -> Result<(), RefreshLoopError> {
        let mut interval = tokio::time::interval(self.rotation_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;

            let peers = self.peers.iter().map(rotated_peer).collect();

            if let Err(error) = self.peers_updated_broadcaster.send(peers) {
                warn!(error = ?error, "failed to send Rotated ID peers to subscriber");
                // todo(plockhart) we may want to bail out of the task with repeated failures
            }
        }
    }

    fn subscribe_to_peer_refreshes(self: Arc<Self>) -> Receiver<HashSet<PeerInfo>> {
        self.peers_updated_broadcaster.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lore_revision::cluster::peer::Locality;

    use super::*;
    use crate::topology::PeerSettings;

    #[test]
    fn empty_peers_succeeds() {
        let config = RotatingIdFixedTopologySettings {
            peers: vec![],
            rotation_interval_seconds: 10,
        };

        let _ = RotatingIdFixedTopology::from_settings(&config);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn topology_refresh_changes_id() {
        let peer_address = "example.com";
        let peer_port = 9090;
        let peer_locality = Locality::SameRegion;

        let config = RotatingIdFixedTopologySettings {
            peers: vec![PeerSettings {
                address: peer_address.to_string(),
                port: peer_port,
                locality: peer_locality,
            }],
            rotation_interval_seconds: 1,
        };
        let topology = RotatingIdFixedTopology::from_settings(&config);
        {
            let topology = topology.clone();
            let _task = tokio::spawn(async move {
                topology
                    .refresh_loop()
                    .await
                    .expect("refresh should not fail");
            });
        }

        let mut receiver = topology.subscribe_to_peer_refreshes();

        let previous_id;
        {
            let result = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
                .await
                .expect("Timeout waiting for peers (round 1)");
            match result {
                Ok(peers) => {
                    assert_eq!(peers.len(), 1);
                    let peer = peers.iter().next().expect("Should have one peer");

                    previous_id = peer.id.clone();
                    assert_eq!(peer.address, peer_address);
                    assert_eq!(peer.port, peer_port);
                    assert_eq!(peer.locality, peer_locality);
                }
                Err(e) => panic!("round 1 receive error: {e:?}"),
            }
        }
        {
            let result = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
                .await
                .expect("Timeout waiting for peers (round 2)");
            match result {
                Ok(peers) => {
                    assert_eq!(peers.len(), 1);
                    let peer = peers.iter().next().expect("Should have one peer");

                    assert_ne!(peer.id, previous_id);
                    assert_eq!(peer.address, peer_address);
                    assert_eq!(peer.port, peer_port);
                    assert_eq!(peer.locality, peer_locality);
                }
                Err(e) => panic!("round 2 receive error: {e:?}"),
            }
        }
    }
}
