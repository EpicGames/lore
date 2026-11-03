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
use lore_revision::util::time::RetryPolicy;
use lore_revision::util::time::RetrySettings;
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

#[lore_macro::test_pub]
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

    /// Backoff for resending the keys a `DynamoDB` batch read leaves unprocessed. `max_attempts`
    /// counts resends; a read with keys still unprocessed after them fails. Unset, the client's
    /// default applies.
    #[serde(default)]
    pub dynamodb_unprocessed_keys_retry: Option<RetrySettings>,

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

    /// Backoff for resending the keys a `DynamoDB` batch read leaves unprocessed. `max_attempts`
    /// counts resends; a read with keys still unprocessed after them fails. Unset, the client's
    /// default applies.
    #[serde(default)]
    pub dynamodb_unprocessed_keys_retry: Option<RetrySettings>,

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

        // Plugin construction is a synchronous trait method. It runs once at startup, one plugin
        // at a time, so at most one core is handed off at a time.
        #[allow(clippy::disallowed_methods)]
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
                .maybe_unprocessed_keys_retry(
                    plugin_config
                        .dynamodb_unprocessed_keys_retry
                        .as_ref()
                        .map(RetryPolicy::from),
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

        // Plugin construction is a synchronous trait method. It runs once at startup, one plugin
        // at a time, so at most one core is handed off at a time.
        #[allow(clippy::disallowed_methods)]
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

        // Plugin construction is a synchronous trait method. It runs once at startup, one plugin
        // at a time, so at most one core is handed off at a time.
        #[allow(clippy::disallowed_methods)]
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
                .maybe_unprocessed_keys_retry(
                    plugin_config
                        .dynamodb_unprocessed_keys_retry
                        .as_ref()
                        .map(RetryPolicy::from),
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
