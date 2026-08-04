// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! AWS store plugin factories.
//!
//! This module provides plugin factories for AWS-backed stores:
//! - [`AwsImmutableStorePluginFactory`] - Creates S3/`DynamoDB`-backed immutable stores
//! - [`AwsMutableStorePluginFactory`] - Creates `DynamoDB`-backed mutable stores
//! - [`AwsLockStorePluginFactory`] - Creates `DynamoDB`-backed lock stores

use std::sync::Arc;
use std::time::Duration;

use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::HttpClientSettings;
use lore_aws::clients::TimeoutConfig;
use lore_aws::store::immutable_store::AwsImmutableStore;
use lore_aws::store::immutable_store::AwsImmutableStoreSettings;
use lore_aws::store::immutable_store::DynamoDbImmutableStoreSettings;
use lore_aws::store::immutable_store::S3StoreSettings;
use lore_aws::store::lock_store::DynamoDbLockStore;
use lore_aws::store::mutable_store::AwsMutableStore;
use lore_aws::store::mutable_store::AwsMutableStoreSettings;
use lore_aws::store::mutable_store::DynamoDbMutableStoreSettings;
use lore_aws::telemetry::AWSResourceDetector;
use lore_base::error::PluginConfigError;
use lore_base::error::PluginInitError;
use lore_base::runtime::runtime;
use lore_revision::lock::LockStore;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use opentelemetry_sdk::resource::ResourceDetector;
use serde::Deserialize;
use tracing::info;

use crate::plugins::ImmutableStorePluginFactory;
use crate::plugins::LockStorePluginFactory;
use crate::plugins::MutableStorePluginFactory;
use crate::plugins::PluginError;
use crate::plugins::PluginRegistry;

const PLUGIN_NAME: &str = "aws";

// =============================================================================
// Configuration Structs
// =============================================================================

/// Configuration for the AWS immutable store plugin.
///
/// This configuration is deserialized from TOML and contains all settings
/// needed to create an [`AwsImmutableStore`].
#[derive(Debug, Clone, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct AwsImmutableStorePluginConfig {
    /// HTTP client settings for AWS operations.
    #[serde(default)]
    pub http: HttpClientSettings,

    /// S3 bucket name for storing fragment payloads.
    pub s3_bucket: String,

    /// Optional S3 endpoint URL (for `LocalStack` or other S3-compatible services).
    #[serde(default)]
    pub s3_endpoint_url: Option<String>,

    /// Optional S3 region.
    #[serde(default)]
    pub s3_region: Option<String>,

    /// `DynamoDB` table name for storing fragment associations.
    pub dynamodb_fragments_table: String,

    /// `DynamoDB` table name for storing fragment state, where a row's presence means the hash
    /// exists.
    ///
    /// Required, and deliberately without an alias: this table is its own, distinct from the one
    /// that held fragment metadata, and which table serves it is a decision rather than something
    /// to inherit.
    pub dynamodb_fragment_state_table: String,

    /// Optional `DynamoDB` table to read fragments from for objects written before they moved onto
    /// the S3 object.
    ///
    /// Accepts the older `dynamodb_metadata_table` spelling, which is what a configuration written
    /// before that change already points at — that table holds fragment metadata, and reading it is
    /// exactly what it is still needed for.
    ///
    /// Leaving it unset declares that no object predating the change exists, so an object carrying
    /// no metadata of its own is reported as damaged rather than described from a row that cannot
    /// be about it. Set it only where such objects may still exist; once a backfill has given them
    /// all their own metadata, removing it retires the fallback read.
    #[serde(default, alias = "dynamodb_metadata_table")]
    pub dynamodb_fragment_metadata_table: Option<String>,

    /// Optional `DynamoDB` endpoint URL (for `LocalStack` or other `DynamoDB`-compatible services).
    #[serde(default)]
    pub dynamodb_endpoint_url: Option<String>,

    /// Optional `DynamoDB` region.
    #[serde(default)]
    pub dynamodb_region: Option<String>,

    /// Slow operation threshold in milliseconds for S3 operations.
    #[serde(default = "default_slow_threshold")]
    pub s3_slow_operation_threshold_millis: u64,

    /// Slow operation threshold in milliseconds for `DynamoDB` operations.
    #[serde(default = "default_slow_threshold")]
    pub dynamodb_slow_operation_threshold_millis: u64,

    /// Timeout in milliseconds for AWS operations.
    #[serde(default = "default_timeout")]
    pub timeout_millis: u64,

    /// Force write mode (bypasses some safety checks).
    #[serde(default)]
    pub force_write: bool,

    /// Force path-style S3 addressing (required for S3-compatible stores behind
    /// non-AWS hostnames like `MinIO` in Docker).
    #[serde(default)]
    pub s3_force_path_style: bool,
}

/// Configuration for the AWS mutable store plugin.
///
/// This configuration is deserialized from TOML and contains all settings
/// needed to create an [`AwsMutableStore`].
#[derive(Debug, Clone, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct AwsMutableStorePluginConfig {
    /// HTTP client settings for AWS operations.
    #[serde(default)]
    pub http: HttpClientSettings,

    /// `DynamoDB` table name for storing mutable data.
    pub dynamodb_table: String,

    /// Optional `DynamoDB` endpoint URL (for `LocalStack` or other `DynamoDB`-compatible services).
    #[serde(default)]
    pub dynamodb_endpoint_url: Option<String>,

    /// Optional `DynamoDB` region.
    #[serde(default)]
    pub dynamodb_region: Option<String>,

    /// Slow operation threshold in milliseconds for `DynamoDB` operations.
    #[serde(default = "default_slow_threshold")]
    pub dynamodb_slow_operation_threshold_millis: u64,

    /// Timeout in milliseconds for AWS operations.
    #[serde(default = "default_timeout")]
    pub timeout_millis: u64,

    /// Force write mode (bypasses some safety checks).
    #[serde(default)]
    pub force_write: bool,
}

/// Configuration for the AWS lock store plugin.
///
/// This configuration is deserialized from TOML and contains all settings
/// needed to create a [`DynamoDbLockStore`].
#[derive(Debug, Clone, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct AwsLockStorePluginConfig {
    /// HTTP client settings for AWS operations.
    #[serde(default)]
    pub http: HttpClientSettings,

    /// `DynamoDB` table name for storing locks.
    pub dynamodb_table: String,

    /// Optional `DynamoDB` endpoint URL (for `LocalStack` or other `DynamoDB`-compatible services).
    #[serde(default)]
    pub dynamodb_endpoint_url: Option<String>,

    /// Optional `DynamoDB` region.
    #[serde(default)]
    pub dynamodb_region: Option<String>,

    /// Slow operation threshold in milliseconds for `DynamoDB` operations.
    #[serde(default = "default_slow_threshold")]
    pub dynamodb_slow_operation_threshold_millis: u64,

    /// Timeout in milliseconds for AWS operations.
    #[serde(default = "default_timeout")]
    pub timeout_millis: u64,
}

fn default_slow_threshold() -> u64 {
    u64::MAX
}

fn default_timeout() -> u64 {
    5000
}

// =============================================================================
// Plugin Factory Implementations
// =============================================================================

/// Plugin factory for creating AWS immutable stores.
///
/// This factory creates [`AwsImmutableStore`] instances backed by S3 (for payloads)
/// and `DynamoDB` (for fragment associations and metadata).
pub struct AwsImmutableStorePluginFactory;

impl ImmutableStorePluginFactory for AwsImmutableStorePluginFactory {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        let plugin_name = self.name();

        // Deserialize and validate configuration without creating AWS clients
        let _plugin_config: AwsImmutableStorePluginConfig =
            config.clone().try_into().map_err(|e| {
                PluginError::from(PluginConfigError {
                    plugin_name: plugin_name.to_string(),
                    message: format!("Failed to deserialize AWS immutable store config: {e}"),
                })
            })?;

        Ok(())
    }

    fn create(&self, config: &toml::Value) -> Result<Arc<dyn ImmutableStore>, PluginError> {
        let plugin_name = self.name();

        // Deserialize configuration
        let plugin_config: AwsImmutableStorePluginConfig =
            config.clone().try_into().map_err(|e| {
                PluginError::from(PluginConfigError {
                    plugin_name: plugin_name.to_string(),
                    message: format!("Failed to deserialize AWS immutable store config: {e}"),
                })
            })?;

        info!(
            plugin_name = plugin_name,
            s3_bucket = %plugin_config.s3_bucket,
            fragments_table = %plugin_config.dynamodb_fragments_table,
            fragment_state_table = %plugin_config.dynamodb_fragment_state_table,
            "Creating AWS immutable store: {plugin_config:?}"
        );

        let (s3_client, dynamodb_client) = tokio::task::block_in_place(|| {
            runtime().block_on(Box::pin(async {
                // Build S3 client
                let s3_client = Box::pin(
                    AwsClientBuilder::builder()
                        .with_http_settings(&plugin_config.http)
                        .maybe_endpoint(plugin_config.s3_endpoint_url.clone())
                        .maybe_region(plugin_config.s3_region.clone())
                        .with_timeout_config(
                            TimeoutConfig::builder()
                                .operation_timeout(Duration::from_millis(
                                    plugin_config.timeout_millis,
                                ))
                                .build(),
                        )
                        .build_config(),
                )
                .await
                .with_slow_operation_threshold(plugin_config.s3_slow_operation_threshold_millis)
                .s3_with_path_style(plugin_config.s3_force_path_style)
                .ensure_bucket(&plugin_config.s3_bucket)
                .build()
                .await
                .map_err(|e| {
                    PluginError::from(PluginInitError {
                        plugin_name: plugin_name.to_string(),
                        message: format!("Failed to create S3 client: {e}"),
                    })
                })?;

                // Build DynamoDB client
                let dynamodb_client_builder = Box::pin(
                    AwsClientBuilder::builder()
                        .with_http_settings(&plugin_config.http)
                        .maybe_endpoint(plugin_config.dynamodb_endpoint_url.clone())
                        .maybe_region(plugin_config.dynamodb_region.clone())
                        .with_timeout_config(
                            TimeoutConfig::builder()
                                .operation_timeout(Duration::from_millis(
                                    plugin_config.timeout_millis,
                                ))
                                .build(),
                        )
                        .build_config(),
                )
                .await
                .with_slow_operation_threshold(
                    plugin_config.dynamodb_slow_operation_threshold_millis,
                )
                .dynamodb()
                .ensure_table(&plugin_config.dynamodb_fragments_table)
                .ensure_table(&plugin_config.dynamodb_fragment_state_table);

                let dynamodb_client =
                    Box::pin(dynamodb_client_builder.build())
                        .await
                        .map_err(|e| {
                            PluginError::from(PluginInitError {
                                plugin_name: plugin_name.to_string(),
                                message: format!("Failed to create DynamoDB client: {e}"),
                            })
                        })?;

                Ok::<_, PluginError>((s3_client, dynamodb_client))
            }))
        })?;

        // Create settings
        let s3_settings = S3StoreSettings {
            bucket: plugin_config.s3_bucket,
            endpoint_url: plugin_config.s3_endpoint_url,
            region: plugin_config.s3_region,
            slow_operation_threshold_millis: plugin_config.s3_slow_operation_threshold_millis,
            timeout_millis: plugin_config.timeout_millis,
        };

        let dynamodb_settings = DynamoDbImmutableStoreSettings {
            fragments_table_name: plugin_config.dynamodb_fragments_table,
            fragment_state_table_name: plugin_config.dynamodb_fragment_state_table,
            fragment_metadata_table_name: plugin_config.dynamodb_fragment_metadata_table,
            endpoint_url: plugin_config.dynamodb_endpoint_url,
            region: plugin_config.dynamodb_region,
            slow_operation_threshold_millis: plugin_config.dynamodb_slow_operation_threshold_millis,
            timeout_millis: plugin_config.timeout_millis,
        };

        let store_settings = AwsImmutableStoreSettings::new(
            s3_settings,
            dynamodb_settings,
            plugin_config.force_write,
        );

        let store = AwsImmutableStore::new(s3_client, dynamodb_client, &store_settings);

        Ok(Arc::new(store))
    }
}

/// Plugin factory for creating AWS mutable stores.
///
/// This factory creates [`AwsMutableStore`] instances backed by `DynamoDB`.
pub struct AwsMutableStorePluginFactory;

impl MutableStorePluginFactory for AwsMutableStorePluginFactory {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        let plugin_name = self.name();

        // Deserialize and validate configuration without creating AWS clients
        let _plugin_config: AwsMutableStorePluginConfig =
            config.clone().try_into().map_err(|e| {
                PluginError::from(PluginConfigError {
                    plugin_name: plugin_name.to_string(),
                    message: format!("Failed to deserialize AWS mutable store config: {e}"),
                })
            })?;

        Ok(())
    }

    fn create(
        &self,
        config: &toml::Value,
        immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<Arc<dyn MutableStore>, PluginError> {
        let plugin_name = self.name();

        // Deserialize configuration
        let plugin_config: AwsMutableStorePluginConfig =
            config.clone().try_into().map_err(|e| {
                PluginError::from(PluginConfigError {
                    plugin_name: plugin_name.to_string(),
                    message: format!("Failed to deserialize AWS mutable store config: {e}"),
                })
            })?;

        info!(
            plugin_name = plugin_name,
            dynamodb_table = %plugin_config.dynamodb_table,
            "Creating AWS mutable store: {plugin_config:?}"
        );

        let dynamodb_client = tokio::task::block_in_place(|| {
            runtime().block_on(Box::pin(async {
                let builder = Box::pin(
                    AwsClientBuilder::builder()
                        .with_http_settings(&plugin_config.http)
                        .maybe_endpoint(plugin_config.dynamodb_endpoint_url.clone())
                        .maybe_region(plugin_config.dynamodb_region.clone())
                        .with_timeout_config(
                            TimeoutConfig::builder()
                                .operation_timeout(Duration::from_millis(
                                    plugin_config.timeout_millis,
                                ))
                                .build(),
                        )
                        .build_config(),
                )
                .await
                .with_slow_operation_threshold(
                    plugin_config.dynamodb_slow_operation_threshold_millis,
                )
                .dynamodb()
                .ensure_table(&plugin_config.dynamodb_table);

                Box::pin(builder.build()).await.map_err(|e| {
                    PluginError::from(PluginInitError {
                        plugin_name: plugin_name.to_string(),
                        message: format!("Failed to create DynamoDB client: {e}"),
                    })
                })
            }))
        })?;

        // Create settings
        let dynamodb_settings = DynamoDbMutableStoreSettings {
            mutable_store_table_name: plugin_config.dynamodb_table,
            endpoint_url: plugin_config.dynamodb_endpoint_url,
            region: plugin_config.dynamodb_region,
            slow_operation_threshold_millis: plugin_config.dynamodb_slow_operation_threshold_millis,
            timeout_millis: plugin_config.timeout_millis,
        };

        let store_settings =
            AwsMutableStoreSettings::new(dynamodb_settings, plugin_config.force_write);

        let store = AwsMutableStore::new(dynamodb_client, &store_settings, immutable_store);

        Ok(Arc::new(store))
    }
}

/// Plugin factory for creating `DynamoDB` lock stores.
///
/// This factory creates [`DynamoDbLockStore`] instances backed by `DynamoDB`.
pub struct AwsLockStorePluginFactory;

impl LockStorePluginFactory for AwsLockStorePluginFactory {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        let plugin_name = self.name();

        // Deserialize and validate configuration without creating AWS clients
        let _plugin_config: AwsLockStorePluginConfig = config.clone().try_into().map_err(|e| {
            PluginError::from(PluginConfigError {
                plugin_name: plugin_name.to_string(),
                message: format!("Failed to deserialize DynamoDB lock store config: {e}"),
            })
        })?;

        Ok(())
    }

    fn create(&self, config: &toml::Value) -> Result<Arc<dyn LockStore>, PluginError> {
        let plugin_name = self.name();

        // Deserialize configuration
        let plugin_config: AwsLockStorePluginConfig = config.clone().try_into().map_err(|e| {
            PluginError::from(PluginConfigError {
                plugin_name: plugin_name.to_string(),
                message: format!("Failed to deserialize DynamoDB lock store config: {e}"),
            })
        })?;

        info!(
            plugin_name = plugin_name,
            dynamodb_table = %plugin_config.dynamodb_table,
            "Creating DynamoDB lock store: {plugin_config:?}"
        );

        let dynamodb_client = tokio::task::block_in_place(|| {
            runtime().block_on(async {
                let builder = Box::pin(
                    AwsClientBuilder::builder()
                        .with_http_settings(&plugin_config.http)
                        .maybe_endpoint(plugin_config.dynamodb_endpoint_url.clone())
                        .maybe_region(plugin_config.dynamodb_region.clone())
                        .with_timeout_config(
                            TimeoutConfig::builder()
                                .operation_timeout(Duration::from_millis(
                                    plugin_config.timeout_millis,
                                ))
                                .build(),
                        )
                        .build_config(),
                )
                .await
                .with_slow_operation_threshold(
                    plugin_config.dynamodb_slow_operation_threshold_millis,
                )
                .dynamodb()
                .ensure_table(&plugin_config.dynamodb_table);

                Box::pin(builder.build()).await.map_err(|e| {
                    PluginError::from(PluginInitError {
                        plugin_name: plugin_name.to_string(),
                        message: format!("Failed to create DynamoDB client: {e}"),
                    })
                })
            })
        })?;

        let store = DynamoDbLockStore::new(dynamodb_client, plugin_config.dynamodb_table);

        Ok(Arc::new(store))
    }
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the AWS plugin factories and resource detector with the given
/// registry.
///
/// The resource detector is registered by the module rather than through the
/// store factories: it describes the AWS deployment environment and is
/// independent of which (if any) AWS store is the configured backend.
pub fn register(registry: &mut PluginRegistry) {
    registry.register_immutable_store_plugin(Box::new(AwsImmutableStorePluginFactory));
    registry.register_mutable_store_plugin(Box::new(AwsMutableStorePluginFactory));
    registry.register_lock_store_plugin(Box::new(AwsLockStorePluginFactory));
    registry.register_resource_detector(|runtime_handle| {
        Box::new(AWSResourceDetector::new(runtime_handle)) as Box<dyn ResourceDetector>
    });
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use tokio::runtime::Handle;

    use super::*;

    #[test]
    fn test_immutable_store_factory_name() {
        let factory = AwsImmutableStorePluginFactory;
        assert_eq!(factory.name(), PLUGIN_NAME);
    }

    #[test]
    fn test_mutable_store_factory_name() {
        let factory = AwsMutableStorePluginFactory;
        assert_eq!(factory.name(), PLUGIN_NAME);
    }

    #[test]
    fn test_lock_store_factory_name() {
        let factory = AwsLockStorePluginFactory;
        assert_eq!(factory.name(), PLUGIN_NAME);
    }

    #[tokio::test]
    async fn test_register_adds_aws_resource_detector() {
        let mut registry = PluginRegistry::new();
        register(&mut registry);

        // The module registers a single AWS resource detector, independent of
        // its three store factories.
        assert_eq!(registry.resource_detectors(Handle::current()).len(), 1);
    }

    #[tokio::test]
    async fn test_immutable_store_config_parsing_error() {
        let factory = AwsImmutableStorePluginFactory;

        // Invalid config - missing required fields
        let config = toml::Value::Table(toml::map::Map::new());
        let result = factory.validate_config(&config);

        let err = result.expect_err("should fail");
        let config_err = err
            .as_plugin_config_error()
            .expect("should be PluginConfigError");
        assert_eq!(config_err.plugin_name, PLUGIN_NAME);
        assert!(config_err.message.contains("Failed to deserialize"));
    }

    #[tokio::test]
    async fn test_mutable_store_config_parsing_error() {
        let factory = AwsMutableStorePluginFactory;

        // Invalid config - missing required fields
        let config = toml::Value::Table(toml::map::Map::new());
        let result = factory.validate_config(&config);

        let err = result.expect_err("should fail");
        let config_err = err
            .as_plugin_config_error()
            .expect("should be PluginConfigError");
        assert_eq!(config_err.plugin_name, PLUGIN_NAME);
        assert!(config_err.message.contains("Failed to deserialize"));
    }

    #[tokio::test]
    async fn test_lock_store_config_parsing_error() {
        let factory = AwsLockStorePluginFactory;

        // Invalid config - missing required fields
        let config = toml::Value::Table(toml::map::Map::new());
        let result = factory.validate_config(&config);

        let err = result.expect_err("should fail");
        let config_err = err
            .as_plugin_config_error()
            .expect("should be PluginConfigError");
        assert_eq!(config_err.plugin_name, PLUGIN_NAME);
        assert!(config_err.message.contains("Failed to deserialize"));
    }

    #[tokio::test]
    async fn test_register_adds_all_plugins() {
        let mut registry = PluginRegistry::new();
        register(&mut registry);

        let immutable_plugins = registry.list_immutable_store_plugins();
        assert!(
            immutable_plugins.contains(&PLUGIN_NAME.to_string()),
            "Expected 'aws' in immutable store plugins, found: {immutable_plugins:?}"
        );

        let mutable_plugins = registry.list_mutable_store_plugins();
        assert!(
            mutable_plugins.contains(&PLUGIN_NAME.to_string()),
            "Expected 'aws' in mutable store plugins, found: {mutable_plugins:?}"
        );

        let lock_plugins = registry.list_lock_store_plugins();
        assert!(
            lock_plugins.contains(&PLUGIN_NAME.to_string()),
            "Expected 'dynamodb' in lock store plugins, found: {lock_plugins:?}"
        );
    }

    #[tokio::test]
    async fn test_config_deserialization_with_all_fields() {
        let config_str = r#"
            s3_bucket = "test-bucket"
            s3_endpoint_url = "http://localhost:4566"
            s3_region = "us-east-1"
            dynamodb_fragments_table = "fragments"
            dynamodb_fragment_state_table = "fragment-state"
            dynamodb_endpoint_url = "http://localhost:4566"
            dynamodb_region = "us-east-1"
            s3_slow_operation_threshold_millis = 1000
            dynamodb_slow_operation_threshold_millis = 500
            timeout_millis = 3000
            force_write = true
        "#;

        let config: toml::Value = toml::from_str(config_str).unwrap();
        let plugin_config: AwsImmutableStorePluginConfig = config.try_into().unwrap();

        assert_eq!(plugin_config.s3_bucket, "test-bucket");
        assert_eq!(
            plugin_config.s3_endpoint_url,
            Some("http://localhost:4566".to_string())
        );
        assert_eq!(plugin_config.s3_region, Some("us-east-1".to_string()));
        assert_eq!(plugin_config.dynamodb_fragments_table, "fragments");
        assert_eq!(
            plugin_config.dynamodb_fragment_state_table,
            "fragment-state"
        );
        assert_eq!(
            plugin_config.dynamodb_endpoint_url,
            Some("http://localhost:4566".to_string())
        );
        assert_eq!(plugin_config.dynamodb_region, Some("us-east-1".to_string()));
        assert_eq!(plugin_config.s3_slow_operation_threshold_millis, 1000);
        assert_eq!(plugin_config.dynamodb_slow_operation_threshold_millis, 500);
        assert_eq!(plugin_config.timeout_millis, 3000);
        assert!(plugin_config.force_write);
    }

    /// A configuration written before this change points at the table holding fragment metadata,
    /// which is what that table is still read for. It carries over under its new name.
    #[tokio::test]
    async fn test_config_reads_the_former_metadata_table_key_as_the_fragment_metadata_table() {
        let config_str = r#"
            s3_bucket = "test-bucket"
            dynamodb_fragments_table = "fragments"
            dynamodb_fragment_state_table = "state"
            dynamodb_metadata_table = "metadata"
        "#;

        let config: toml::Value = toml::from_str(config_str).unwrap();
        let plugin_config: AwsImmutableStorePluginConfig = config.try_into().unwrap();

        assert_eq!(plugin_config.dynamodb_fragment_state_table, "state");
        assert_eq!(
            plugin_config.dynamodb_fragment_metadata_table,
            Some("metadata".to_string())
        );
    }

    /// The state table has no alias on purpose. A configuration that never named one must fail
    /// rather than silently reuse whichever table used to hold fragment metadata.
    #[tokio::test]
    async fn test_config_requires_the_fragment_state_table() {
        let config_str = r#"
            s3_bucket = "test-bucket"
            dynamodb_fragments_table = "fragments"
            dynamodb_metadata_table = "metadata"
        "#;

        let config: toml::Value = toml::from_str(config_str).unwrap();

        config
            .try_into::<AwsImmutableStorePluginConfig>()
            .expect_err("the fragment state table must be configured explicitly");
    }

    #[tokio::test]
    async fn test_config_deserialization_with_defaults() {
        let config_str = r#"
            s3_bucket = "test-bucket"
            dynamodb_fragments_table = "fragments"
            dynamodb_fragment_state_table = "fragment-state"
        "#;

        let config: toml::Value = toml::from_str(config_str).unwrap();
        let plugin_config: AwsImmutableStorePluginConfig = config.try_into().unwrap();

        assert_eq!(plugin_config.s3_bucket, "test-bucket");
        assert!(plugin_config.s3_endpoint_url.is_none());
        assert!(plugin_config.s3_region.is_none());
        assert_eq!(plugin_config.dynamodb_fragments_table, "fragments");
        assert_eq!(
            plugin_config.dynamodb_fragment_state_table,
            "fragment-state"
        );
        assert!(plugin_config.dynamodb_endpoint_url.is_none());
        assert!(plugin_config.dynamodb_region.is_none());
        assert_eq!(plugin_config.s3_slow_operation_threshold_millis, u64::MAX);
        assert_eq!(
            plugin_config.dynamodb_slow_operation_threshold_millis,
            u64::MAX
        );
        assert_eq!(plugin_config.timeout_millis, 5000);
        assert!(!plugin_config.force_write);
    }

    #[tokio::test]
    async fn test_lock_store_config_error_includes_field_name() {
        let factory = AwsLockStorePluginFactory;

        // Empty config - missing required 'dynamodb_table' field
        let config = toml::Value::Table(toml::map::Map::new());
        let result = factory.validate_config(&config);

        let err = result.expect_err("should fail");
        let config_err = err
            .as_plugin_config_error()
            .expect("should be PluginConfigError");
        assert_eq!(config_err.plugin_name, PLUGIN_NAME);
        assert!(
            config_err.message.contains("dynamodb_table"),
            "Error message should mention the missing field 'dynamodb_table', got: {}",
            config_err.message
        );
    }
}
