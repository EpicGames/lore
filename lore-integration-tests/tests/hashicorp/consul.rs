// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod client;
mod peer_discovery;

#[cfg(all(test, feature = "integration_tests"))]
mod helpers {

    use lore_hashicorp::consul::ConsulClient;
    use lore_hashicorp::consul::client::RsConsul;
    use rs_consul::Config;
    use rs_consul::Consul;

    pub fn docker_compose_config() -> Config {
        Config {
            address: "http://127.0.0.1:8500".to_string(),
            token: None,
            ..Default::default()
        }
    }

    pub fn make_compose_client() -> Box<dyn ConsulClient + Send + Sync> {
        let wrapper: RsConsul = Consul::new(docker_compose_config()).into();
        Box::new(wrapper)
    }
}
