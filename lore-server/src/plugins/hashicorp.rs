// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Consul topology plugin factory.
//!
//! This module provides a plugin factory for Consul-based topology discovery:
//! - [`ConsulTopologyPluginFactory`] - Creates Consul-backed topology instances

use std::env;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::PluginConfigError;
use lore_hashicorp::consul::client;
use lore_hashicorp::consul::client::Consul;
use lore_hashicorp::consul::client::RsConsul;
use lore_hashicorp::consul::service_peer_discovery::ServicePeerDiscoveryBuilder;
use lore_hashicorp::telemetry::NomadResourceDetector;
use lore_revision::cluster::topology::Topology;
use opentelemetry_sdk::resource::ResourceDetector;
use serde::Deserialize;
use tracing::info;

use crate::plugins::PluginError;
use crate::plugins::PluginRegistry;
use crate::plugins::TopologyPluginFactory;

/// Configuration for the Consul topology plugin.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsulTopologyPluginConfig {
    /// Optional Consul client config. Will read from environment if not set
    pub client_config: Option<client::Config>,

    /// Consul HTTP address. This is the concise form used by existing examples.
    #[serde(default)]
    pub address: Option<String>,

    /// Optional Consul ACL token. Prefer `CONSUL_HTTP_TOKEN` in production.
    #[serde(default)]
    pub token: Option<String>,

    /// Service name to discover peers for.
    pub service_name: String,

    /// Optional address to ignore (typically self) when discovering peers.
    #[serde(default)]
    pub ignore_address: Option<String>,

    /// Optional poll interval in seconds for refreshing the peer list.
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
}

impl fmt::Debug for ConsulTopologyPluginConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConsulTopologyPluginConfig")
            .field(
                "client_config_address",
                &self.client_config.as_ref().map(|config| &config.address),
            )
            .field("address", &self.address)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("service_name", &self.service_name)
            .field("ignore_address", &self.ignore_address)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .finish()
    }
}

impl ConsulTopologyPluginConfig {
    fn client_config(&self) -> client::Config {
        if let Some(config) = &self.client_config {
            return config.clone();
        }
        if let Some(address) = &self.address {
            return client::Config {
                address: address.clone(),
                token: self
                    .token
                    .clone()
                    .or_else(|| env::var("CONSUL_HTTP_TOKEN").ok()),
                ..Default::default()
            };
        }
        client::Config::from_env()
    }
}

/// Plugin factory for creating Consul-backed topology discovery.
pub struct ConsulTopologyPluginFactory;

impl TopologyPluginFactory for ConsulTopologyPluginFactory {
    fn name(&self) -> &'static str {
        "consul"
    }

    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        let plugin_name = self.name();

        let _plugin_config: ConsulTopologyPluginConfig =
            config.clone().try_into().map_err(|e| {
                PluginError::from(PluginConfigError {
                    plugin_name: plugin_name.to_string(),
                    message: format!("Failed to deserialize Consul topology config: {e}"),
                })
            })?;

        Ok(())
    }

    fn create(&self, config: &toml::Value) -> Result<Arc<dyn Topology + Send + Sync>, PluginError> {
        let plugin_name = self.name();

        let plugin_config: ConsulTopologyPluginConfig = config.clone().try_into().map_err(|e| {
            PluginError::from(PluginConfigError {
                plugin_name: plugin_name.to_string(),
                message: format!("Failed to deserialize Consul topology config: {e}"),
            })
        })?;

        info!(
            plugin_name = plugin_name,
            client_config = ?plugin_config.client_config.as_ref().map(|c| c.address.clone()),
            service_name = %plugin_config.service_name,
            ignore_address = ?plugin_config.ignore_address,
            poll_interval_secs = ?plugin_config.poll_interval_secs,
            "Creating Consul topology"
        );

        let consul_client_config = plugin_config.client_config();

        let consul_client = Consul::new(consul_client_config);
        let rs_consul: RsConsul = consul_client.into();

        let mut builder =
            ServicePeerDiscoveryBuilder::new(Box::new(rs_consul), plugin_config.service_name);

        if let Some(addr) = plugin_config.ignore_address {
            builder = builder.with_ignore_address(addr);
        }

        if let Some(interval_secs) = plugin_config.poll_interval_secs {
            builder = builder.with_poll_interval(Duration::from_secs(interval_secs));
        }

        let discovery = builder.build();

        Ok(Arc::new(discovery))
    }
}

/// Registers the `HashiCorp` plugins and resource detector with the given
/// registry.
///
/// The Consul topology plugin and the Nomad resource detector are registered
/// independently: Consul service discovery and Nomad workload orchestration are
/// distinct concerns that merely share the `HashiCorp` module, so the detector
/// is not tied to the topology factory.
pub fn register(registry: &mut PluginRegistry) {
    registry.register_topology_plugin(Box::new(ConsulTopologyPluginFactory));
    registry.register_resource_detector(|_runtime_handle| {
        Box::new(NomadResourceDetector) as Box<dyn ResourceDetector>
    });
}

#[cfg(test)]
mod tests {
    use tokio::runtime::Handle;

    use super::*;

    #[test]
    fn test_consul_topology_factory_name() {
        let factory = ConsulTopologyPluginFactory;
        assert_eq!(factory.name(), "consul");
    }

    #[test]
    fn top_level_address_is_used_and_token_is_redacted() {
        let config: ConsulTopologyPluginConfig = toml::from_str(
            r#"
address = "http://consul.internal:8500"
token = "do-not-log"
service_name = "lore"
"#,
        )
        .expect("valid Consul config");

        let client_config = config.client_config();
        assert_eq!(client_config.address, "http://consul.internal:8500");
        assert_eq!(client_config.token.as_deref(), Some("do-not-log"));
        assert!(!format!("{config:?}").contains("do-not-log"));
    }

    #[tokio::test]
    async fn test_register_adds_nomad_resource_detector() {
        let mut registry = PluginRegistry::new();
        register(&mut registry);

        // The module registers the Nomad resource detector independently of the
        // Consul topology plugin.
        assert_eq!(registry.resource_detectors(Handle::current()).len(), 1);
    }

    #[test]
    fn test_consul_topology_config_parsing_error() {
        let factory = ConsulTopologyPluginFactory;

        let config = toml::Value::Table(toml::map::Map::new());
        let result = factory.create(&config);

        let Err(e) = result else {
            panic!("Expected config error, got Ok");
        };
        let config_err = e
            .as_plugin_config_error()
            .expect("should be PluginConfigError");
        assert_eq!(config_err.plugin_name, "consul");
        assert!(config_err.message.contains("Failed to deserialize"));
    }

    #[test]
    fn test_consul_topology_config_deserialization_with_all_fields() {
        let config_str = r#"
            service_name = "urc-server"
            ignore_address = "127.0.0.1"
            poll_interval_secs = 30
        "#;

        let config: toml::Value = toml::from_str(config_str).expect("Failed to parse TOML");
        let plugin_config: ConsulTopologyPluginConfig =
            config.try_into().expect("Failed to deserialize config");

        assert_eq!(plugin_config.service_name, "urc-server");
        assert_eq!(plugin_config.ignore_address, Some("127.0.0.1".to_string()));
        assert_eq!(plugin_config.poll_interval_secs, Some(30));
    }

    #[test]
    fn test_register_adds_consul_topology_plugin() {
        let mut registry = PluginRegistry::new();
        register(&mut registry);

        let topology_plugins = registry.list_topology_plugins();
        assert!(
            topology_plugins.contains(&"consul".to_string()),
            "Expected 'consul' in topology plugins, found: {topology_plugins:?}"
        );
    }

    #[test]
    fn test_consul_topology_creation_success() {
        let factory = ConsulTopologyPluginFactory;

        let config_str = r#"
            address = "http://localhost:8500"
            service_name = "test-service"
        "#;

        let config: toml::Value = toml::from_str(config_str).expect("Failed to parse TOML");
        let result = factory.create(&config);

        assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
    }
}
