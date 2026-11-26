// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use lore_revision::util::time::RetryPolicy;
use opentelemetry::KeyValue;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::debug;
use url::Url;

use crate::aws::ClientCore;
use crate::aws::configured_endpoint;
use crate::aws::creds::CachedCredentials;
use crate::aws::creds::Credentials;
use crate::aws::creds::ProvideCredentials;
use crate::aws::creds::StaticProvider;
use crate::aws::default_credentials_chain;
use crate::aws::default_endpoint;
use crate::aws::dynamodb::DynamoClient;
use crate::aws::error::TransportError;
use crate::aws::http::HttpClient;
use crate::aws::resolve_region_with_imds;
use crate::aws::s3::S3Client;
use crate::aws_error::AwsError;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::DynamoError;
use crate::dynamodb::default_unprocessed_keys_retry;
use crate::s3::S3;
use crate::s3::S3Error;

#[derive(Debug, Error, PartialEq)]
pub enum AwsClientError<E> {
    #[error("DynamoDB table not found: {0}")]
    DynamoTableNotFound(String),
    #[error("S3 bucket not found: {0}")]
    BucketNotFound(String),
    #[error("AWS error: {0}")]
    Aws(#[from] E),
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
    /// The most connections each client built from these settings has in use at once, or no limit
    /// when unset. Zero is refused when the settings are read.
    ///
    /// Each service client has a pool of its own and takes this many. A request past the limit
    /// waits for a connection to be released, for at most `connection_acquire_timeout_millis` and
    /// never past its operation's deadline, and is shed as a back-off when none is.
    #[serde(default)]
    pub max_connections: Option<NonZeroUsize>,
    /// The longest a request waits for a connection under `max_connections`, in milliseconds, or
    /// until its operation's deadline when unset. Zero sheds a request at once when no connection
    /// is free. Has no effect without `max_connections`.
    #[serde(default)]
    pub connection_acquire_timeout_millis: Option<u64>,
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
        TaskQueueSettings {
            quota_per_second: default_quota_per_second(),
            concurrency_limit: default_concurrency_limit(),
            submission_limit: default_submission_limit(),
        }
    }
}

/// How long an operation gets, retries included.
///
/// Each attempt's exchange is given what remains of this budget, capped at a share of it so that a
/// retry stays possible, and the attempt's deadline also bounds reading its response body. A
/// `GetObject` body is streamed by the caller after the call returns, so it is read under the
/// operation's deadline, and the client does not retry a read that overruns it. With a connection
/// limit, an attempt's wait for a connection may take what remains of the budget, past its share,
/// unless the connection acquire timeout ends it sooner. There is no separate connect timeout.
#[derive(Clone, Copy, Debug)]
pub struct TimeoutConfig {
    operation_timeout: Duration,
}

impl TimeoutConfig {
    pub fn builder() -> TimeoutConfigBuilder {
        TimeoutConfigBuilder::default()
    }

    pub fn operation_timeout(&self) -> Duration {
        self.operation_timeout
    }
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            operation_timeout: Duration::from_millis(crate::default_aws_timeout_millis()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TimeoutConfigBuilder {
    operation_timeout: Option<Duration>,
}

impl TimeoutConfigBuilder {
    pub fn operation_timeout(mut self, timeout: Duration) -> Self {
        self.operation_timeout = Some(timeout);
        self
    }

    pub fn build(self) -> TimeoutConfig {
        TimeoutConfig {
            operation_timeout: self
                .operation_timeout
                .unwrap_or_else(|| Duration::from_millis(crate::default_aws_timeout_millis())),
        }
    }
}

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
            max_connections: None,
            connection_acquire_timeout_millis: None,
        }
    }
}

/// The store a client serves, which labels the time its requests wait for a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    Immutable,
    Mutable,
    Lock,
}

impl StoreKind {
    /// The `store` label value naming this store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Immutable => "immutable",
            Self::Mutable => "mutable",
            Self::Lock => "lock",
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
        AwsClientBuilder(WantsAwsConfig {
            http: settings.clone(),
            store: None,
            credentials: None,
            region: None,
            endpoint: None,
            timeout: TimeoutConfig::default(),
        })
    }
}

pub struct WantsAwsConfig {
    /// Built into a transport with the service it sends to, so a transport that cannot be built is
    /// reported by `build()` rather than by a panic in the middle of the builder chain.
    http: HttpClientSettings,
    /// The store the clients serve, when one is named.
    store: Option<StoreKind>,
    credentials: Option<Arc<dyn ProvideCredentials>>,
    region: Option<String>,
    endpoint: Option<String>,
    timeout: TimeoutConfig,
}

impl AwsClientBuilder<WantsAwsConfig> {
    /// Name the store these clients serve, as the `store` label on the time their requests wait
    /// for a connection.
    pub fn for_store(mut self, store: StoreKind) -> Self {
        self.0.store = Some(store);

        self
    }

    /// Sign with these credentials rather than resolving the default chain.
    pub fn with_static_credentials(mut self, credentials: Credentials) -> Self {
        self.0.credentials = Some(Arc::new(StaticProvider::new(credentials)));

        self
    }

    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.0.region = Some(region.into());

        self
    }

    pub fn maybe_region(mut self, region: Option<String>) -> Self {
        if let Some(region) = region {
            self.0.region = Some(region);
        }

        self
    }

    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.0.endpoint = Some(endpoint.into());

        self
    }

    pub fn maybe_endpoint(mut self, endpoint: Option<String>) -> Self {
        if let Some(endpoint) = endpoint {
            self.0.endpoint = Some(endpoint);
        }

        self
    }

    pub fn with_timeout_config(mut self, timeout_config: TimeoutConfig) -> Self {
        self.0.timeout = timeout_config;

        self
    }

    /// Fix the configuration every service client is then built from.
    pub fn build_config(self) -> AwsClientBuilder<WantsService> {
        AwsClientBuilder(WantsService {
            config: self.0,
            slow_operation_threshold: u64::MAX,
            metric_labels: vec![],
            unprocessed_keys_retry: default_unprocessed_keys_retry(),
        })
    }
}

pub struct WantsService {
    config: WantsAwsConfig,
    slow_operation_threshold: u64,
    metric_labels: Vec<KeyValue>,
    unprocessed_keys_retry: RetryPolicy,
}

impl WantsService {
    /// The pieces every service client shares, resolved once.
    ///
    /// The region is resolved here rather than at first use, so a deployment with no region at all
    /// fails when its client is built, with something to read, instead of on its first request.
    async fn core(&self, service: &'static str) -> Result<Arc<ClientCore>, TransportError> {
        let http = HttpClient::for_service(&self.config.http, service, self.config.store)?;

        let region = resolve_region_with_imds(self.config.region.as_deref())
            .await
            .ok_or_else(|| {
                TransportError::Build(
                    "no AWS region is configured: set the region setting, AWS_REGION or \
                     AWS_DEFAULT_REGION, or on EC2 leave AWS_EC2_METADATA_DISABLED unset so the \
                     instance metadata service can name it"
                        .to_owned(),
                )
            })?;

        let credentials = match &self.config.credentials {
            Some(provider) => Arc::new(CachedCredentials::new(provider.clone())),
            None => default_credentials_chain(&region)?,
        };

        let endpoint = configured_endpoint(self.config.endpoint.as_deref(), service)
            .unwrap_or_else(|| default_endpoint(service, &region));
        let endpoint = Url::parse(&endpoint).map_err(|error| {
            TransportError::Build(format!("endpoint {endpoint:?} is not a URL: {error}"))
        })?;

        Ok(Arc::new(ClientCore::new(
            http,
            credentials,
            Arc::from(region.as_str()),
            endpoint,
            self.config.timeout.operation_timeout(),
            // A request id is only read by the slow-operation warning, so it is captured only when
            // there is a threshold that can fire.
            self.slow_operation_threshold != u64::MAX,
        )))
    }
}

impl AwsClientBuilder<WantsService> {
    pub fn with_slow_operation_threshold(mut self, slow_operation_threshold_millis: u64) -> Self {
        self.0.slow_operation_threshold = slow_operation_threshold_millis;

        self
    }

    /// Sets the backoff a `DynamoDB` client uses to resend each `BatchGetItem` chunk's unprocessed
    /// keys, in place of [`default_unprocessed_keys_retry`]. Its limit counts resends.
    pub fn maybe_unprocessed_keys_retry(mut self, retry: Option<RetryPolicy>) -> Self {
        if let Some(retry) = retry {
            self.0.unprocessed_keys_retry = retry;
        }

        self
    }

    pub fn and_append_metric_labels(mut self, mut additional_labels: Vec<KeyValue>) -> Self {
        self.0.metric_labels.append(&mut additional_labels);

        self
    }

    pub fn dynamodb(self) -> AwsClientBuilder<WantsTables> {
        AwsClientBuilder(WantsTables {
            service: self.0,
            tables: vec![],
        })
    }

    pub fn s3(self) -> AwsClientBuilder<WantsBuckets> {
        self.s3_with_path_style(false)
    }

    pub fn s3_with_path_style(self, force_path_style: bool) -> AwsClientBuilder<WantsBuckets> {
        AwsClientBuilder(WantsBuckets {
            service: self.0,
            force_path_style,
            buckets: vec![],
        })
    }
}

pub struct WantsTables {
    service: WantsService,
    tables: Vec<String>,
}

impl AwsClientBuilder<WantsTables> {
    pub fn ensure_table(mut self, table_name: impl Into<String>) -> AwsClientBuilder<WantsTables> {
        self.0.tables.push(table_name.into());

        self
    }

    pub async fn build(self) -> Result<DynamoDb, AwsClientError<DynamoError>> {
        let service = self.0.service;
        let core = service
            .core("dynamodb")
            .await
            .map_err(|error| AwsClientError::Aws(AwsError::Aws(error.into())))?;
        debug!(core = ?core, "Using DynamoDB core");

        let client = DynamoDb::new(
            DynamoClient::new(core),
            Duration::from_millis(service.slow_operation_threshold),
            service.metric_labels,
            service.unprocessed_keys_retry,
        );

        for table_name in self.0.tables {
            debug!("Checking if table exists: {table_name}");

            match client.table_exists(&Arc::from(table_name.as_str())).await {
                Ok(exists) if !exists => {
                    return Err(AwsClientError::DynamoTableNotFound(table_name));
                }
                Ok(_) => {
                    debug!("Dynamo table exists: {table_name}");
                }
                Err(e) => return Err(AwsClientError::Aws(e)),
            }
        }

        Ok(client)
    }
}

pub struct WantsBuckets {
    service: WantsService,
    force_path_style: bool,
    buckets: Vec<String>,
}

impl AwsClientBuilder<WantsBuckets> {
    pub fn ensure_bucket(mut self, bucket: impl Into<String>) -> AwsClientBuilder<WantsBuckets> {
        self.0.buckets.push(bucket.into());

        self
    }

    pub async fn build(self) -> Result<S3, AwsClientError<S3Error>> {
        let core = self
            .0
            .service
            .core("s3")
            .await
            .map_err(|error| AwsClientError::Aws(AwsError::Aws(error.into())))?;
        debug!(core = ?core, force_path_style = self.0.force_path_style, "Using S3 core");

        let client = S3::new(
            S3Client::new(core, self.0.force_path_style),
            Duration::from_millis(self.0.service.slow_operation_threshold),
        );

        for bucket in self.0.buckets {
            debug!("Checking if bucket exists: {bucket}");

            match client.bucket_exists(&bucket).await {
                Ok(exists) if exists => {
                    debug!("S3 bucket exists: {bucket}");
                }
                Ok(_) => return Err(AwsClientError::BucketNotFound(bucket)),
                Err(e) => return Err(AwsClientError::Aws(e)),
            }
        }

        Ok(client)
    }
}
