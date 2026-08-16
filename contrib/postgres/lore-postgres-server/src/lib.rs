// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Derived Lore server with a PostgreSQL catalog and S3-compatible payload storage.

use std::sync::Arc;
use std::time::Duration;

use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::HttpClientSettings;
use lore_aws::clients::TimeoutConfig;
use lore_aws::store::immutable_store::AwsImmutableStore;
use lore_aws::store::immutable_store::ObjectStoreImmutableStoreSettings;
use lore_aws::store::immutable_store::S3ObjectVersioning;
use lore_aws::store::immutable_store::S3StoreSettings;
use lore_base::error::PluginConfigError;
use lore_base::error::PluginInitError;
use lore_base::runtime::runtime;
use lore_postgres::PostgresFragmentCatalog;
use lore_postgres::PostgresFragmentCatalogConfig;
use lore_server::plugins::ImmutableStorePluginFactory;
use lore_server::plugins::PluginError;
use lore_server::plugins::PluginRegistry;
use lore_storage::ImmutableStore;
use serde::Deserialize;
use tracing::info;

/// Configuration name used in `[immutable_store]` and `[plugins]`.
pub const PLUGIN_NAME: &str = "postgres_s3";

fn default_slow_threshold() -> u64 {
    u64::MAX
}

fn default_timeout() -> u64 {
    5_000
}

/// PostgreSQL catalog plus S3-compatible payload-store configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresS3ImmutableStorePluginConfig {
    /// HTTP client settings for S3-compatible operations.
    #[serde(default)]
    pub http: HttpClientSettings,
    /// Bucket that holds fragment payload bytes.
    pub s3_bucket: String,
    /// S3-compatible endpoint. Cloudflare R2 uses its account endpoint.
    #[serde(default)]
    pub s3_endpoint_url: Option<String>,
    /// S3-compatible region. Cloudflare R2 uses `auto`.
    #[serde(default)]
    pub s3_region: Option<String>,
    /// Whether the backend retains historical object versions.
    #[serde(default)]
    pub s3_object_versioning: S3ObjectVersioning,
    /// Slow-operation threshold for payload requests.
    #[serde(default = "default_slow_threshold")]
    pub s3_slow_operation_threshold_millis: u64,
    /// End-to-end timeout for payload requests.
    #[serde(default = "default_timeout")]
    pub timeout_millis: u64,
    /// Use path-style S3 addressing.
    #[serde(default)]
    pub s3_force_path_style: bool,
    /// Overwrite an existing payload during writes.
    #[serde(default)]
    pub force_write: bool,
    /// PostgreSQL catalog connection and namespace.
    pub postgres: PostgresFragmentCatalogConfig,
}

impl PostgresS3ImmutableStorePluginConfig {
    fn validate(&self) -> Result<(), String> {
        if self.s3_bucket.trim().is_empty() {
            return Err("s3_bucket must not be empty".to_string());
        }
        if self.timeout_millis == 0 {
            return Err("timeout_millis must be greater than zero".to_string());
        }
        self.postgres.validate().map_err(|error| error.to_string())
    }
}

/// Creates an immutable store backed by PostgreSQL metadata and S3-compatible payloads.
pub struct PostgresS3ImmutableStorePluginFactory;

impl PostgresS3ImmutableStorePluginFactory {
    fn parse(
        &self,
        config: &toml::Value,
    ) -> Result<PostgresS3ImmutableStorePluginConfig, PluginError> {
        let parsed: PostgresS3ImmutableStorePluginConfig =
            config.clone().try_into().map_err(|error| {
                PluginError::from(PluginConfigError {
                    plugin_name: PLUGIN_NAME.to_string(),
                    message: format!("Failed to deserialize PostgreSQL/S3 config: {error}"),
                })
            })?;
        parsed.validate().map_err(|message| {
            PluginError::from(PluginConfigError {
                plugin_name: PLUGIN_NAME.to_string(),
                message,
            })
        })?;
        Ok(parsed)
    }
}

impl ImmutableStorePluginFactory for PostgresS3ImmutableStorePluginFactory {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        self.parse(config).map(|_| ())
    }

    fn create(&self, config: &toml::Value) -> Result<Arc<dyn ImmutableStore>, PluginError> {
        let plugin_config = self.parse(config)?;
        info!(
            plugin_name = PLUGIN_NAME,
            s3_bucket = %plugin_config.s3_bucket,
            postgres_schema = %plugin_config.postgres.schema,
            object_versioning = ?plugin_config.s3_object_versioning,
            "Creating PostgreSQL catalog with S3-compatible payload storage"
        );

        let (s3_client, catalog) = tokio::task::block_in_place(|| {
            runtime().block_on(Box::pin(async {
                let s3_client = AwsClientBuilder::builder()
                    .with_http_settings(&plugin_config.http)
                    .maybe_endpoint(plugin_config.s3_endpoint_url.clone())
                    .maybe_region(plugin_config.s3_region.clone())
                    .with_timeout_config(
                        TimeoutConfig::builder()
                            .operation_timeout(Duration::from_millis(plugin_config.timeout_millis))
                            .build(),
                    )
                    .build_config()
                    .await
                    .with_slow_operation_threshold(plugin_config.s3_slow_operation_threshold_millis)
                    .s3_with_path_style(plugin_config.s3_force_path_style)
                    .ensure_bucket(&plugin_config.s3_bucket)
                    .build()
                    .await
                    .map_err(|error| {
                        PluginError::from(PluginInitError {
                            plugin_name: PLUGIN_NAME.to_string(),
                            message: format!("Failed to create S3-compatible client: {error}"),
                        })
                    })?;

                let catalog = PostgresFragmentCatalog::connect(plugin_config.postgres.clone())
                    .await
                    .map_err(|error| {
                        PluginError::from(PluginInitError {
                            plugin_name: PLUGIN_NAME.to_string(),
                            message: format!("Failed to create PostgreSQL catalog: {error}"),
                        })
                    })?;

                Ok::<_, PluginError>((s3_client, catalog))
            }))
        })?;

        let s3_settings = S3StoreSettings {
            bucket: plugin_config.s3_bucket,
            endpoint_url: plugin_config.s3_endpoint_url,
            region: plugin_config.s3_region,
            object_versioning: plugin_config.s3_object_versioning,
            slow_operation_threshold_millis: plugin_config.s3_slow_operation_threshold_millis,
            timeout_millis: plugin_config.timeout_millis,
        };
        let settings =
            ObjectStoreImmutableStoreSettings::new(s3_settings, plugin_config.force_write);
        Ok(Arc::new(AwsImmutableStore::with_catalog(
            s3_client,
            Arc::new(catalog),
            &settings,
        )))
    }
}

/// Register the PostgreSQL/S3 immutable-store plugin.
pub fn register(registry: &mut PluginRegistry) {
    registry.register_immutable_store_plugin(Box::new(PostgresS3ImmutableStorePluginFactory));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> toml::Value {
        toml::from_str(
            r#"
s3_bucket = "lore-test"
s3_endpoint_url = "https://example.r2.cloudflarestorage.com"
s3_region = "auto"
s3_object_versioning = "unversioned"

[postgres]
connection_string = "host=localhost user=lore password=do-not-log"
schema = "lore_catalog"
"#,
        )
        .expect("valid test config")
    }

    #[test]
    fn parses_r2_config_and_redacts_postgres_credentials() {
        let factory = PostgresS3ImmutableStorePluginFactory;
        let parsed = factory.parse(&config()).expect("config should validate");

        assert_eq!(parsed.s3_object_versioning, S3ObjectVersioning::Unversioned);
        let debug = format!("{parsed:?}");
        assert!(!debug.contains("do-not-log"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn rejects_invalid_postgres_schema_without_connecting() {
        let mut config = config();
        config["postgres"]["schema"] = toml::Value::String("not-safe;drop".to_string());

        let error = PostgresS3ImmutableStorePluginFactory
            .validate_config(&config)
            .expect_err("unsafe identifier should fail validation");
        assert!(error.to_string().contains("identifier"));
    }

    #[test]
    fn register_adds_postgres_s3_plugin() {
        let mut registry = PluginRegistry::new();
        register(&mut registry);

        assert_eq!(
            registry.list_immutable_store_plugins(),
            vec![PLUGIN_NAME.to_string()]
        );
    }
}
