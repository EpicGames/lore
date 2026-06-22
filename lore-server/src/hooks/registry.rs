// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Hook registry for managing hook factories.
//!
//! The [`HookRegistry`] is the central hub for registering hook factories and
//! creating enabled hook instances based on configuration. It maintains:
//!
//! - A map of hook factories (populated via `register_all_hooks()`)
//! - Methods for creating enabled hook instances from configuration
//!
//! # Usage
//!
//! ```
//! use lore_server::hooks::{HookRegistry, Hook, HookFactory, HookError, HookContext, HookPoint};
//! use async_trait::async_trait;
//!
//! // Define a simple hook
//! struct LogHook;
//!
//! #[async_trait]
//! impl Hook for LogHook {
//!     fn name(&self) -> &'static str { "log" }
//!     fn hook_points(&self) -> &'static [HookPoint] { &[HookPoint::BranchPush] }
//! }
//!
//! // Define a factory for the hook
//! struct LogHookFactory;
//!
//! impl HookFactory for LogHookFactory {
//!     fn name(&self) -> &'static str { "log" }
//!     fn create(&self, _config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
//!         Ok(Box::new(LogHook))
//!     }
//! }
//!
//! // Create and use the registry
//! let mut registry = HookRegistry::new();
//!
//! // Register hook factory
//! registry.register_hook(Box::new(LogHookFactory));
//!
//! // List available hooks
//! let available = registry.list_hooks();
//! assert!(available.contains(&"log".to_string()));
//!
//! // Create a hook instance
//! let config = toml::Value::Table(toml::map::Map::new());
//! let hook = registry.create_hook("log", &config).unwrap();
//! assert_eq!(hook.name(), "log");
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use lore_revision::notification::NotificationSender;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::hooks::traits::Hook;
use crate::hooks::traits::HookError;
use crate::hooks::traits::HookFactory;

/// Context providing runtime dependencies for hook factory registration.
///
/// This struct is passed to each hook's `register()` function during startup,
/// allowing hook factories to capture the dependencies they need for creating
/// hook instances.
pub struct HookRegistrationContext {
    /// Notification sender for hooks that need to send notifications.
    pub notification_sender: Arc<dyn NotificationSender>,
}

/// Registry for hook factories.
///
/// The registry maintains a map from hook names to their factories.
/// Hook factories are registered at application startup, typically via
/// the auto-generated `register_all_hooks()` function.
#[derive(Default)]
pub struct HookRegistry {
    factories: HashMap<&'static str, Box<dyn HookFactory>>,
}

impl HookRegistry {
    /// Creates a new empty hook registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a hook factory.
    ///
    /// # Arguments
    ///
    /// * `factory` - The hook factory to register
    ///
    /// # Panics
    ///
    /// Panics if a hook with the same name is already registered.
    pub fn register_hook(&mut self, factory: Box<dyn HookFactory>) {
        let name = factory.name();
        if self.factories.contains_key(name) {
            panic!("Hook '{name}' is already registered");
        }
        info!(hook_name = name, "Registered hook factory");
        self.factories.insert(name, factory);
    }

    /// Returns a list of all registered hook names.
    pub fn list_hooks(&self) -> Vec<String> {
        self.factories.keys().map(|s| (*s).to_string()).collect()
    }

    /// Creates a hook instance by name.
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the hook to create
    /// * `config` - TOML configuration for the hook
    ///
    /// # Returns
    ///
    /// A boxed hook instance on success.
    ///
    /// # Errors
    ///
    /// - [`HookError::ConfigError`] - Hook not found (name not registered)
    /// - [`HookError::ConfigError`] - Configuration validation failed
    /// - [`HookError::InitError`] - Hook initialization failed
    pub fn create_hook(
        &self,
        name: &str,
        config: &toml::Value,
    ) -> Result<Box<dyn Hook>, HookError> {
        if let Some(factory) = self.factories.get(name) {
            factory.create(config).inspect_err(|e| {
                error!(
                    hook_name = name,
                    error = %e,
                    "Failed to create hook"
                );
            })
        } else {
            let available = self.list_hooks();
            error!(
                hook_name = name,
                error = "Hook not found",
                available_hooks = ?available,
                "Failed to create hook"
            );
            Err(HookError::ConfigError {
                hook_name: name.to_string(),
                message: format!("Hook '{name}' not found. Available hooks: {available:?}"),
            })
        }
    }

    /// Creates hook instances for all enabled hooks in the configuration.
    ///
    /// # Arguments
    ///
    /// * `hook_settings` - Map of hook name to (enabled flag, config)
    ///
    /// # Returns
    ///
    /// A vector of (name, hook) pairs for all successfully created enabled hooks.
    ///
    /// # Errors
    ///
    /// Returns an error if any enabled hook fails to create.
    ///
    /// # Example
    ///
    /// ```
    /// use lore_server::hooks::{HookRegistry, HookSettings, Hook, HookFactory, HookError, HookContext, HookPoint};
    /// use async_trait::async_trait;
    /// use std::collections::HashMap;
    ///
    /// // Define a simple hook
    /// struct TestHook;
    ///
    /// #[async_trait]
    /// impl Hook for TestHook {
    ///     fn name(&self) -> &'static str { "test" }
    ///     fn hook_points(&self) -> &'static [HookPoint] { &[HookPoint::BranchPush] }
    /// }
    ///
    /// struct TestHookFactory;
    ///
    /// impl HookFactory for TestHookFactory {
    ///     fn name(&self) -> &'static str { "test" }
    ///     fn create(&self, _config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
    ///         Ok(Box::new(TestHook))
    ///     }
    /// }
    ///
    /// // Set up registry
    /// let mut registry = HookRegistry::new();
    /// registry.register_hook(Box::new(TestHookFactory));
    ///
    /// // Configure hooks
    /// let mut settings = HashMap::new();
    /// settings.insert(
    ///     "test".to_string(),
    ///     HookSettings {
    ///         enabled: true,
    ///         config: toml::Value::Table(toml::map::Map::new()),
    ///     },
    /// );
    ///
    /// // Create enabled hooks
    /// let enabled_hooks = registry.create_enabled_hooks(&settings).unwrap();
    /// assert_eq!(enabled_hooks.len(), 1);
    /// assert_eq!(enabled_hooks[0].0, "test");
    /// ```
    #[allow(clippy::type_complexity)]
    pub fn create_enabled_hooks(
        &self,
        hook_settings: &HashMap<String, HookSettings>,
    ) -> Result<Vec<(String, Box<dyn Hook>)>, HookError> {
        let mut hooks = Vec::new();

        for (name, settings) in hook_settings {
            if !settings.enabled {
                debug!(hook_name = name, "Hook is disabled, skipping");
                continue;
            }

            let hook = self.create_hook(name, &settings.config)?;
            info!(
                hook_name = name,
                hook_points = ?hook.hook_points(),
                "Created enabled hook"
            );
            hooks.push((name.clone(), hook));
        }

        Ok(hooks)
    }

    /// Returns whether a hook with the given name is registered.
    pub fn has_hook(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("hooks", &self.list_hooks())
            .finish()
    }
}

/// Settings for a single hook from configuration.
///
/// This struct matches the expected TOML configuration format:
///
/// ```toml
/// [hooks.compliance]
/// enabled = true
/// # ... other config fields ...
/// ```
///
/// The `enabled` flag is extracted separately, and all other fields
/// are preserved in `config` for the hook factory to deserialize.
#[derive(Debug, Clone)]
pub struct HookSettings {
    /// Whether this hook is enabled.
    pub enabled: bool,

    /// The hook's configuration (everything except 'enabled').
    pub config: toml::Value,
}

impl Default for HookSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            config: toml::Value::Table(toml::map::Map::new()),
        }
    }
}

impl<'de> serde::Deserialize<'de> for HookSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut table = toml::Table::deserialize(deserializer)?;

        // Extract 'enabled' flag, defaulting to false
        let enabled = table
            .remove("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Keep remaining config
        let config = toml::Value::Table(table);

        Ok(Self { enabled, config })
    }
}

impl serde::Serialize for HookSettings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        // Get the underlying table or create empty one
        let config_table = match &self.config {
            toml::Value::Table(t) => t.clone(),
            _ => toml::map::Map::new(),
        };

        // Serialize enabled + all config fields
        let mut map = serializer.serialize_map(Some(config_table.len() + 1))?;
        map.serialize_entry("enabled", &self.enabled)?;
        for (k, v) in &config_table {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::hooks::traits::HookPoint;

    struct MockHook {
        name: &'static str,
        points: &'static [HookPoint],
    }

    #[async_trait]
    impl Hook for MockHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }
    }

    struct MockHookFactory {
        name: &'static str,
        points: &'static [HookPoint],
        should_fail_config: bool,
        should_fail_init: bool,
    }

    impl MockHookFactory {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                points: &[HookPoint::BranchPush],
                should_fail_config: false,
                should_fail_init: false,
            }
        }

        fn with_config_error(mut self) -> Self {
            self.should_fail_config = true;
            self
        }

        fn with_init_error(mut self) -> Self {
            self.should_fail_init = true;
            self
        }
    }

    impl HookFactory for MockHookFactory {
        fn name(&self) -> &'static str {
            self.name
        }

        fn create(&self, config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
            if self.should_fail_config {
                return Err(HookError::ConfigError {
                    hook_name: self.name.to_string(),
                    message: format!("Invalid config: {config:?}"),
                });
            }
            if self.should_fail_init {
                return Err(HookError::InitError {
                    hook_name: self.name.to_string(),
                    message: "Failed to initialize".to_string(),
                });
            }
            Ok(Box::new(MockHook {
                name: self.name,
                points: self.points,
            }))
        }
    }

    #[test]
    fn test_register_hook() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("test")));

        let hooks = registry.list_hooks();
        assert_eq!(hooks.len(), 1);
        assert!(hooks.contains(&"test".to_string()));
    }

    #[test]
    fn test_register_multiple_hooks() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("hook1")));
        registry.register_hook(Box::new(MockHookFactory::new("hook2")));

        let hooks = registry.list_hooks();
        assert_eq!(hooks.len(), 2);
        assert!(hooks.contains(&"hook1".to_string()));
        assert!(hooks.contains(&"hook2".to_string()));
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn test_register_duplicate_hook_panics() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("test")));
        registry.register_hook(Box::new(MockHookFactory::new("test")));
    }

    #[test]
    fn test_create_hook_success() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("test")));

        let config = toml::Value::Table(toml::map::Map::new());
        let result = registry.create_hook("test", &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_hook_not_found() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("other")));

        let config = toml::Value::Table(toml::map::Map::new());
        let result = registry.create_hook("missing", &config);

        match result {
            Err(HookError::ConfigError { hook_name, message }) => {
                assert_eq!(hook_name, "missing");
                assert!(message.contains("not found"));
                assert!(message.contains("other"));
            }
            _ => panic!("Expected ConfigError"),
        }
    }

    #[test]
    fn test_create_hook_config_error() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("test").with_config_error()));

        let config = toml::Value::Table(toml::map::Map::new());
        let result = registry.create_hook("test", &config);

        match result {
            Err(HookError::ConfigError { hook_name, .. }) => {
                assert_eq!(hook_name, "test");
            }
            _ => panic!("Expected ConfigError"),
        }
    }

    #[test]
    fn test_create_hook_init_error() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("test").with_init_error()));

        let config = toml::Value::Table(toml::map::Map::new());
        let result = registry.create_hook("test", &config);

        match result {
            Err(HookError::InitError { hook_name, .. }) => {
                assert_eq!(hook_name, "test");
            }
            _ => panic!("Expected InitError"),
        }
    }

    #[test]
    fn test_has_hook() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("test")));

        assert!(registry.has_hook("test"));
        assert!(!registry.has_hook("nonexistent"));
    }

    #[test]
    fn test_empty_registry() {
        let registry = HookRegistry::new();
        assert!(registry.list_hooks().is_empty());
        assert!(!registry.has_hook("anything"));
    }

    #[test]
    fn test_registry_debug() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("hook1")));
        registry.register_hook(Box::new(MockHookFactory::new("hook2")));

        let debug_str = format!("{registry:?}");
        assert!(debug_str.contains("hook1"));
        assert!(debug_str.contains("hook2"));
    }

    #[test]
    fn test_create_enabled_hooks_empty() {
        let registry = HookRegistry::new();
        let settings: HashMap<String, HookSettings> = HashMap::new();

        let result = registry.create_enabled_hooks(&settings);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_create_enabled_hooks_all_disabled() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("hook1")));
        registry.register_hook(Box::new(MockHookFactory::new("hook2")));

        let mut settings = HashMap::new();
        settings.insert(
            "hook1".to_string(),
            HookSettings {
                enabled: false,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );
        settings.insert(
            "hook2".to_string(),
            HookSettings {
                enabled: false,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );

        let result = registry.create_enabled_hooks(&settings).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_create_enabled_hooks_some_enabled() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("hook1")));
        registry.register_hook(Box::new(MockHookFactory::new("hook2")));

        let mut settings = HashMap::new();
        settings.insert(
            "hook1".to_string(),
            HookSettings {
                enabled: true,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );
        settings.insert(
            "hook2".to_string(),
            HookSettings {
                enabled: false,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );

        let result = registry.create_enabled_hooks(&settings).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "hook1");
    }

    #[test]
    fn test_create_enabled_hooks_all_enabled() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("hook1")));
        registry.register_hook(Box::new(MockHookFactory::new("hook2")));

        let mut settings = HashMap::new();
        settings.insert(
            "hook1".to_string(),
            HookSettings {
                enabled: true,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );
        settings.insert(
            "hook2".to_string(),
            HookSettings {
                enabled: true,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );

        let result = registry.create_enabled_hooks(&settings).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_create_enabled_hooks_error_propagates() {
        let mut registry = HookRegistry::new();
        registry.register_hook(Box::new(MockHookFactory::new("good")));
        registry.register_hook(Box::new(MockHookFactory::new("bad").with_init_error()));

        let mut settings = HashMap::new();
        settings.insert(
            "good".to_string(),
            HookSettings {
                enabled: true,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );
        settings.insert(
            "bad".to_string(),
            HookSettings {
                enabled: true,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );

        let result = registry.create_enabled_hooks(&settings);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_enabled_hooks_missing_factory() {
        let registry = HookRegistry::new();

        let mut settings = HashMap::new();
        settings.insert(
            "nonexistent".to_string(),
            HookSettings {
                enabled: true,
                config: toml::Value::Table(toml::map::Map::new()),
            },
        );

        let result = registry.create_enabled_hooks(&settings);
        assert!(result.is_err());
    }

    #[test]
    fn test_hook_settings_default() {
        let settings = HookSettings::default();
        assert!(!settings.enabled);
        assert!(matches!(settings.config, toml::Value::Table(_)));
    }

    #[test]
    fn test_hook_settings_deserialize() {
        let toml_str = r#"
            enabled = true
            some_config = "value"
            number = 42
        "#;

        let settings: HookSettings = toml::from_str(toml_str).unwrap();

        assert!(settings.enabled);

        // Config should have the remaining fields
        let table = settings.config.as_table().unwrap();
        assert_eq!(table.get("some_config").unwrap().as_str(), Some("value"));
        assert_eq!(table.get("number").unwrap().as_integer(), Some(42));
        assert!(!table.contains_key("enabled"));
    }

    #[test]
    fn test_hook_settings_deserialize_default_enabled() {
        let toml_str = r#"
            some_config = "value"
        "#;

        let settings: HookSettings = toml::from_str(toml_str).unwrap();

        assert!(!settings.enabled); // Default to false
    }

    #[test]
    fn test_hook_settings_deserialize_empty() {
        let toml_str = "";

        let settings: HookSettings = toml::from_str(toml_str).unwrap();

        assert!(!settings.enabled);
        assert!(settings.config.as_table().unwrap().is_empty());
    }

    #[test]
    fn test_hook_settings_serialize() {
        let settings = HookSettings {
            enabled: true,
            config: toml::Value::Table({
                let mut t = toml::map::Map::new();
                t.insert("key".to_string(), toml::Value::String("value".to_string()));
                t
            }),
        };

        let serialized = toml::to_string(&settings).unwrap();
        assert!(serialized.contains("enabled = true"));
        assert!(serialized.contains("key = \"value\""));
    }

    #[test]
    fn test_hook_settings_roundtrip() {
        let original = HookSettings {
            enabled: true,
            config: toml::Value::Table({
                let mut t = toml::map::Map::new();
                t.insert("key".to_string(), toml::Value::String("value".to_string()));
                t.insert("number".to_string(), toml::Value::Integer(42));
                t
            }),
        };

        let serialized = toml::to_string(&original).unwrap();
        let deserialized: HookSettings = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.enabled, original.enabled);
        assert_eq!(
            deserialized.config.as_table().unwrap().get("key"),
            original.config.as_table().unwrap().get("key")
        );
        assert_eq!(
            deserialized.config.as_table().unwrap().get("number"),
            original.config.as_table().unwrap().get("number")
        );
    }
}
