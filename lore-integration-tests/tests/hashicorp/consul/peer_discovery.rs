// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Sanity checks Peer discovery and filtering.
// Run `docker compose up` before running tests

#[cfg(all(test, feature = "integration_tests"))]
mod peer_discovery_tests {

    use std::sync::Arc;

    use lore_hashicorp::consul::service_peer_discovery::ServicePeerDiscoveryBuilder;
    use lore_revision::cluster::topology::Topology;
    use rs_consul::ConsulError;
    use rs_consul::GetServiceNodesRequest;

    use crate::hashicorp::consul::helpers::make_compose_client;

    // Locally there is only 1 node running a `consul` service.
    // This tests that we can fetch only the node running that service
    // and then we can also filter it out by address
    #[ignore]
    #[tokio::test]
    async fn peers() -> Result<(), ConsulError> {
        let nodes_for_consul_service = make_compose_client()
            .get_service_nodes(
                GetServiceNodesRequest {
                    service: "consul",
                    near: None,
                    passing: true,
                    filter: None,
                },
                None,
            )
            .await?;
        assert!(!nodes_for_consul_service.response.is_empty());
        let server_node_address = nodes_for_consul_service.response[0].node.address.clone();

        // discover peers without a filter should return the 1 node running consul
        {
            let discovery = Arc::new(
                ServicePeerDiscoveryBuilder::new(make_compose_client(), "consul".into()).build(),
            );

            let mut sub = discovery.clone().subscribe_to_peer_refreshes();
            discovery
                .refresh_peers()
                .await
                .expect("refresh should have worked");

            let peers = sub.recv().await.expect("recv should have worked");
            assert_eq!(peers.len(), 1);
        }

        // discover peers except we ignore the known node address running consule
        // will return nothing as there are no other nodes running that service
        {
            let discovery = Arc::new(
                ServicePeerDiscoveryBuilder::new(make_compose_client(), "consul".into())
                    .with_ignore_address(server_node_address)
                    .build(),
            );

            let mut sub = discovery.clone().subscribe_to_peer_refreshes();
            discovery
                .refresh_peers()
                .await
                .expect("refresh should have worked");

            let peers = sub.recv().await.expect("recv should have worked");
            assert!(peers.is_empty());
        }

        Ok(())
    }
}
