// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::cluster::peer::Locality;
use lore_revision::cluster::peer::PeerInfo;
use lore_revision::cluster::topology::Topology;
use lore_server::topology::FixedTopologySettings;
use lore_server::topology::PeerSettings;
use lore_server::topology::RotatingIdFixedTopologySettings;
use lore_server::topology::composite::*;
use lore_server::topology::fixed::FixedTopology;
use lore_server::topology::rotating_id_fixed::RotatingIdFixedTopology;

use super::*;
use crate::util::test_support::setup_test_execution;

fn make_fixed_topology(peers: Vec<PeerSettings>) -> Arc<dyn Topology + Send + Sync> {
    FixedTopology::from_settings(&FixedTopologySettings {
        peers,
        self_address: None,
    })
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

#[derive(Debug)]
struct StubTopology {
    sender: broadcast::Sender<HashSet<PeerInfo>>,
}

impl StubTopology {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(1);
        Self { sender }
    }
}

#[async_trait]
impl Topology for StubTopology {
    fn supports_refresh_loop(&self) -> bool {
        false
    }

    async fn refresh_loop(self: Arc<Self>) -> Result<(), RefreshLoopError> {
        Err(RefreshLoopError::internal("not supported"))
    }

    fn subscribe_to_peer_refreshes(self: Arc<Self>) -> broadcast::Receiver<HashSet<PeerInfo>> {
        let subscriber = self.sender.subscribe();
        let mut stub_peers = HashSet::new();
        stub_peers.insert(PeerInfo {
            id: "stub_consul_peer".to_string(),
            address: "stub_consul_peer.example.com".to_string(),
            port: 1234,
            locality: Locality::SameRegion,
            metric_id: "stub_consul_peer".into(),
        });
        self.sender.send(stub_peers).expect("should not fail");
        subscriber
    }
}

struct StubConsulTopologyFactory;

impl TopologyPluginFactory for StubConsulTopologyFactory {
    fn validate_config(&self, _config: &toml::Value) -> Result<(), PluginError> {
        Ok(())
    }

    fn create(
        &self,
        _config: &toml::Value,
    ) -> Result<Arc<dyn Topology + Send + Sync>, PluginError> {
        Ok(Arc::new(StubTopology::new()))
    }

    fn name(&self) -> &'static str {
        "consul"
    }
}

#[tokio::test]
async fn test_configure_composite_with_rotating_id_fixed_and_consul() {
    let execution = setup_test_execution();
    LORE_CONTEXT
        .scope(execution, async {
            let mut registry = PluginRegistry::new();
            registry.register_topology_plugin(Box::new(StubConsulTopologyFactory));

            let settings = TopologySettings {
                provider: TopologyProvider::Composite,
                fixed: None,
                rotating_id_fixed: None,
                composite: Some(CompositeTopologySettings {
                    sources: vec![
                        TopologySettings {
                            provider: TopologyProvider::RotatingIdFixed,
                            fixed: None,
                            rotating_id_fixed: Some(RotatingIdFixedTopologySettings {
                                peers: vec![PeerSettings {
                                    address: "fixed.example.com".to_string(),
                                    port: 41340,
                                    locality: Locality::OtherRegion,
                                }],
                                rotation_interval_seconds: 300,
                            }),
                            composite: None,
                        },
                        TopologySettings {
                            provider: TopologyProvider::Consul,
                            fixed: None,
                            rotating_id_fixed: None,
                            composite: None,
                        },
                    ],
                }),
            };

            let consul_config: toml::Value = toml::from_str(
                r#"
                        address = "http://consul.example.com:8500"
                        service_name = "urc-server"
                    "#,
            )
            .expect("valid toml");
            let mut plugin_configs = HashMap::new();
            plugin_configs.insert("consul".to_string(), consul_config);

            let result =
                configure_topology_with_registry(&registry, Some(&settings), &plugin_configs)
                    .expect("composite topology should configure successfully");
            let topology = result.expect("topology should be set");

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
            let last_peers = last_peers.expect("last_peers should be set");
            assert_eq!(last_peers.len(), 2);

            last_peers
                .iter()
                .find(|p| p.id == "stub_consul_peer")
                .expect("missing stub_consul_peer");

            last_peers
                .iter()
                .find(|p| p.address == "fixed.example.com")
                .expect("missing stub_consul_peer");
        })
        .await;
}
