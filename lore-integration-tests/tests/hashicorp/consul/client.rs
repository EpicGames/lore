// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Sanity checks API calls and responses.
// Run `docker compose up` or set env variable to point to consul running in our cluster.
// When testing remotely, you might want to see the DBG output as well:
// `cargo test --package lore-integration-tests -- --no-capture`

#[cfg(all(test, feature = "integration_tests"))]
mod rs_consul_client_interactions {

    use std::env;

    use lore_hashicorp::consul::ConsulClient;
    use lore_hashicorp::consul::client::RsConsul;
    use rs_consul::Config;
    use rs_consul::Consul;
    use rs_consul::ConsulError;
    use rs_consul::GetServiceNodesRequest;

    #[allow(clippy::dbg_macro)]
    #[ignore]
    #[tokio::test]
    async fn service_nodes() -> Result<(), ConsulError> {
        let (is_local, consul_address) = if let Ok(raw_address) = env::var("CONSUL_REMOTE_ADDRESS")
        {
            (false, raw_address)
        } else {
            (true, "http://127.0.0.1:8500".to_string())
        };

        let consul_config = Config {
            address: consul_address,
            token: None,
            ..Default::default()
        };

        // interact via trait to prove abstraction
        let consul: Box<dyn ConsulClient> = {
            let wrapper: RsConsul = Consul::new(consul_config).into();
            Box::new(wrapper)
        };

        let nomad_lore_nodes = consul
            .get_service_nodes(
                GetServiceNodesRequest {
                    service: "nomad-lore",
                    near: None,
                    passing: true,
                    filter: None,
                },
                None,
            )
            .await?;
        // locally there will be no service `nomad-lore`
        // but against a valid cloud deployment you will get nodes back
        if is_local {
            assert!(nomad_lore_nodes.response.is_empty());
        } else {
            assert!(!nomad_lore_nodes.response.is_empty());
            dbg!(&nomad_lore_nodes.response);
        }

        let nodes_for_consul_service = consul
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
        // locally there is 1 node running the consul service in the compose file
        // but in the infra there will be 1 or more
        if is_local {
            assert!(nodes_for_consul_service.response.len() == 1);
        } else {
            assert!(!nodes_for_consul_service.response.is_empty());
            dbg!(&nodes_for_consul_service.response);
        }

        Ok(())
    }
}
