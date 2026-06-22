// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Topology configuration and provider integration.
//!
//! This module provides topology configuration with both built-in providers
//! and plugin-based providers. Built-in providers (like fixed topology) are
//! handled directly, while dynamic providers (like Consul) use the plugin system.
//!
//! # Configuration
//!
//! Topology is configured in two parts:
//!
//! 1. **Provider Selection** - The `[topology]` section specifies which provider to use:
//!    ```toml
//!    [topology]
//!    provider = "consul"  # or e.g. "fixed"
//!    ```
//!
//! 2. **Provider Configuration** - Depending on the provider type:
//!    - **A Built-in provider** (e.g. "fixed" uses `[topology.fixed])`
//!    - **A Plugin based provider** (e.g. "consul" uses `[plugins.consul])`
//!

mod composite;
pub mod fixed;
pub mod rotating_id_fixed;

use std::collections::HashMap;
use std::sync::Arc;

use lore_base::error::PluginConfigError;
use lore_base::error::PluginInitError;
use lore_base::error::PluginNotFound;
use lore_error_set::prelude::*;
use lore_revision::cluster::peer::Locality;
use lore_revision::cluster::topology::Topology;
use serde::Deserialize;
use tracing::info;
use tracing::info_span;
use tracing::warn;

use crate::plugins::PluginRegistry;
use crate::topology::composite::CompositeTopology;
use crate::topology::fixed::FixedTopology;
use crate::topology::rotating_id_fixed::RotatingIdFixedTopology;

/// Topology provider selection.
///
/// This enum specifies which topology provider to use. Some providers are
/// built-in (handled directly), while others require plugins.
#[derive(Clone, Default, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TopologyProvider {
    /// No topology configured - single node mode.
    #[default]
    None,
    /// Consul-based service discovery (requires plugin).
    Consul,
    /// Fixed/static peer list (built-in).
    Fixed,
    /// Fixed/static peer list with a periodically rotating ID
    RotatingIdFixed,
    /// A Topology formed from 1 or more Topology sources
    Composite,
}

impl TopologyProvider {
    /// Returns the plugin name for this provider, if it requires a plugin.
    ///
    /// Built-in providers return `None`, plugin-based providers return the plugin name.
    pub fn plugin_name(&self) -> Option<&'static str> {
        match self {
            Self::None | Self::Fixed | Self::RotatingIdFixed | Self::Composite => None, // Built-in, no plugin needed
            Self::Consul => Some("consul"),
        }
    }
}

/// Topology configuration settings.
///
/// This struct contains the provider selection and optional provider-specific
/// configuration for built-in providers.
#[derive(Clone, Debug, Deserialize)]
pub struct TopologySettings {
    /// The topology provider to use.
    ///
    /// See [`TopologyProvider`] for available options.
    pub provider: TopologyProvider,

    /// Fixed topology configuration (built-in).
    #[serde(default)]
    pub fixed: Option<FixedTopologySettings>,

    /// Rotating Id Fixed topology configuration (built-in).
    #[serde(default)]
    pub rotating_id_fixed: Option<RotatingIdFixedTopologySettings>,

    /// Composite topology configuration (built-in)
    #[serde(default)]
    pub composite: Option<CompositeTopologySettings>,
}

/// Fixed topology settings.
#[derive(Clone, Debug, Deserialize)]
pub struct FixedTopologySettings {
    /// List of peer configurations.
    #[serde(default)]
    pub peers: Vec<PeerSettings>,
}

/// Rotating ID Fixed topology settings.
#[derive(Clone, Debug, Deserialize)]
pub struct RotatingIdFixedTopologySettings {
    /// List of peer configurations.
    #[serde(default)]
    pub peers: Vec<PeerSettings>,

    /// How often peer IDs are rotated
    pub rotation_interval_seconds: u64,
}

/// Composite topology settings.
#[derive(Clone, Debug, Deserialize)]
pub struct CompositeTopologySettings {
    /// The sources to make up this topology
    #[serde(default)]
    pub sources: Vec<TopologySettings>,
}

/// Peer settings for fixed topology.
#[derive(Clone, Debug, Deserialize)]
pub struct PeerSettings {
    /// Peer address.
    pub address: String,
    /// Peer port.
    pub port: u16,
    /// From a Topology perspective, where is this Peer relative to this Lore Server
    pub locality: Locality,
}

/// Errors that can occur during topology configuration.
///
/// Shares the plugin variants with [`crate::plugins::PluginError`] so
/// `.forward()` from the registry preserves the actionable signal.
/// Errors for built-in providers (fixed, rotating_id_fixed, composite) are
/// surfaced via `PluginConfigError` with the provider name — operators fix
/// the corresponding config section either way.
#[error_set]
pub enum ConfigureTopologyError {
    PluginNotFound,
    PluginConfigError,
    PluginInitError,
}

/// Constructs a "configuration error for <provider>" error for built-in providers.
fn topology_config_error(
    provider: &str,
    message: impl std::fmt::Display,
) -> ConfigureTopologyError {
    PluginConfigError {
        plugin_name: provider.to_string(),
        message: message.to_string(),
    }
    .into()
}

/// Configures topology using the plugin registry.
///
/// This function creates a topology instance based on the provider specified in settings.
/// Built-in providers (fixed) are handled directly, while plugin providers (consul)
/// use the plugin registry.
///
/// # Arguments
///
/// * `registry` - The plugin registry containing registered topology factories
/// * `settings` - The topology settings from the configuration file
/// * `plugin_config` - Plugin configuration from `[plugins.{provider}]`
///
/// # Returns
///
/// Returns `Ok(Some(topology))` if topology was configured, `Ok(None)` if provider is `none`,
/// or an error if configuration failed.
///
/// # Example
///
/// ```
/// use std::collections::HashMap;
/// use lore_server::plugins::PluginRegistry;
/// use lore_server::topology::{TopologySettings, TopologyProvider, configure_topology_with_registry};
///
/// let mut registry = PluginRegistry::new();
/// lore_server::plugins::register_all_plugins(&mut registry);
///
/// // Configure with no topology (single-node mode)
/// let settings = TopologySettings {
///     provider: TopologyProvider::None,
///     fixed: None,
///     rotating_id_fixed: None,
///     composite: None,
/// };
///
/// let topology = configure_topology_with_registry(
///     &registry,
///     Some(&settings),
///     &HashMap::default(), // No plugin-specific config needed for "none" provider
/// ).expect("Should not fail for None provider");
///
/// assert!(topology.is_none()); // Single-node mode returns None
/// ```
pub fn configure_topology_with_registry(
    registry: &PluginRegistry,
    settings: Option<&TopologySettings>,
    plugin_configs: &HashMap<String, toml::Value>,
) -> Result<Option<Arc<dyn Topology + Send + Sync>>, ConfigureTopologyError> {
    let Some(settings) = settings else {
        info!("No topology settings configured, running in single-node mode");
        return Ok(None);
    };

    match &settings.provider {
        TopologyProvider::None => {
            info!("Topology provider set to 'none', running in single-node mode");
            Ok(None)
        }
        TopologyProvider::Fixed => {
            // Fixed topology is built-in - handle directly, not via plugin
            configure_fixed_topology(settings)
        }
        TopologyProvider::RotatingIdFixed => configure_rotating_id_fixed_topology(settings),
        TopologyProvider::Consul => {
            // Consul uses the plugin system exclusively
            let plugin_name = settings.provider.plugin_name().unwrap_or_default();
            let plugin_config = plugin_configs.get(plugin_name);

            let Some(config) = plugin_config else {
                return Err(PluginConfigError {
                    plugin_name: plugin_name.to_string(),
                    message: format!(
                        "No configuration found for topology provider '{plugin_name}'. \
                        Add a [plugins.{plugin_name}] section to your configuration.",
                    ),
                }
                .into());
            };

            info!(
                plugin_name = plugin_name,
                "Using topology plugin with configuration from [plugins.{}]", plugin_name
            );

            let topology = registry
                .create_topology(plugin_name, config)
                .forward::<ConfigureTopologyError>("creating topology plugin")?;
            Ok(Some(topology))
        }
        TopologyProvider::Composite => {
            let composite = configure_composite_topology(registry, settings, plugin_configs)?;
            Ok(Some(composite))
        }
    }
}

fn configure_composite_topology(
    registry: &PluginRegistry,
    settings: &TopologySettings,
    plugin_configs: &HashMap<String, toml::Value>,
) -> Result<Arc<CompositeTopology>, ConfigureTopologyError> {
    if let Some(settings) = &settings.composite {
        let root_span = info_span!("composite_topology_settings");
        let _root_guard = root_span.enter();

        info!(
            source_num = settings.sources.len(),
            "Creating Composite Topology sources"
        );

        let mut sources = Vec::with_capacity(settings.sources.len());
        for source_settings in settings.sources.iter() {
            let source_provider = source_settings.provider.plugin_name().unwrap_or_default();
            let source_span = info_span!("source", provider = source_provider);
            let _source_guard = source_span.enter();

            let source =
                configure_topology_with_registry(registry, Some(source_settings), plugin_configs)?;
            if let Some(source) = source {
                sources.push(source);
            } else {
                warn!("source did not generate a topology");
            }
        }
        return Ok(CompositeTopology::from_sources(sources));
    }

    Err(topology_config_error(
        "composite",
        "No configuration found for composite topology",
    ))
}

/// Configures fixed topology (built-in).
///
/// Fixed topology is handled directly without going through the plugin system.
fn configure_fixed_topology(
    settings: &TopologySettings,
) -> Result<Option<Arc<dyn Topology + Send + Sync>>, ConfigureTopologyError> {
    // Try predefined format ([topology.fixed])
    if let Some(fixed_settings) = &settings.fixed {
        info!("Creating fixed topology from [topology.fixed] configuration");

        let topology = FixedTopology::from_settings(fixed_settings);
        return Ok(Some(topology));
    }

    // No configuration found
    Err(topology_config_error(
        "fixed",
        "No configuration found for fixed topology. Add [topology.fixed]",
    ))
}

fn configure_rotating_id_fixed_topology(
    settings: &TopologySettings,
) -> Result<Option<Arc<dyn Topology + Send + Sync>>, ConfigureTopologyError> {
    if let Some(fixed_settings) = &settings.rotating_id_fixed {
        info!(
            "Creating Rotating Id Fixed topology from [topology.rotating_id_fixed] configuration"
        );

        let topology = RotatingIdFixedTopology::from_settings(fixed_settings);
        return Ok(Some(topology));
    }

    Err(topology_config_error(
        "rotating_id_fixed",
        "No configuration found for Rotating Id Fixed topology",
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::cluster::peer::PeerInfo;
    use lore_revision::cluster::topology::RefreshLoopError;
    use lore_revision::cluster::topology::Topology;
    use tokio::sync::broadcast;

    use super::*;
    use crate::plugins::PluginError;
    use crate::plugins::PluginRegistry;
    use crate::plugins::traits::TopologyPluginFactory;
    use crate::util::setup_test_execution;

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

        let result =
            configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
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
            }),
            rotating_id_fixed: None,
            composite: None,
        };

        // No plugin config - should use inline config
        let result =
            configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
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
        let result =
            configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
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

        let result =
            configure_topology_with_registry(&registry, Some(&settings), &HashMap::default())
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
        let result =
            configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
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
        let result =
            configure_topology_with_registry(&registry, Some(&settings), &HashMap::default());
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

    mod composite {
        use super::*;

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

            fn subscribe_to_peer_refreshes(
                self: Arc<Self>,
            ) -> broadcast::Receiver<HashSet<PeerInfo>> {
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

                    let result = configure_topology_with_registry(
                        &registry,
                        Some(&settings),
                        &plugin_configs,
                    )
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
    }
}
