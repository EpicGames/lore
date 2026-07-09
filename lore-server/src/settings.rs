// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::env;

use config::Config;
use lore_base::runtime::TokioSettings;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::version::LORE_LIBRARY_VERSION;
use lore_revision::branch::CachedRevisionItem;
use lore_revision::branch::CachedRevisionListHeader;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::environment::EnvironmentConfig;
use lore_revision::util::time::RetrySettings;
use lore_storage::hash;
use lore_storage::hash::StringHash;
use lore_telemetry::TelemetryConfig;
use lore_telemetry::TraceConfigError;
use serde::Deserialize;

use crate::auth::jwk::JWKServiceSettings;
use crate::grpc::server::FeatureSettings;
use crate::grpc::server::GrpcPublicServicesSettings;
use crate::hooks::HookSettings;
use crate::quic::client_monitor::default_quic_client_monitor_interval_secs;
use crate::store::replica_factory::ReplicaFactorySettings;
use crate::tls::CertificateSettings;
use crate::topology::TopologySettings;

#[derive(Clone, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Settings {
    pub environment: Option<EnvironmentConfig>,
    pub immutable_store: ImmutableStoreSettings,
    pub mutable_store: MutableStoreSettings,
    pub lock_store: Option<LockStoreSettings>,
    pub telemetry: Option<TelemetryConfig>,
    pub server: ServerSettings,
    pub feature: Option<FeatureSettings>,
    pub tokio: Option<TokioSettings>,
    pub notification: Option<NotificationSettings>,
    pub topology: Option<TopologySettings>,
    #[serde(default)]
    pub plugins: HashMap<String, toml::Value>,
    #[serde(default)]
    pub hooks: HashMap<String, HookSettings>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid printing the plugins and hooks in case they contain api keys
        // or other sensitive information, and will print all since they are
        // just toml bags of key-value pairs
        f.debug_struct("Settings")
            .field("environment", &self.environment)
            .field("immutable_store", &self.immutable_store)
            .field("mutable_store", &self.mutable_store)
            .field("lock_store", &self.lock_store)
            .field("telemetry", &self.telemetry)
            .field("server", &self.server)
            .field("feature", &self.feature)
            .field("tokio", &self.tokio)
            .field("notification", &self.notification)
            .field("topology", &self.topology)
            .field(
                "plugins",
                &self
                    .plugins
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .field(
                "hooks",
                &self
                    .hooks
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .finish()
    }
}

/// The built-in default configuration, baked into the binary at compile time.
///
/// This is the contents of `config/default.toml` and serves as the base layer
/// for every server invocation, so a stand alone server binary with no external
/// config starts with sensible defaults and needs no configuration files on disk.
const DEFAULT_CONFIG_TOML: &str = include_str!("../config/default.toml");

/// The on-disk config directory used when no `config_path` is supplied (via
/// `--config` / `LORE_CONFIG_PATH`). Files in it are optional, so a missing
/// directory just leaves the server running on its built-in defaults.
const DEFAULT_CONFIG_DIR: &str = "lore-server/config";

impl Settings {
    /// Load settings, layering optional on-disk overrides over the built-in
    /// defaults baked into the binary.
    ///
    /// Layering order (later sources win):
    /// 1. The built-in [`DEFAULT_CONFIG_TOML`] (always present).
    /// 2. The optional on-disk `default.toml` from the config directory.
    ///    When present it lets operators tune the compiled-in defaults without
    ///    requiring a rebuild.
    /// 3. The optional files `<environment>.toml`,
    ///    `<environment>_<region>.toml`, and `local.toml` from the config
    ///    directory. The directory comes from `config_path` (via `--config` /
    ///    `LORE_CONFIG_PATH`) when supplied, otherwise it falls back to
    ///    [`DEFAULT_CONFIG_DIR`].
    /// 4. Environment variables prefixed with `LORE__`.
    ///
    /// Every on-disk file is optional, so the server still starts from the
    /// built-in defaults (and environment variables) even when the config
    /// directory is absent.
    pub fn load(
        config_path: Option<&str>,
        environment: Option<&str>,
    ) -> Result<(Self, StringHash), config::ConfigError> {
        println!("Server version: {}", LORE_LIBRARY_VERSION.as_str());

        let environment = environment.unwrap_or("local");
        println!("Using environment: {environment}");

        // Start from the defaults baked into the binary so the server can run
        // with no configuration files present at all.
        let mut settings_builder = Config::builder().add_source(config::File::from_str(
            DEFAULT_CONFIG_TOML,
            config::FileFormat::Toml,
        ));

        // Resolve the config directory: use the path supplied on the command
        // line (or via LORE_CONFIG_PATH) when present, otherwise fall back to
        // the default `lore-server/config` directory. Every on-disk file below
        // is optional: missing files (or a missing directory) are silently skipped.
        let config_path = config_path.unwrap_or(DEFAULT_CONFIG_DIR);
        println!("Using config path: {config_path}");

        // Layer an optional on-disk default.toml for env/region agnostic settings
        // extending the built-in defaults
        settings_builder = settings_builder
            .add_source(config::File::with_name(&format!("{config_path}/default")).required(false));
        settings_builder = settings_builder.add_source(
            config::File::with_name(&format!("{config_path}/{environment}")).required(false),
        );
        if let Ok(instance_region) = env::var("LORE_PLATFORM_REGION") {
            settings_builder = settings_builder.add_source(
                config::File::with_name(&format!("{config_path}/{environment}_{instance_region}"))
                    .required(false),
            );
        }
        settings_builder = settings_builder
            .add_source(config::File::with_name(&format!("{config_path}/local")).required(false));

        settings_builder =
            settings_builder.add_source(config::Environment::with_prefix("lore").separator("__"));

        let settings = settings_builder.build()?;
        let settings: Settings = settings.try_deserialize()?;
        validate_trace_config(&settings)?;
        validate_feature_config(&settings)?;
        let settings_string = format!("{settings:?}");
        let settings_hash = hash::hash_string(&settings_string);

        // Logger isn't configured yet.
        println!("Loaded config: {settings_string}");

        Ok((settings, settings_hash))
    }
}

fn validate_trace_config(settings: &Settings) -> Result<(), config::ConfigError> {
    if let Some(traces) = settings.telemetry.as_ref().and_then(|t| t.traces.as_ref()) {
        traces.validate().map_err(trace_config_error_to_config)?;
    }
    Ok(())
}

/// A cached revision-list blob is a single `CachedRevisionListHeader`
/// followed by `history_step_size` packed `CachedRevisionItem`s. The
/// whole blob is written to the immutable store as one fragment, so it
/// must fit strictly under `FRAGMENT_SIZE_THRESHOLD` — otherwise pushes
/// would silently fail to materialize cache entries.
fn validate_feature_config(settings: &Settings) -> Result<(), config::ConfigError> {
    let history_step_size = settings
        .feature
        .as_ref()
        .and_then(|f| f.history_step_size)
        .unwrap_or(DEFAULT_HISTORY_STEP_SIZE);
    let header_size = std::mem::size_of::<CachedRevisionListHeader>();
    let item_size = std::mem::size_of::<CachedRevisionItem>();
    let blob_size =
        header_size.saturating_add((history_step_size as usize).saturating_mul(item_size));
    if blob_size >= FRAGMENT_SIZE_THRESHOLD {
        return Err(config::ConfigError::Message(format!(
            "feature.history_step_size ({history_step_size}) × CachedRevisionItem size \
             ({item_size}) + header ({header_size}) = {blob_size} bytes does not fit under the \
             fragment threshold ({FRAGMENT_SIZE_THRESHOLD}); reduce history_step_size",
        )));
    }
    Ok(())
}

fn trace_config_error_to_config(err: TraceConfigError) -> config::ConfigError {
    if let Some(out_of_range) = err.as_out_of_range() {
        return config::ConfigError::Message(format!(
            "telemetry.traces.{} value {} is outside [0.0, 1.0]",
            out_of_range.field, out_of_range.value
        ));
    }
    config::ConfigError::Message(format!("telemetry.traces validation failed: {err}"))
}

///
/// Server-related settings
///

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct AuthSettings {
    pub jwk: Option<JWKServiceSettings>,
    pub jwt_audience: Option<Vec<String>>,
    pub jwt_issuer: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct GrpcSettings {
    /// Whether to start this gRPC endpoint. Defaults to `false`
    #[serde(default)]
    pub enabled: bool,
    pub certificate: Option<CertificateSettings>,
    pub host: String,
    pub port: i32,
    pub http2_keepalive_interval_seconds: Option<u64>,
    pub http2_keepalive_timeout_seconds: Option<u64>,
    /// Keep below the ALB timeout to ensure we gracefully observe stuck requests
    /// rather than clients receive a 504 response from the ALB
    pub request_handler_timeout_seconds: u64,
    /// Require client certificates (mTLS): `true` demands a full mTLS triple,
    /// `false` accepts unverified clients.
    #[serde(default = "default_verify_client_certs")]
    pub verify_client_certs: bool,
}

fn default_verify_client_certs() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct HttpSettings {
    #[allow(dead_code)]
    pub certificate: Option<CertificateSettings>,
    pub enabled: bool,
    pub host: String,
    pub max_file_size: u64,
    pub port: i32,
    pub request_timeout_seconds: u64,
    pub request_body_timeout_seconds: u64,
    pub available_interval_seconds: u64,
    pub available_timeout_seconds: u64,
    pub store_health_check: bool,
    pub presigned_url_hmac_key: Option<String>,
    #[serde(default = "HttpSettings::default_presigned_url_min_ttl_seconds")]
    pub presigned_url_min_ttl_seconds: u64,
    #[serde(default = "HttpSettings::default_presigned_url_default_ttl_seconds")]
    pub presigned_url_default_ttl_seconds: u64,
    #[serde(default = "HttpSettings::default_presigned_url_max_ttl_seconds")]
    pub presigned_url_max_ttl_seconds: u64,
}

impl HttpSettings {
    fn default_presigned_url_min_ttl_seconds() -> u64 {
        1
    }

    fn default_presigned_url_default_ttl_seconds() -> u64 {
        3600
    }

    fn default_presigned_url_max_ttl_seconds() -> u64 {
        86400
    }
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct QuicSettings {
    /// Whether to start this QUIC endpoint. Defaults to `false`
    #[serde(default)]
    pub enabled: bool,
    pub certificate: Option<CertificateSettings>,
    /// Require client certificates (mTLS): `true` demands a full mTLS triple,
    /// `false` accepts unverified clients.
    #[serde(default = "default_verify_client_certs")]
    pub verify_client_certs: bool,
    pub host: String,
    pub idle_timeout: Option<u64>,
    pub keep_alive: Option<u64>,
    pub max_bidi_streams: Option<u64>,
    pub num_listeners: u8,
    pub port: i32,
    pub transport_bits_per_second: Option<usize>,
    pub transport_rtt: Option<usize>,
    /// Keep below a threshold for whatever Load Balancer sits infront of the server
    /// or is expecting responses. If request handlers exceed this reasonable threshold
    /// then assume something has gone wrong and return a timeout response so we can get metrics
    /// and clients don't hang forever
    pub handler_timeout_seconds: Option<u64>,
    /// How many inflight messages are allowed per connection
    pub connection_message_limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct ServerSettings {
    pub auth: Option<AuthSettings>,
    pub grpc: Option<GrpcSettings>,
    pub grpc_public_services: Option<GrpcPublicServicesSettings>,
    pub grpc_internal: Option<GrpcSettings>,
    pub http: Option<HttpSettings>,
    // the public facing QUIC server settings
    pub quic: Option<QuicSettings>,
    // the internal-only QUIC server settings
    pub quic_internal: Option<QuicSettings>,
    /// Seconds to wait for existing connections to close gracefully after shutdown signal.
    #[serde(default = "default_connection_close_timeout")]
    pub connection_close_timeout_seconds: u16,
    /// Seconds to wait for async runtime to shut down after connections are closed.
    #[serde(
        default = "default_runtime_shutdown_timeout",
        alias = "shutdown_delay_seconds"
    )]
    pub runtime_shutdown_timeout_seconds: u16,
    #[serde(default)]
    pub user_agent: UserAgentSettings,
}

// For when this server acts as a client to another server's Internal port
#[derive(Clone, Debug, Deserialize)]
pub struct GrpcInternalClientSettings {
    pub url: String,
    pub certs: Option<CertificateSettings>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct UserAgentSettings {
    #[serde(default)]
    pub user_agent_patterns: Vec<String>,
    #[serde(default)]
    pub unknown_user_agent_sample_rate: f64,
}

fn default_connection_close_timeout() -> u16 {
    5
}

fn default_runtime_shutdown_timeout() -> u16 {
    25
}

///
/// Storage-related settings
///

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct CompositeStoreSettings {
    pub durable: Option<CompositeSubStoreSettings>,
    pub local: CompositeSubStoreSettings,
    pub replica: Option<Vec<CompositeSubStoreSettings>>,
    pub replica_factory: Option<ReplicaFactorySettings>,
    pub should_cache_query_results: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct CompositeSubStoreSettings {
    pub local: Option<LocalImmutableStoreSettings>,
    pub mode: String,
    pub remote: Option<RemoteStoreSettings>,
    pub replicated: Option<ReplicatedStoreSettings>,
    pub replication_mode: Option<ReplicationMode>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct ImmutableStoreSettings {
    pub composite: Option<CompositeStoreSettings>,
    pub local: Option<LocalImmutableStoreSettings>,
    pub mode: String,
    pub remote: Option<RemoteStoreSettings>,
    pub replicated: Option<ReplicatedStoreSettings>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct LocalImmutableStoreSettings {
    pub compaction_delay: Option<usize>,
    pub eviction_delay: Option<usize>,
    pub flush_delay_seconds: u16,
    /// Filesystem location for the local store. When empty (the default), the
    /// server derives `<system temp dir>/lore-server` at startup so a stand
    /// alone server binary with no external config can run as-is.
    #[serde(default)]
    pub path: String,
    pub max_capacity: Option<usize>,
    pub max_size: Option<usize>,
    pub target_capacity_percentage: Option<usize>,
    pub target_size_percentage: Option<usize>,
    pub compaction_parallel_groups: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct LocalMutableStoreSettings {
    pub flush_delay_seconds: u16,
    /// Filesystem location for the local store. When empty (the default), the
    /// server derives `<system temp dir>/lore-server` at startup so a stand
    /// alone server binary with no external config can run as-is.
    #[serde(default)]
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct MutableStoreSettings {
    pub local: Option<LocalMutableStoreSettings>,
    pub mode: String,
    pub remote: Option<RemoteStoreSettings>,
}

#[derive(Clone, Default, Debug, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct RemoteStoreSettings {
    pub auth_url: Option<String>,
    pub remote_url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReplicatedStoreSettings {
    pub remote_url: String,
    pub certs: Option<CertificateSettings>,
    pub regenerate_retry: RetrySettings,
    pub periodic_client_refresh_secs: u64,
    #[serde(default = "default_quic_client_monitor_interval_secs")]
    pub client_metrics_interval_seconds: u64,
    /// how many inflight messages are allowed before we self-throttle
    pub client_message_limit: Option<usize>,
    pub client_max_reconnects: Option<u32>,
    pub max_bandwidth_bytes_per_second: Option<u64>,
    pub expected_rtt_ms: Option<u64>,
}

#[derive(Copy, Clone, Default, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReplicationMode {
    Read,
    Write,
    #[default]
    ReadWrite,
}

/// Lock-related settings
///
/// Settings for lock store configuration using dynamic plugin selection.
#[derive(Clone, Debug, Default, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct LockStoreSettings {
    /// The lock store plugin mode (e.g., "dynamodb", "local")
    #[allow(dead_code)]
    pub mode: String,
}

/// Notification system configuration.
///
/// The `mode` field selects the notification backend:
/// - `"local"` (default) - In-process broadcast channels, no external dependencies
/// - Any other value - Looked up as a notification plugin in the `PluginRegistry`
///
/// Plugin-specific configuration is provided through the top-level `[plugins.<name>]`
/// section in the config file, not in this struct.
#[derive(Clone, Debug, Default, Deserialize)]
//#[serde(deny_unknown_fields)]
pub struct NotificationSettings {
    /// The notification backend mode. Defaults to "local" if not specified.
    #[serde(default = "default_notification_mode")]
    pub mode: String,
}

fn default_notification_mode() -> String {
    "local".to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Instant;

    use lore_revision::cluster::peer::Locality;

    use super::*;
    use crate::plugins::PluginRegistry;
    use crate::store::resolve_plugin_config_with_fallback;
    use crate::topology::TopologyProvider;

    #[test]
    fn test_settings_with_plugin_sections() {
        let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "aws"

            [mutable_store]
            mode = "aws"

            [topology]
            provider = "consul"

            [plugins.aws]
            s3_bucket = "my-bucket"
            region = "us-east-1"

            [plugins.consul]
            address = "localhost:8500"

            [hooks.compliance]
            enabled = true
            webhook_url = "https://example.com/notify"
        "#;

        let settings: Settings = toml::from_str(config).unwrap();
        assert_eq!(settings.immutable_store.mode, "aws");
        assert_eq!(settings.mutable_store.mode, "aws");
        assert!(settings.plugins.contains_key("aws"));
        assert!(settings.plugins.contains_key("consul"));

        // Verify aws plugin settings
        let aws_plugin = settings.plugins.get("aws").unwrap();
        assert_eq!(
            aws_plugin.get("s3_bucket").unwrap().as_str().unwrap(),
            "my-bucket"
        );
        assert_eq!(
            aws_plugin.get("region").unwrap().as_str().unwrap(),
            "us-east-1"
        );

        // Verify consul plugin settings
        let consul_plugin = settings.plugins.get("consul").unwrap();
        assert_eq!(
            consul_plugin.get("address").unwrap().as_str().unwrap(),
            "localhost:8500"
        );

        // Verify hooks
        let compliance_hook = settings.hooks.get("compliance").unwrap();
        assert!(compliance_hook.enabled);
        assert_eq!(
            compliance_hook
                .config
                .get("webhook_url")
                .unwrap()
                .as_str()
                .unwrap(),
            "https://example.com/notify"
        );

        // Verify topology
        let topology = settings.topology.unwrap();
        assert!(matches!(
            topology.provider,
            crate::topology::TopologyProvider::Consul
        ));
    }

    #[test]
    fn test_settings_empty_plugins_and_hooks() {
        let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5
        "#;

        let settings: Settings = toml::from_str(config).unwrap();
        assert_eq!(settings.immutable_store.mode, "local");
        assert_eq!(settings.mutable_store.mode, "local");
        assert!(settings.plugins.is_empty());
        assert!(settings.hooks.is_empty());
    }

    #[test]
    fn test_hook_settings_disabled_by_default() {
        let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [hooks.some_hook]
            custom_field = "value"
        "#;

        let settings: Settings = toml::from_str(config).unwrap();
        let some_hook = settings.hooks.get("some_hook").unwrap();
        // enabled defaults to false
        assert!(!some_hook.enabled);
        assert_eq!(
            some_hook
                .config
                .get("custom_field")
                .unwrap()
                .as_str()
                .unwrap(),
            "value"
        );
    }

    #[test]
    fn test_settings_with_lock_store() {
        let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [lock_store]
            mode = "local"
        "#;

        let settings: Settings = toml::from_str(config).unwrap();
        assert!(settings.lock_store.is_some());
        // "local" mode uses the built-in LocalLockStore (in-memory lock store)
        assert_eq!(settings.lock_store.unwrap().mode, "local");
    }

    #[test]
    fn test_settings_with_builtin_fixed_topology() {
        let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [topology]
            provider = "fixed"

            [topology.fixed]
            peers = [{ address = "192.168.1.10", port = 9090, locality = "SameRegion" }]
        "#;

        let settings: Settings = toml::from_str(config).unwrap();

        // Verify topology is configured with fixed provider
        let topology = settings.topology.as_ref().unwrap();
        assert!(matches!(
            topology.provider,
            crate::topology::TopologyProvider::Fixed
        ));

        // Verify the built-in fixed topology configuration is present
        let fixed = topology.fixed.as_ref().unwrap();
        assert_eq!(fixed.peers.len(), 1);
        assert_eq!(fixed.peers[0].address, "192.168.1.10");
        assert_eq!(fixed.peers[0].port, 9090);
        assert_eq!(fixed.peers[0].locality, Locality::SameRegion);

        // No plugins should be needed for built-in fixed topology
        assert!(!settings.plugins.contains_key("fixed"));
    }

    #[test]
    fn test_settings_with_no_topology() {
        let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5
        "#;

        let settings: Settings = toml::from_str(config).unwrap();

        // Verify topology is not configured
        assert!(settings.topology.is_none());
    }

    #[test]
    fn test_settings_with_none_topology_provider() {
        let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [topology]
            provider = "none"
        "#;

        let settings: Settings = toml::from_str(config).unwrap();

        // Verify topology has 'none' provider
        let topology = settings.topology.as_ref().unwrap();
        assert!(matches!(
            topology.provider,
            crate::topology::TopologyProvider::None
        ));
    }

    // =========================================================================
    // Config File Validation Tests
    // =========================================================================
    //
    // These tests validate all configuration files in `lore-server/config/` to ensure:
    // 1. All config files can be loaded and parsed successfully
    // 2. Plugin configurations are correctly structured
    // 3. Plugins referenced in configs can be loaded by the current binary
    //
    // TODO(mjansson): The Settings and ServerSettings structs SHOULD use `#[serde(deny_unknown_fields)]`
    //                 to catch invalid config sections (e.g., `[server.lock_store]` instead of `[lock_store]`).

    /// Store modes that are handled directly (not via plugins)
    const CORE_STORE_MODES: &[&str] = &["local", "composite", "remote"];

    /// Finds the config directory relative to the workspace root.
    fn find_config_dir() -> PathBuf {
        // Try multiple potential locations for the config directory
        let potential_paths = [
            PathBuf::from("lore-server/config"),
            PathBuf::from("config"),
            PathBuf::from("../lore-server/config"),
            PathBuf::from("../../lore-server/config"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config"),
        ];

        for path in &potential_paths {
            if path.exists() && path.is_dir() {
                return path.clone();
            }
        }

        // Last resort: use the CARGO_MANIFEST_DIR relative path
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config")
    }

    /// Discovers all config files in the config directory.
    /// Excludes `.example` files which are templates.
    fn discover_standalone_config_files() -> Vec<PathBuf> {
        let config_dir = find_config_dir();

        if !config_dir.exists() {
            panic!(
                "Config directory not found. Tried: {:?}. Current dir: {:?}",
                config_dir,
                std::env::current_dir()
            );
        }

        let mut config_files = Vec::new();
        let region_suffixes = ["us-east-2", "ap-southeast-1", "eu-west-3"];

        for entry in fs::read_dir(&config_dir).expect("Failed to read config directory") {
            let entry = entry.expect("Failed to read directory entry");
            let path = entry.path();

            if let Some(extension) = path.extension()
                && extension == "toml"
            {
                let file_name = path.file_name().unwrap_or_default().to_string_lossy();
                // skip intentionally partial regional-override configs
                let is_region_override = region_suffixes
                    .iter()
                    .any(|r| file_name.ends_with(&format!("_{r}.toml")));
                // Skip example files
                let is_example = file_name.ends_with(".example") || file_name.contains(".example.");

                if !is_example && !is_region_override {
                    config_files.push(path);
                }
            }
        }

        config_files.sort();
        config_files
    }

    /// Loads and merges config files similar to how the binary does it.
    /// This loads default.toml and merges with the environment-specific config.
    fn load_merged_config(config_path: &Path) -> Result<Settings, String> {
        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        let default_path = config_dir.join("default.toml");

        // Load default config
        let default_content = fs::read_to_string(&default_path)
            .map_err(|e| format!("Failed to read default.toml at {default_path:?}: {e}"))?;

        let mut default_settings: toml::Value = toml::from_str(&default_content)
            .map_err(|e| format!("Failed to parse default.toml at {default_path:?}: {e}"))?;

        // If this is not the default.toml itself, merge with the environment config
        if config_path.file_name().unwrap_or_default() != "default.toml" {
            let env_content = fs::read_to_string(config_path)
                .map_err(|e| format!("Failed to read config at {config_path:?}: {e}"))?;

            let env_settings: toml::Value = toml::from_str(&env_content)
                .map_err(|e| format!("Failed to parse config at {config_path:?}: {e}"))?;

            // Deep merge env_settings into default_settings
            merge_toml_values(&mut default_settings, &env_settings);
        }

        // Deserialize the merged config
        default_settings
            .clone()
            .try_into()
            .map_err(|e| format!("Failed to deserialize merged config for {config_path:?}: {e}"))
    }

    /// Deep merges two TOML values. Source values override target values.
    fn merge_toml_values(target: &mut toml::Value, source: &toml::Value) {
        match (target, source) {
            (toml::Value::Table(target_table), toml::Value::Table(source_table)) => {
                for (key, source_value) in source_table {
                    if let Some(target_value) = target_table.get_mut(key) {
                        merge_toml_values(target_value, source_value);
                    } else {
                        target_table.insert(key.clone(), source_value.clone());
                    }
                }
            }
            (target, source) => {
                *target = source.clone();
            }
        }
    }

    /// Checks if a mode requires plugin configuration.
    fn requires_plugin_config(mode: &str) -> bool {
        !CORE_STORE_MODES.contains(&mode)
    }

    /// Validates that a plugin configuration exists and is properly structured.
    fn validate_plugin_config(
        plugins: &HashMap<String, toml::Value>,
        mode: &str,
        store_type: &str,
    ) -> Result<(), String> {
        if !requires_plugin_config(mode) {
            return Ok(());
        }

        // Check if plugin config exists
        if !plugins.contains_key(mode) {
            return Err(format!(
                "Mode '{mode}' requires plugin configuration [plugins.{mode}], but it's missing"
            ));
        }

        // For AWS plugin, verify the store-specific section exists or can be inferred
        if let Some(plugin_config) = plugins.get(mode) {
            // Check if there's a store-specific section
            let has_store_section = plugin_config.get(store_type).is_some();

            // For AWS plugin, we require store-specific config sections
            if mode == "aws" && !has_store_section {
                return Err(format!(
                    "AWS plugin config [plugins.aws] is missing [plugins.aws.{store_type}] section"
                ));
            }
        }

        Ok(())
    }

    /// Creates a plugin registry with all compiled-in plugins registered.
    fn create_test_registry() -> PluginRegistry {
        let mut registry = PluginRegistry::new();
        crate::plugins::register_all_plugins(&mut registry);
        registry
    }

    #[test]
    fn test_discover_config_files() {
        let config_files = discover_standalone_config_files();

        // We should find at least the default.toml and one environment config
        assert!(
            !config_files.is_empty(),
            "No config files found in config directory"
        );

        // Verify default.toml is present
        let has_default = config_files
            .iter()
            .any(|p| p.file_name().unwrap_or_default() == "default.toml");
        assert!(has_default, "default.toml not found in config files");

        println!("Discovered {} config files:", config_files.len());
        for file in &config_files {
            println!("  - {}", file.display());
        }
    }

    #[test]
    fn test_all_config_files_load_successfully() {
        let config_files = discover_standalone_config_files();
        let mut failures: Vec<String> = Vec::new();

        println!("\n=== Config Loading Validation ===\n");

        for config_path in &config_files {
            let config_name = config_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();

            match load_merged_config(config_path) {
                Ok(settings) => {
                    println!("✓ {config_name} - loaded successfully");
                    println!(
                        "    immutable_store.mode = {}, mutable_store.mode = {}",
                        settings.immutable_store.mode, settings.mutable_store.mode
                    );
                }
                Err(e) => {
                    let msg = format!("✗ {config_name} - FAILED: {e}");
                    println!("{msg}");
                    failures.push(msg);
                }
            }
        }

        if !failures.is_empty() {
            panic!("\n\nConfig loading failures:\n{}\n", failures.join("\n"));
        }

        println!(
            "\n=== All {} config files loaded successfully ===\n",
            config_files.len()
        );
    }

    #[test]
    fn test_all_config_files_have_valid_plugin_configs() {
        let config_files = discover_standalone_config_files();
        let mut failures: Vec<String> = Vec::new();

        println!("\n=== Plugin Configuration Validation ===\n");

        for config_path in &config_files {
            let config_name = config_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();

            let settings = match load_merged_config(config_path) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!("{config_name}: Failed to load - {e}"));
                    continue;
                }
            };

            let mut config_errors: Vec<String> = Vec::new();

            // Validate immutable store configuration
            let immutable_mode = &settings.immutable_store.mode;
            if let Err(e) =
                validate_plugin_config(&settings.plugins, immutable_mode, "immutable_store")
            {
                config_errors.push(format!("immutable_store: {e}"));
            }

            // Handle composite store - check durable tier
            if immutable_mode == "composite"
                && let Some(composite) = &settings.immutable_store.composite
                && let Some(durable) = &composite.durable
                && let Err(e) =
                    validate_plugin_config(&settings.plugins, &durable.mode, "immutable_store")
            {
                config_errors.push(format!("composite.durable: {e}"));
            }

            // Validate mutable store configuration
            let mutable_mode = &settings.mutable_store.mode;
            if let Err(e) = validate_plugin_config(&settings.plugins, mutable_mode, "mutable_store")
            {
                config_errors.push(format!("mutable_store: {e}"));
            }

            // Validate topology configuration
            if let Some(topology) = &settings.topology
                && let Some(plugin_name) = topology.provider.plugin_name()
                && !settings.plugins.contains_key(plugin_name)
            {
                // Check if inline config is available as fallback (only for fixed topology)
                let has_inline = match topology.provider {
                    TopologyProvider::Fixed => topology.fixed.is_some(),
                    TopologyProvider::RotatingIdFixed => topology.rotating_id_fixed.is_some(),
                    TopologyProvider::Composite => topology.composite.is_some(),
                    // Consul requires plugin configuration
                    TopologyProvider::Consul => false,
                    TopologyProvider::None => true,
                };

                if !has_inline {
                    config_errors.push(format!(
                        "topology: Provider '{plugin_name}' requires [plugins.{plugin_name}] configuration"
                    ));
                }
            }

            if config_errors.is_empty() {
                println!("✓ {config_name} - plugin configs valid");
            } else {
                let error_msg = format!(
                    "✗ {} - INVALID:\n    {}",
                    config_name,
                    config_errors.join("\n    ")
                );
                println!("{error_msg}");
                failures.push(error_msg);
            }
        }

        if !failures.is_empty() {
            panic!(
                "\n\nPlugin configuration validation failures:\n{}\n",
                failures.join("\n")
            );
        }

        println!("\n=== All config files have valid plugin configurations ===\n");
    }

    #[test]
    fn test_registered_plugins_match_config_requirements() {
        let registry = create_test_registry();

        println!("\n=== Registered Plugins ===\n");
        println!(
            "Immutable store plugins: {:?}",
            registry.list_immutable_store_plugins()
        );
        println!(
            "Mutable store plugins: {:?}",
            registry.list_mutable_store_plugins()
        );
        println!(
            "Lock store plugins: {:?}",
            registry.list_lock_store_plugins()
        );
        println!("Topology plugins: {:?}", registry.list_topology_plugins());
        println!(
            "\nNote: Core modes (local, composite, remote) are handled directly, not via plugins."
        );
        println!(
            "Note: External plugins (aws, consul) are registered in derived crates (e.g., lore-server-epic)."
        );

        let immutable_plugins = registry.list_immutable_store_plugins();
        let mutable_plugins = registry.list_mutable_store_plugins();
        let topology_plugins = registry.list_topology_plugins();

        // Local stores should NOT be in the plugin list - they are core modes
        // handled directly in the server code, not via the plugin system
        assert!(
            !immutable_plugins.contains(&"local".to_string()),
            "Local immutable store should NOT be registered as a plugin (it's a core mode)"
        );
        assert!(
            !mutable_plugins.contains(&"local".to_string()),
            "Local mutable store should NOT be registered as a plugin (it's a core mode)"
        );

        // Fixed topology is a built-in feature, NOT a plugin.
        assert!(
            !topology_plugins.contains(&"fixed".to_string()),
            "Fixed topology should NOT be registered as a plugin (it's a built-in feature)"
        );
    }

    #[test]
    fn test_plugin_configs_can_be_parsed() {
        let config_files = discover_standalone_config_files();
        let registry = create_test_registry();
        let mut results: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        println!("\n=== Plugin Config Parsing Validation ===\n");
        println!("Note: External plugins (aws, consul) are registered in derived");
        println!("crates (e.g., lore-server-epic). Only registered plugins are validated.\n");

        for config_path in &config_files {
            let start = Instant::now();

            let config_name = config_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();

            let settings = match load_merged_config(config_path) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!("{config_name}: Failed to load - {e}"));
                    continue;
                }
            };

            let mut parse_results: Vec<String> = Vec::new();
            let mut parse_errors: Vec<String> = Vec::new();

            // Test immutable store plugin config parsing
            let immutable_mode = &settings.immutable_store.mode;
            if requires_plugin_config(immutable_mode) {
                if registry
                    .list_immutable_store_plugins()
                    .contains(&immutable_mode.clone())
                {
                    if let Some(plugin_config) = resolve_plugin_config_with_fallback(
                        &settings.plugins,
                        immutable_mode,
                        "immutable_store",
                    ) {
                        match registry
                            .validate_immutable_store_config(immutable_mode, &plugin_config)
                        {
                            Ok(()) => {
                                parse_results.push(format!("immutable_store[{immutable_mode}]: ✓"));
                            }
                            Err(e) => {
                                parse_errors
                                    .push(format!("immutable_store[{immutable_mode}]: {e}"));
                            }
                        }
                    }
                } else {
                    parse_results.push(format!(
                        "immutable_store[{immutable_mode}]: ✓ (external plugin)"
                    ));
                }
            } else {
                parse_results.push(format!("immutable_store[{immutable_mode}]: ✓ (core mode)"));
            }

            // Test composite durable tier
            if immutable_mode == "composite"
                && let Some(composite) = &settings.immutable_store.composite
                && let Some(durable) = &composite.durable
            {
                let durable_mode = &durable.mode;
                if requires_plugin_config(durable_mode) {
                    if registry
                        .list_immutable_store_plugins()
                        .contains(&durable_mode.clone())
                    {
                        if let Some(plugin_config) = resolve_plugin_config_with_fallback(
                            &settings.plugins,
                            durable_mode,
                            "immutable_store",
                        ) {
                            match registry
                                .validate_immutable_store_config(durable_mode, &plugin_config)
                            {
                                Ok(()) => {
                                    parse_results
                                        .push(format!("composite.durable[{durable_mode}]: ✓"));
                                }
                                Err(e) => {
                                    parse_errors
                                        .push(format!("composite.durable[{durable_mode}]: {e}"));
                                }
                            }
                        }
                    } else {
                        parse_results.push(format!(
                            "composite.durable[{durable_mode}]: ✓ (external plugin)"
                        ));
                    }
                }
            }

            // Test mutable store plugin config parsing
            let mutable_mode = &settings.mutable_store.mode;
            if requires_plugin_config(mutable_mode) {
                if registry
                    .list_mutable_store_plugins()
                    .contains(&mutable_mode.clone())
                {
                    if let Some(plugin_config) = resolve_plugin_config_with_fallback(
                        &settings.plugins,
                        mutable_mode,
                        "mutable_store",
                    ) {
                        match registry.validate_mutable_store_config(mutable_mode, &plugin_config) {
                            Ok(()) => {
                                parse_results.push(format!("mutable_store[{mutable_mode}]: ✓"));
                            }
                            Err(e) => {
                                parse_errors.push(format!("mutable_store[{mutable_mode}]: {e}"));
                            }
                        }
                    }
                } else {
                    parse_results.push(format!(
                        "mutable_store[{mutable_mode}]: ✓ (external plugin)"
                    ));
                }
            } else {
                parse_results.push(format!("mutable_store[{mutable_mode}]: ✓ (core mode)"));
            }

            // Test topology plugin config parsing
            if let Some(topology) = &settings.topology
                && let Some(plugin_name) = topology.provider.plugin_name()
                && let Some(plugin_config) = settings.plugins.get(plugin_name)
            {
                if registry
                    .list_topology_plugins()
                    .contains(&plugin_name.to_string())
                {
                    match registry.validate_topology_config(plugin_name, plugin_config) {
                        Ok(()) => {
                            parse_results.push(format!("topology[{plugin_name}]: ✓"));
                        }
                        Err(e) => {
                            let error_msg = e.to_string();
                            let is_config_error = error_msg.contains("configuration error");
                            if is_config_error {
                                let is_expected_env_injected_field =
                                    plugin_name == "consul" && error_msg.contains("address");

                                if is_expected_env_injected_field {
                                    parse_results.push(format!(
                                        "topology[{plugin_name}]: ✓ (address provided via env)"
                                    ));
                                } else {
                                    parse_errors.push(format!("topology[{plugin_name}]: {e}"));
                                }
                            } else {
                                parse_errors.push(format!("topology[{plugin_name}]: {e}"));
                            }
                        }
                    }
                } else {
                    parse_results.push(format!("topology[{plugin_name}]: ✓ (external plugin)"));
                }
            }

            if parse_errors.is_empty() {
                let result = format!("✓ {}\n    {}", config_name, parse_results.join("\n    "));
                println!("{} ({:.2}s)", result, start.elapsed().as_secs_f32());
                results.push(result);
            } else {
                let error_msg = format!(
                    "✗ {}\n    Passed: {}\n    Failed: {}",
                    config_name,
                    parse_results.join(", "),
                    parse_errors.join("\n    ")
                );
                println!("{error_msg}");
                failures.push(error_msg);
            }
        }

        if !failures.is_empty() {
            panic!(
                "\n\nPlugin config parsing failures:\n{}\n",
                failures.join("\n")
            );
        }

        println!("\n=== All plugin configs can be parsed ===\n");
    }

    #[test]
    fn test_default_config_is_local_only() {
        let config_dir = find_config_dir();
        let default_path = config_dir.join("default.toml");

        let settings = load_merged_config(&default_path).expect("default.toml should load");

        // Default config should use local stores (no external dependencies)
        assert_eq!(
            settings.immutable_store.mode, "local",
            "default.toml should use local immutable store"
        );
        assert_eq!(
            settings.mutable_store.mode, "local",
            "default.toml should use local mutable store"
        );

        // Should not have AWS plugin config (or if it does, it's optional)
        // This is intentional - default should work without any cloud services
    }

    #[test]
    fn test_gha_config_is_local_only() {
        let config_dir = find_config_dir();
        let gha_path = config_dir.join("gha.toml");

        if !gha_path.exists() {
            println!("Skipping test - gha.toml not found");
            return;
        }

        let settings = load_merged_config(&gha_path).expect("gha.toml should load");

        // GHA config should use local stores for CI testing
        assert_eq!(
            settings.immutable_store.mode, "local",
            "gha.toml should use local immutable store for CI"
        );
        assert_eq!(
            settings.mutable_store.mode, "local",
            "gha.toml should use local mutable store for CI"
        );
    }

    #[test]
    fn test_production_configs() {
        let config_files = discover_standalone_config_files();

        // Production configs that should use AWS
        let production_configs = [
            "ci.toml",
            "gamedev.toml",
            "benchmark.toml",
            "uefn-live.toml",
            "uefn-canary.toml",
            "uefn-livetesting.toml",
            "live-internal.toml",
        ];

        for config_name in &production_configs {
            let config_path = config_files
                .iter()
                .find(|p| p.file_name().unwrap_or_default().to_string_lossy() == *config_name);

            if let Some(config_path) = config_path {
                let settings = load_merged_config(config_path)
                    .unwrap_or_else(|_| panic!("{config_name} should load"));

                // Production configs should use composite or aws for immutable store
                let immutable_mode = &settings.immutable_store.mode;
                assert!(
                    immutable_mode == "composite" || immutable_mode == "aws",
                    "{config_name} should use composite or aws immutable store, found: {immutable_mode}"
                );

                // Production configs should use aws for mutable store
                assert_eq!(
                    settings.mutable_store.mode, "aws",
                    "{config_name} should use aws mutable store"
                );

                // Should have AWS plugin configuration
                assert!(
                    settings.plugins.contains_key("aws"),
                    "{config_name} should have [plugins.aws] configuration"
                );

                println!("✓ {config_name} correctly uses AWS stores");
            }
        }
    }

    #[test]
    fn test_composite_store_durable_modes_are_valid() {
        let config_files = discover_standalone_config_files();
        let registry = create_test_registry();
        let registered_plugins = registry.list_immutable_store_plugins();

        println!("\n=== Composite Store Durable Mode Validation ===\n");
        println!("Note: External plugin modes (e.g., aws) are accepted without validation.");
        println!("They are validated in the derived crate (e.g., lore-server-epic).\n");

        for config_path in &config_files {
            let config_name = config_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();

            let settings = match load_merged_config(config_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if settings.immutable_store.mode != "composite" {
                continue;
            }

            if let Some(composite) = &settings.immutable_store.composite {
                // Validate local tier mode - must be core or a known plugin
                let local_mode = &composite.local.mode;
                let local_valid = CORE_STORE_MODES.contains(&local_mode.as_str())
                    || registered_plugins.contains(&local_mode.clone());

                // Validate durable tier mode if present
                if let Some(durable) = &composite.durable {
                    let durable_mode = &durable.mode;
                    let durable_valid = CORE_STORE_MODES.contains(&durable_mode.as_str())
                        || registered_plugins.contains(&durable_mode.clone());

                    let local_label = if local_valid {
                        "✓".to_string()
                    } else {
                        "external".to_string()
                    };
                    let durable_label = if durable_valid {
                        "✓".to_string()
                    } else {
                        "external".to_string()
                    };
                    println!(
                        "✓ {config_name} - composite store: local={local_mode} ({local_label}), durable={durable_mode} ({durable_label})"
                    );
                } else {
                    println!("✓ {config_name} - composite store: local={local_mode}, durable=none");
                }
            }
        }
    }

    #[test]
    fn test_config_validation_summary() {
        let config_files = discover_standalone_config_files();
        let registry = create_test_registry();

        println!("\n");
        println!("╔════════════════════════════════════════════════════════════════╗");
        println!("║         Lore Server Configuration Validation Summary            ║");
        println!("╠════════════════════════════════════════════════════════════════╣");
        println!("║                                                                ║");
        println!(
            "║  Config files found: {:>3}                                      ║",
            config_files.len()
        );
        println!("║                                                                ║");
        println!("║  Registered Plugins:                                           ║");
        println!(
            "║    - Immutable stores: {:?}",
            registry.list_immutable_store_plugins()
        );
        println!(
            "║    - Mutable stores:   {:?}",
            registry.list_mutable_store_plugins()
        );
        println!(
            "║    - Lock stores:      {:?}",
            registry.list_lock_store_plugins()
        );
        println!(
            "║    - Topology:         {:?}",
            registry.list_topology_plugins()
        );
        println!("║                                                                ║");
        println!("╠════════════════════════════════════════════════════════════════╣");
        println!("║  Config Files:                                                 ║");

        for config_path in &config_files {
            let config_name = config_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let status = match load_merged_config(config_path) {
                Ok(settings) => {
                    format!(
                        "✓ {} | imm: {}, mut: {}",
                        config_name, settings.immutable_store.mode, settings.mutable_store.mode
                    )
                }
                Err(e) => format!("✗ {config_name} | ERROR: {e}"),
            };
            println!("║  {status}  ");
        }

        println!("║                                                                ║");
        println!("╚════════════════════════════════════════════════════════════════╝");
        println!("\n");
    }
}
