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
//! # Optional: leave this server's own entry out, so every server can share one peer list.
//! self_address = "192.168.1.10"
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
        let (peers_updated_broadcaster, _) =
            broadcast::channel::<HashSet<PeerInfo>>(PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY);

        let peers: HashSet<PeerInfo> = config
            .peers
            .iter()
            .filter(|peer| config.self_address.as_deref() != Some(peer.address.as_str()))
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

        info!(
            peer_count = peers.len(),
            self_address = config.self_address.as_deref(),
            "Creating fixed topology"
        );

        Arc::new(FixedTopology {
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
