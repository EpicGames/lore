// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_revision::cluster::peer::Locality;
use lore_revision::cluster::topology::Topology;
use lore_server::topology::FixedTopologySettings;
use lore_server::topology::PeerSettings;
use lore_server::topology::fixed::*;

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
        self_address: None,
    };

    let topology = FixedTopology::from_settings(&config);
    assert!(!topology.supports_refresh_loop());
}

#[test]
fn test_fixed_topology_empty_peers_succeeds() {
    // Empty peers should succeed - an empty topology is valid
    // (though perhaps not useful in practice)
    let config = FixedTopologySettings {
        peers: vec![],
        self_address: None,
    };

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
        self_address: None,
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
        self_address: None,
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
        self_address: None,
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
        self_address: None,
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

async fn subscribed_peer_addresses(config: &FixedTopologySettings) -> Vec<String> {
    let mut receiver = FixedTopology::from_settings(config).subscribe_to_peer_refreshes();
    let peers = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("Timeout waiting for peers")
        .expect("Broadcast receive error");
    let mut addresses: Vec<String> = peers.into_iter().map(|peer| peer.address).collect();
    addresses.sort();
    addresses
}

fn same_region_peers(addresses: &[&str]) -> Vec<PeerSettings> {
    addresses
        .iter()
        .map(|address| PeerSettings {
            address: (*address).to_string(),
            port: 41340,
            locality: Locality::SameRegion,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fixed_topology_self_address_leaves_out_matching_peer() {
    let config = FixedTopologySettings {
        peers: same_region_peers(&["lore-0.lore", "lore-1.lore", "lore-2.lore"]),
        self_address: Some("lore-1.lore".to_string()),
    };

    assert_eq!(
        subscribed_peer_addresses(&config).await,
        vec!["lore-0.lore", "lore-2.lore"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fixed_topology_self_address_without_match_keeps_all_peers() {
    let config = FixedTopologySettings {
        peers: same_region_peers(&["lore-0.lore", "lore-1.lore"]),
        self_address: Some("lore-9.lore".to_string()),
    };

    assert_eq!(
        subscribed_peer_addresses(&config).await,
        vec!["lore-0.lore", "lore-1.lore"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fixed_topology_self_address_requires_exact_match() {
    // "lore-1.lore" must not match a peer whose address merely starts with it.
    let config = FixedTopologySettings {
        peers: same_region_peers(&["lore-1.lore", "lore-1.lore.svc"]),
        self_address: Some("lore-1.lore".to_string()),
    };

    assert_eq!(
        subscribed_peer_addresses(&config).await,
        vec!["lore-1.lore.svc"]
    );
}
