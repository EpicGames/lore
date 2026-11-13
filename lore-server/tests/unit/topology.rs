// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod composite;
mod fixed;
mod rotating_id_fixed;

use std::collections::HashMap;

use async_trait::async_trait;
use lore_revision::cluster::peer::Locality;
use lore_revision::cluster::topology::RefreshLoopError;
use lore_server::plugins::PluginError;
use lore_server::plugins::PluginRegistry;
use lore_server::plugins::traits::TopologyPluginFactory;
use lore_server::topology::*;
use tokio::sync::broadcast;

#[test]
fn test_topology_provider_plugin_name() {
    assert_eq!(TopologyProvider::None.plugin_name(), None);
    assert_eq!(TopologyProvider::Consul.plugin_name(), Some("consul"));
    // Fixed is built-in, returns None for plugin_name
    assert_eq!(TopologyProvider::Fixed.plugin_name(), None);
}

#[test]
fn test_configure_topology_with_registry_none_provider() {
    let registry = PluginRegistry::new();
    let settings = TopologySettings {
        provider: TopologyProvider::None,
        fixed: None,
        rotating_id_fixed: None,
        composite: None,
    };

    let result = configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
    assert!(result.is_ok());
    assert!(result.expect("should succeed").is_none());
}

#[test]
fn test_configure_topology_with_registry_no_settings() {
    let registry = PluginRegistry::new();

    let result = configure_topology_with_registry(&registry, None, &HashMap::default());
    assert!(result.is_ok());
    assert!(result.expect("should succeed").is_none());
}

#[test]
fn test_configure_topology_fixed_builtin_with_inline_config() {
    // Fixed topology is now built-in, so it works without registering a plugin
    let registry = PluginRegistry::new();

    let settings = TopologySettings {
        provider: TopologyProvider::Fixed,
        fixed: Some(FixedTopologySettings {
            peers: vec![PeerSettings {
                address: "192.168.1.10".to_string(),
                port: 9090,
                locality: Locality::SameRegion,
            }],
            self_address: None,
        }),
        rotating_id_fixed: None,
        composite: None,
    };

    // No plugin config - should use inline config
    let result = configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
    assert!(result.is_ok());
    assert!(result.expect("should succeed").is_some());
}

#[test]
fn test_configure_topology_fixed_no_config_fails() {
    let registry = PluginRegistry::new();

    let settings = TopologySettings {
        provider: TopologyProvider::Fixed,
        fixed: None,
        rotating_id_fixed: None,
        composite: None,
    };

    // No config at all should fail
    let result = configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
    assert!(result.is_err());

    let err = result.expect_err("should fail");
    let config_err = err
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "fixed");
    assert!(config_err.message.contains("No configuration found"));
}

#[test]
fn test_configure_topology_rotating_fixed_builtin_with_inline_config() {
    let registry = PluginRegistry::new();

    let settings = TopologySettings {
        provider: TopologyProvider::RotatingIdFixed,
        rotating_id_fixed: Some(RotatingIdFixedTopologySettings {
            peers: vec![PeerSettings {
                address: "192.168.1.10".to_string(),
                port: 9090,
                locality: Locality::SameRegion,
            }],
            rotation_interval_seconds: 1,
        }),
        fixed: None,
        composite: None,
    };

    let result = configure_topology_with_registry(&registry, Some(&settings), &HashMap::default())
        .expect("should succeed");
    assert!(result.is_some());
}

#[test]
fn test_configure_topology_rotating_fixed_no_config_fails() {
    let registry = PluginRegistry::new();

    let settings = TopologySettings {
        provider: TopologyProvider::RotatingIdFixed,
        fixed: None,
        rotating_id_fixed: None,
        composite: None,
    };

    // No config at all should fail
    let result = configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
    assert!(result.is_err());

    let err = result.expect_err("should fail");
    let config_err = err
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "rotating_id_fixed");
    assert!(config_err.message.contains("No configuration found"));
}

#[test]
fn test_configure_topology_consul_missing_plugin() {
    let registry = PluginRegistry::new(); // Empty registry

    let settings = TopologySettings {
        provider: TopologyProvider::Consul,
        fixed: None,
        rotating_id_fixed: None,
        composite: None,
    };

    let config: toml::Value =
        toml::from_str("address = 'http://localhost:8500'").expect("valid toml");
    let mut configs = HashMap::new();
    configs.insert("consul".to_string(), config);

    let result = configure_topology_with_registry(&registry, Some(&settings), &configs);
    assert!(result.is_err());

    let err = result.expect_err("should fail");
    let not_found = err.as_plugin_not_found().expect("should be PluginNotFound");
    assert_eq!(not_found.plugin_name, "consul");
}

#[test]
fn test_configure_topology_consul_missing_config() {
    let registry = PluginRegistry::new();

    let settings = TopologySettings {
        provider: TopologyProvider::Consul,
        fixed: None,
        rotating_id_fixed: None,
        composite: None,
    };

    // No plugin config should fail with appropriate error
    let result = configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
    assert!(result.is_err());

    let err = result.expect_err("should fail");
    let config_err = err
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "consul");
    assert!(config_err.message.contains("[plugins.consul]"));
}

#[test]
fn test_topology_settings_deserialization() {
    let config_str = r#"
            provider = "fixed"
        "#;

    let settings: TopologySettings = toml::from_str(config_str).expect("valid toml");
    assert_eq!(settings.provider, TopologyProvider::Fixed);
    assert!(settings.fixed.is_none());
}

#[test]
fn test_topology_settings_deserialization_with_fixed_config() {
    let config_str = r#"
            provider = "fixed"

            [fixed]
            peers = [
                { address = "192.168.1.10", port = 9090, locality = "SameRegion" },
                { address = "192.168.1.11", port = 9091, locality = "OtherRegion" },
            ]
        "#;

    let settings: TopologySettings = toml::from_str(config_str).expect("valid toml");
    assert_eq!(settings.provider, TopologyProvider::Fixed);
    assert!(settings.fixed.is_some());

    let fixed = settings.fixed.as_ref().expect("should have fixed");
    assert_eq!(fixed.peers.len(), 2);
    assert_eq!(fixed.peers[0].address, "192.168.1.10");
    assert_eq!(fixed.peers[0].port, 9090);
}

#[test]
fn test_topology_settings_deserialization_with_fixed_self_address() {
    let config_str = r#"
            provider = "fixed"

            [fixed]
            peers = [{ address = "lore-0.lore", port = 41340, locality = "SameRegion" }]
            self_address = "lore-0.lore"
        "#;

    let settings: TopologySettings = toml::from_str(config_str).expect("valid toml");
    let fixed = settings.fixed.as_ref().expect("should have fixed");
    assert_eq!(fixed.self_address.as_deref(), Some("lore-0.lore"));
}

#[test]
fn test_topology_settings_deserialization_fixed_self_address_defaults_to_none() {
    let config_str = r#"
            provider = "fixed"

            [fixed]
            peers = []
        "#;

    let settings: TopologySettings = toml::from_str(config_str).expect("valid toml");
    let fixed = settings.fixed.as_ref().expect("should have fixed");
    assert!(fixed.self_address.is_none());
}
