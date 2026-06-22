// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use aws_config::meta::region::ProvideRegion;
use aws_sdk_dynamodb::config::ProvideCredentials;
use aws_sdk_dynamodb::operation::describe_table::DescribeTableError;
use aws_sdk_s3::operation::head_bucket::HeadBucketError;
use aws_smithy_http_client::Builder as HttpClientBuilder;
use aws_smithy_http_client::Connector;
use aws_smithy_http_client::tls;
use aws_smithy_http_client::tls::rustls_provider::CryptoMode;
use aws_smithy_runtime_api::client::behavior_version::BehaviorVersion;
use opentelemetry::KeyValue;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::debug;
use tracing::warn;

use crate::aws_error::AwsError;
use crate::dynamodb::DynamoDb;
use crate::s3::S3;

#[derive(Debug, Error, PartialEq)]
pub enum AwsClientError<E> {
    #[error("DynamoDB table not found: {0}")]
    DynamoTableNotFound(String),
    #[error("S3 bucket not found: {0}")]
    BucketNotFound(String),
    #[error("AWS SDK error: {0:?}")]
    SdkError(#[from] E),
    #[error("Unknown error")]
    Unknown,
}

pub const DEFAULT_POOL_IDLE_TIMEOUT_SECONDS: u64 = 30;

fn default_idle_timeout() -> u64 {
    DEFAULT_POOL_IDLE_TIMEOUT_SECONDS
}

#[derive(Clone, Debug, Deserialize)]
pub struct HttpClientSettings {
    #[serde(default = "default_idle_timeout")]
    pub pool_idle_timeout_seconds: u64,
    #[serde(default)]
    pub nodelay: bool,
}

fn default_quota_per_second() -> u32 {
    u32::MAX
}

fn default_concurrency_limit() -> usize {
    Semaphore::MAX_PERMITS
}

fn default_submission_limit() -> usize {
    Semaphore::MAX_PERMITS
}

#[derive(Clone, Debug, Deserialize)]
pub struct TaskQueueSettings {
    #[serde(default = "default_quota_per_second")]
    pub quota_per_second: u32,
    #[serde(default = "default_concurrency_limit")]
    pub concurrency_limit: usize,
    #[serde(default = "default_submission_limit")]
    pub submission_limit: usize,
}

impl Default for TaskQueueSettings {
    fn default() -> Self {
        Self {
            quota_per_second: default_quota_per_second(),
            concurrency_limit: default_concurrency_limit(),
            submission_limit: default_submission_limit(),
        }
    }
}

pub type TimeoutConfig = aws_config::timeout::TimeoutConfig;

#[derive(Clone, Debug, Deserialize)]
pub struct AwsSettings {
    pub http: Option<HttpClientSettings>,
    pub task_queue: Option<TaskQueueSettings>,
}

impl Default for HttpClientSettings {
    fn default() -> Self {
        Self {
            pool_idle_timeout_seconds: DEFAULT_POOL_IDLE_TIMEOUT_SECONDS,
            nodelay: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct AwsClientBuilder<State>(State);

pub struct WantsHttpConfig(());

impl AwsClientBuilder<WantsHttpConfig> {
    pub fn builder() -> Self {
        Self(WantsHttpConfig(()))
    }

    pub fn with_http_settings(
        self,
        settings: &HttpClientSettings,
    ) -> AwsClientBuilder<WantsAwsConfig> {
        let nodelay = settings.nodelay;
        let http_client = HttpClientBuilder::new()
            .pool_idle_timeout(Duration::from_secs(settings.pool_idle_timeout_seconds))
            .build_with_connector_fn(move |_, _| {
                Connector::builder()
                    .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
                    .enable_tcp_nodelay(nodelay)
                    .build()
            });

        AwsClientBuilder(WantsAwsConfig {
            config: aws_config::defaults(BehaviorVersion::latest())
                .http_client(http_client)
                // We default to disabling HTTP connect timeouts for the time being, hopefully once
                // we sort out whatever is causing our mysterious network latency we can remove
                // this. Note: this is override-able in the next phase of the client builder
                // typestate settings.
                .timeout_config(TimeoutConfig::builder().disable_connect_timeout().build()),
        })
    }
}

pub struct WantsAwsConfig {
    config: aws_config::ConfigLoader,
}

impl AwsClientBuilder<WantsAwsConfig> {
    pub fn with_credentials_provider(
        mut self,
        credentials_provider: impl ProvideCredentials + 'static,
    ) -> Self {
        self.0.config = self.0.config.credentials_provider(credentials_provider);

        self
    }

    pub fn region(mut self, region: impl ProvideRegion + 'static) -> Self {
        self.0.config = self.0.config.region(region);

        self
    }

    pub fn maybe_region(mut self, region: Option<String>) -> Self {
        if let Some(region) = region {
            let region = aws_types::region::Region::new(region);
            self.0.config = self.0.config.region(region);
        }

        self
    }

    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.0.config = self.0.config.endpoint_url(endpoint.into());

        self
    }

    pub fn maybe_endpoint(mut self, endpoint: Option<String>) -> Self {
        if let Some(endpoint) = endpoint {
            self.0.config = self.0.config.endpoint_url(endpoint);
        }

        self
    }

    pub fn with_timeout_config(mut self, timeout_config: TimeoutConfig) -> Self {
        self.0.config = self.0.config.timeout_config(timeout_config);

        self
    }

    pub async fn build_config(self) -> AwsClientBuilder<WantsService> {
        AwsClientBuilder(WantsService {
            config: self.0.config.load().await,
            slow_operation_threshold: u64::MAX,
            metric_labels: vec![],
        })
    }
}

pub struct WantsService {
    config: aws_config::SdkConfig,
    slow_operation_threshold: u64,
    metric_labels: Vec<KeyValue>,
}

impl AwsClientBuilder<WantsService> {
    pub fn with_slow_operation_threshold(mut self, slow_operation_threshold_millis: u64) -> Self {
        self.0.slow_operation_threshold = slow_operation_threshold_millis;

        self
    }

    pub fn and_append_metric_labels(mut self, mut additional_labels: Vec<KeyValue>) -> Self {
        self.0.metric_labels.append(&mut additional_labels);

        self
    }

    pub fn dynamodb(self) -> AwsClientBuilder<WantsTables> {
        let ddb_config = aws_sdk_dynamodb::config::Builder::from(&self.0.config).build();
        debug!("Using dynamodb config: {ddb_config:?}");

        AwsClientBuilder(WantsTables {
            client: DynamoDb::new(
                aws_sdk_dynamodb::Client::from_conf(ddb_config),
                Duration::from_millis(self.0.slow_operation_threshold),
                self.0.metric_labels,
            ),
            tables: vec![],
        })
    }

    pub fn s3(self) -> AwsClientBuilder<WantsBuckets> {
        self.s3_with_path_style(false)
    }

    pub fn s3_with_path_style(self, force_path_style: bool) -> AwsClientBuilder<WantsBuckets> {
        let s3_config = aws_sdk_s3::config::Builder::from(&self.0.config)
            .force_path_style(force_path_style)
            .build();
        debug!("Using S3 config: {s3_config:?}");

        AwsClientBuilder(WantsBuckets {
            client: S3::new(
                aws_sdk_s3::Client::from_conf(s3_config),
                Duration::from_millis(self.0.slow_operation_threshold),
            ),
            buckets: vec![],
        })
    }
}

pub struct WantsTables {
    client: DynamoDb,
    tables: Vec<String>,
}

impl AwsClientBuilder<WantsTables> {
    pub fn ensure_table(mut self, table_name: impl Into<String>) -> Self {
        self.0.tables.push(table_name.into());

        self
    }

    pub async fn build(
        self,
    ) -> Result<DynamoDb, AwsClientError<aws_sdk_dynamodb::error::SdkError<DescribeTableError>>>
    {
        for table_name in self.0.tables {
            debug!("Checking if table exists: {table_name}");

            match self
                .0
                .client
                .table_exists(&Arc::from(table_name.as_str()))
                .await
            {
                Ok(exists) if !exists => {
                    return Err(AwsClientError::DynamoTableNotFound(table_name));
                }
                Ok(_) => {
                    debug!("Dynamo table exists: {table_name}");
                }
                Err(AwsError::AwsSdkError(e)) => return Err(e.into()),
                Err(e) => {
                    warn!("Unknown error while checking if table exists: {e:?}");
                    return Err(AwsClientError::Unknown);
                }
            }
        }

        Ok(self.0.client)
    }
}

pub struct WantsBuckets {
    client: S3,
    buckets: Vec<String>,
}

impl AwsClientBuilder<WantsBuckets> {
    pub fn ensure_bucket(mut self, bucket: impl Into<String>) -> Self {
        self.0.buckets.push(bucket.into());

        self
    }

    pub async fn build(
        self,
    ) -> Result<S3, AwsClientError<aws_sdk_s3::error::SdkError<HeadBucketError>>> {
        for bucket in self.0.buckets {
            debug!("Checking if bucket exists: {bucket}");

            match self.0.client.bucket_exists(bucket.clone()).await {
                Ok(exists) if exists => {
                    debug!("S3 bucket exists: {bucket}");
                }
                Ok(_) => return Err(AwsClientError::BucketNotFound(bucket)),
                Err(AwsError::AwsSdkError(e)) => return Err(e.into()),
                Err(e) => {
                    warn!("Unknown error while checking if bucket exists: {e:?}");
                    return Err(AwsClientError::Unknown);
                }
            }
        }

        Ok(self.0.client)
    }
}
