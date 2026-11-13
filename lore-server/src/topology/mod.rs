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

#[cfg(not(feature = "test-util"))]
mod composite;
#[cfg(feature = "test-util")]
pub mod composite;
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
            TopologyProvider::None
            | TopologyProvider::Fixed
            | TopologyProvider::RotatingIdFixed
            | TopologyProvider::Composite => None, // Built-in, no plugin needed
            TopologyProvider::Consul => Some("consul"),
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

    /// This server's own address, left out of the peer list. A peer is left out when its
    /// `address` is exactly equal to this value.
    #[serde(default)]
    pub self_address: Option<String>,
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
