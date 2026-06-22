// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::lore_spawn;
use lore_revision::cluster::peer::PeerInfo;
use lore_revision::cluster::topology::RefreshLoopError;
use lore_revision::cluster::topology::Topology;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;
use tracing::info;
use tracing::warn;

/// Buffer capacity for peer update notifications.
const PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY: usize = 10;

/// A single topology source within a [`CompositeTopology`].
///
/// Each source wraps an inner [`Topology`] and maintains a cached snapshot
/// of the peers most recently reported by that topology. The cache is
/// updated whenever the inner topology broadcasts a change, allowing the
/// composite to re-merge all sources without polling.
#[derive(Debug)]
struct CompositeSource {
    /// The inner topology that provides peer updates.
    topology: Arc<dyn Topology + Send + Sync>,

    /// Most recent peer set received from this source.
    ///
    /// Written by the per-source subscription task and read when any source
    /// emits an update so the composite can union all cached sets.
    cached_peer_infos: RwLock<HashSet<PeerInfo>>,
}

/// A topology that merges peers from multiple underlying topology sources.
///
/// `CompositeTopology` subscribes to each source topology's peer updates,
/// caches the latest peer set per source, and broadcasts the union of all
/// cached sets whenever any source changes.
#[derive(Debug)]
pub struct CompositeTopology {
    /// The set of topology sources whose peers are merged.
    composite_sources: Vec<Arc<CompositeSource>>,

    /// Broadcaster for the merged peer set.
    ///
    /// Subscribers receive the full union of all source peer sets each time
    /// any individual source reports a change.
    peers_updated_broadcaster: broadcast::Sender<HashSet<PeerInfo>>,
}

impl CompositeTopology {
    pub fn from_sources(sources: Vec<Arc<dyn Topology + Send + Sync>>) -> Arc<Self> {
        info!(num_sources = sources.len(), "Creating Composite Topology");

        let mut composite_sources: Vec<Arc<CompositeSource>> = Vec::with_capacity(sources.len());

        for source in sources {
            let composite_source: Arc<_> = CompositeSource {
                topology: source.clone(),
                cached_peer_infos: HashSet::new().into(),
            }
            .into();

            composite_sources.push(composite_source);
        }

        let (peers_updated_broadcaster, _) =
            broadcast::channel::<HashSet<PeerInfo>>(PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY);
        let composite_topology = Self {
            composite_sources,
            peers_updated_broadcaster,
        };
        Arc::new(composite_topology)
    }
}

#[async_trait]
impl Topology for CompositeTopology {
    fn supports_refresh_loop(&self) -> bool {
        true
    }

    async fn refresh_loop(self: Arc<Self>) -> Result<(), RefreshLoopError> {
        let mut refresh_loops = JoinSet::new();

        let (source_updated_broadcaster, mut source_updated_receiver) =
            broadcast::channel::<()>(PEERS_UPDATED_NOTIFICATION_BUFFER_CAPACITY);

        // subscribe to source changes - caching the peers and then notify composite topology channel
        let mut subscriptions = Vec::with_capacity(self.composite_sources.len());
        for source in &self.composite_sources {
            let source_cloned = source.clone();
            let mut subscription = source_cloned.topology.clone().subscribe_to_peer_refreshes();
            let source_updated_broadcaster = source_updated_broadcaster.clone();
            let subscribe_task = AbortOnDropHandle::new(lore_spawn!(async move {
                loop {
                    let change_event = match subscription.recv().await {
                        Ok(change_event) => change_event,
                        Err(error) => {
                            info!("composite topology source receive error {error:?}");
                            match error {
                                RecvError::Closed => {
                                    info!("stopping composite source topology subscription");
                                    break;
                                }
                                RecvError::Lagged(_) => {
                                    continue;
                                }
                            };
                        }
                    };
                    let mut write = source_cloned.cached_peer_infos.write().await;
                    *write = change_event;
                    // notify the composite topology that something has changed
                    if let Err(error) = source_updated_broadcaster.send(()) {
                        warn!(error = ?error, "failed to send updated peers to composite");
                    }
                }
            }));
            subscriptions.push(subscribe_task);

            // run the refresh loop for this topology so we get subsequent updates
            if source.topology.supports_refresh_loop() {
                let source_cloned = source.clone();
                lore_spawn!(refresh_loops, async move {
                    let topology = source_cloned.topology.clone();
                    topology.refresh_loop().await.map_err(anyhow::Error::from)
                });
            }
        }

        loop {
            match source_updated_receiver.recv().await {
                Ok(change_event) => change_event,
                Err(error) => {
                    info!("composite topology sources receive error {error:?}");
                    match error {
                        RecvError::Closed => {
                            info!("stopping composite topology sources subscription");
                            return Ok(());
                        }
                        RecvError::Lagged(_) => {
                            continue;
                        }
                    };
                }
            };

            let mut total_peers = HashSet::new();
            for source in &self.composite_sources {
                let peers = source.cached_peer_infos.read().await;
                total_peers.extend(peers.clone());
            }

            if let Err(error) = self.peers_updated_broadcaster.send(total_peers) {
                warn!(error = ?error, "failed to send updated peers from composite");
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

    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::cluster::peer::Locality;
    use lore_revision::cluster::topology::Topology;

    use super::*;
    use crate::topology::FixedTopologySettings;
    use crate::topology::PeerSettings;
    use crate::topology::RotatingIdFixedTopologySettings;
    use crate::topology::fixed::FixedTopology;
    use crate::topology::rotating_id_fixed::RotatingIdFixedTopology;
    use crate::util::setup_test_execution;

    fn make_fixed_topology(peers: Vec<PeerSettings>) -> Arc<dyn Topology + Send + Sync> {
        FixedTopology::from_settings(&FixedTopologySettings { peers })
    }

    fn make_peer(address: &str, port: u16, locality: Locality) -> PeerSettings {
        PeerSettings {
            address: address.to_string(),
            port,
            locality,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preserves_locality_from_sources() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                let source = make_fixed_topology(vec![
                    make_peer("same-region", 1000, Locality::SameRegion),
                    make_peer("other-region", 2000, Locality::OtherRegion),
                ]);

                let topology = CompositeTopology::from_sources(vec![source]);
                let mut receiver = topology.clone().subscribe_to_peer_refreshes();

                let loop_topology = topology.clone();
                let _task = lore_spawn!(async move {
                    let _ = loop_topology.refresh_loop().await;
                });

                let peers = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                    .await
                    .expect("Timeout waiting for composite peers")
                    .expect("Broadcast receive error");

                let same = peers
                    .iter()
                    .find(|p| p.address == "same-region")
                    .expect("missing same-region peer");
                let other = peers
                    .iter()
                    .find(|p| p.address == "other-region")
                    .expect("missing other-region peer");
                assert_eq!(same.locality, Locality::SameRegion);
                assert_eq!(other.locality, Locality::OtherRegion);
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deduplicates_identical_peers_across_sources() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                // Both sources have the same peer — FixedTopology generates IDs
                // deterministically from address:port, so they deduplicate via HashSet.
                let peer = make_peer("shared-host", 3000, Locality::SameRegion);
                let source_a = make_fixed_topology(vec![peer.clone()]);
                let source_b = make_fixed_topology(vec![peer]);

                let topology = CompositeTopology::from_sources(vec![source_a, source_b]);
                let mut receiver = topology.clone().subscribe_to_peer_refreshes();

                let loop_topology = topology.clone();
                let _task = lore_spawn!(async move {
                    let _ = loop_topology.refresh_loop().await;
                });

                let mut last_peers: Option<HashSet<PeerInfo>> = None;
                // clear out the initial notifications from first time registrations
                // emitted by each fixed topology and get to a stable empty receive
                tokio::time::sleep(Duration::from_secs(2)).await;
                while let Ok(peer) = receiver.try_recv() {
                    last_peers = Some(peer);
                }

                let last_peers = last_peers.expect("last_peers should be Some");
                assert_eq!(last_peers.len(), 1);
                assert_eq!(last_peers.iter().next().unwrap().address, "shared-host");
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receives_updates_from_a_mix_of_sources() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                let rotating =
                    RotatingIdFixedTopology::from_settings(&RotatingIdFixedTopologySettings {
                        peers: vec![make_peer("rotating-host", 4000, Locality::OtherRegion)],
                        rotation_interval_seconds: 1,
                    });
                let fixed =
                    make_fixed_topology(vec![make_peer("fixed-host", 5000, Locality::SameRegion)]);

                let topology = CompositeTopology::from_sources(vec![
                    fixed,
                    rotating as Arc<dyn Topology + Send + Sync>,
                ]);
                let mut receiver = topology.clone().subscribe_to_peer_refreshes();

                let loop_topology = topology.clone();
                let _task = lore_spawn!(async move {
                    let _ = loop_topology.refresh_loop().await;
                });

                // clear out the initial notifications from first time registrations
                // emitted by each fixed topology and get to a stable empty receive
                tokio::time::sleep(Duration::from_secs(2)).await;
                loop {
                    if receiver.try_recv().is_err() {
                        break;
                    }
                }

                // First update driven by Rotating Topology — should contain both fixed and rotating peers
                let peers1 = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                    .await
                    .expect("Timeout waiting for first update")
                    .expect("Broadcast error");
                assert_eq!(peers1.len(), 2);

                assert!(
                    peers1
                        .iter()
                        .any(|p| p.address == "fixed-host" && p.port == 5000)
                );
                let rotating_peer1 = peers1
                    .iter()
                    .find(|p| p.address == "rotating-host")
                    .expect("missing rotating-host peer");
                assert_eq!(rotating_peer1.port, 4000);
                assert_eq!(rotating_peer1.locality, Locality::OtherRegion);
                let first_id = rotating_peer1.id.clone();

                // Second update — rotating ID should have changed, fixed peer still present
                let peers2 = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                    .await
                    .expect("Timeout waiting for second update")
                    .expect("Broadcast error");
                assert_eq!(peers2.len(), 2);

                assert!(peers2.iter().any(|p| p.address == "fixed-host"));
                let rotating_peer2 = peers2
                    .iter()
                    .find(|p| p.address == "rotating-host")
                    .expect("missing rotating-host peer");
                assert_ne!(rotating_peer2.id, first_id);
            })
            .await;
    }
}
