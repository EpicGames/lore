// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_revision::util::time::RetryPolicy;
use lore_revision::util::time::RetrySettings;
use lore_server::plugins::ImmutableStorePluginFactory;
use lore_server::plugins::LockStorePluginFactory;
use lore_server::plugins::MutableStorePluginFactory;
use lore_server::plugins::PluginRegistry;
use lore_server::plugins::aws::*;
use tokio::runtime::Handle;

const UNPROCESSED_KEYS_RETRY: &str = r#"
    [dynamodb_unprocessed_keys_retry]
    initial_backoff_ms = 20
    max_backoff_ms = 30
    max_attempts = 3
    jitter = 0.0
"#;

/// The wait durations yielded by a retry built from `settings` until its limit is reached.
fn waits(settings: &RetrySettings) -> Vec<Duration> {
    let mut retry = RetryPolicy::from(settings).retry();
    std::iter::from_fn(|| retry.next_interval()).collect()
}

/// The wait durations yielded by [`UNPROCESSED_KEYS_RETRY`], which has no jitter.
const UNPROCESSED_KEYS_WAITS_MS: [u64; 3] = [20, 30, 30];

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
    let config_str = format!("{config_str}{UNPROCESSED_KEYS_RETRY}");

    let config: toml::Value = toml::from_str(&config_str).unwrap();
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
    let retry = plugin_config
        .dynamodb_unprocessed_keys_retry
        .expect("the unprocessed keys retry is configured");
    assert_eq!(
        waits(&retry),
        UNPROCESSED_KEYS_WAITS_MS.map(Duration::from_millis)
    );
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
    assert!(plugin_config.dynamodb_unprocessed_keys_retry.is_none());
}

#[tokio::test]
async fn test_lock_store_config_reads_the_unprocessed_keys_retry() {
    let config_str = format!("dynamodb_table = \"locks\"\n{UNPROCESSED_KEYS_RETRY}");

    let config: toml::Value = toml::from_str(&config_str).unwrap();
    let plugin_config: AwsLockStorePluginConfig = config.try_into().unwrap();

    let retry = plugin_config
        .dynamodb_unprocessed_keys_retry
        .expect("the unprocessed keys retry is configured");
    assert_eq!(
        waits(&retry),
        UNPROCESSED_KEYS_WAITS_MS.map(Duration::from_millis)
    );
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

/// An immutable store config whose shared `http` table sets `shared` as its limit, and whose S3 and
/// `DynamoDB` clients set `s3` and `dynamodb`.
fn immutable_config(
    shared: Option<usize>,
    s3: Option<usize>,
    dynamodb: Option<usize>,
) -> AwsImmutableStorePluginConfig {
    let limit = |key: &str, value: Option<usize>| {
        value.map_or(String::new(), |value| format!("{key} = {value}\n"))
    };
    let text = format!(
        "s3_bucket = \"b\"\n\
         dynamodb_fragments_table = \"f\"\n\
         dynamodb_fragment_state_table = \"s\"\n\
         {}{}\
         [http]\n\
         pool_idle_timeout_seconds = 7\n\
         nodelay = true\n\
         connection_acquire_timeout_millis = 250\n\
         {}",
        limit("s3_max_connections", s3),
        limit("dynamodb_max_connections", dynamodb),
        limit("max_connections", shared),
    );

    toml::from_str(&text).expect("the config parses")
}

mod s3_http {
    use std::num::NonZeroUsize;

    use super::*;

    /// The S3 client's own limit replaces the shared one, and the shared settings are otherwise
    /// kept.
    #[test]
    fn its_own_limit_replaces_the_shared_limit() {
        let s3 = immutable_config(Some(100), Some(40), Some(60)).s3_http();

        assert_eq!(s3.max_connections, NonZeroUsize::new(40));
        assert_eq!(s3.pool_idle_timeout_seconds, 7);
        assert!(s3.nodelay);
        assert_eq!(s3.connection_acquire_timeout_millis, Some(250));
    }

    #[test]
    fn without_its_own_limit_the_shared_limit_applies() {
        let s3 = immutable_config(Some(100), None, Some(60)).s3_http();

        assert_eq!(s3.max_connections, NonZeroUsize::new(100));
    }

    #[test]
    fn its_own_limit_applies_where_none_is_shared() {
        let s3 = immutable_config(None, Some(40), None).s3_http();

        assert_eq!(s3.max_connections, NonZeroUsize::new(40));
    }
}

mod dynamodb_http {
    use std::num::NonZeroUsize;

    use super::*;

    /// The `DynamoDB` client's own limit replaces the shared one, and the shared settings are
    /// otherwise kept.
    #[test]
    fn its_own_limit_replaces_the_shared_limit() {
        let dynamodb = immutable_config(Some(100), Some(40), Some(60)).dynamodb_http();

        assert_eq!(dynamodb.max_connections, NonZeroUsize::new(60));
        assert_eq!(dynamodb.pool_idle_timeout_seconds, 7);
        assert!(dynamodb.nodelay);
        assert_eq!(dynamodb.connection_acquire_timeout_millis, Some(250));
    }

    #[test]
    fn without_its_own_limit_the_shared_limit_applies() {
        let dynamodb = immutable_config(Some(100), Some(40), None).dynamodb_http();

        assert_eq!(dynamodb.max_connections, NonZeroUsize::new(100));
    }

    #[test]
    fn without_any_limit_there_is_none() {
        let dynamodb = immutable_config(None, Some(40), None).dynamodb_http();

        assert_eq!(dynamodb.max_connections, None);
    }
}
