// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::time::Duration;

#[cfg(feature = "test-util")]
pub use MockS3Impl as S3;
#[cfg(not(feature = "test-util"))]
pub use S3Impl as S3;
use bytes::Bytes;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::observe::Observe;
#[cfg(feature = "test-util")]
use mockall::automock;
use opentelemetry::metrics::Histogram;
use tracing::warn;

use crate::aws::error::AwsCallError;
use crate::aws::error::S3ErrorCode;
use crate::aws::observe::observe_aws_operation_callback;
use crate::aws::s3::GetObjectOutput;
use crate::aws::s3::HeadObjectOutput;
use crate::aws::s3::ListObjectVersionsOutput;
use crate::aws::s3::ObjectMetadata;
use crate::aws::s3::S3Client;
use crate::aws::s3::WriteOutput;
use crate::aws_error::AwsError;

/// What a call to this facade can fail with.
///
/// One alias for every operation: the error code is a typed enum, so the operation it came from is
/// not part of the type.
pub type S3Error = AwsError<AwsCallError<S3ErrorCode>>;

#[derive(Clone)]
struct S3InstrumentProvider;

impl InstrumentProvider for S3InstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.aws.s3"
    }
}

#[derive(Clone)]
struct S3Instruments {
    operation_latency_histogram: Histogram<f64>,
    instrument_provider: S3InstrumentProvider,
}

impl S3Instruments {
    fn new(instrument_provider: S3InstrumentProvider) -> Self {
        Self {
            operation_latency_histogram: instrument_provider
                .latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            instrument_provider,
        }
    }
}

impl InstrumentProvider for S3Instruments {
    fn namespace(&self) -> &'static str {
        self.instrument_provider.namespace()
    }
}

pub struct S3Impl {
    client: S3Client,
    instruments: S3Instruments,
    slow_operation_duration: Duration,
}

#[cfg_attr(feature = "test-util", automock)]
impl S3Impl {
    pub fn new(client: S3Client, slow_operation_duration: Duration) -> Self {
        Self {
            client,
            instruments: S3Instruments::new(S3InstrumentProvider {}),
            slow_operation_duration,
        }
    }

    #[tracing::instrument(name = "S3Impl::bucket_exists", skip_all)]
    pub async fn bucket_exists(&self, bucket: &str) -> Result<bool, S3Error> {
        match self
            .client
            .bucket_exists(bucket)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("head_bucket"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
        {
            Ok(exists) => Ok(exists),
            Err(e) => {
                warn!("Failed to check if bucket exists: {e}");
                Err(AwsError::Aws(e))
            }
        }
    }

    #[tracing::instrument(name = "S3Impl::head_object", skip_all)]
    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<HeadObjectOutput, S3Error> {
        self.client
            .head_object(bucket, key)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("head_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    #[tracing::instrument(name = "S3Impl::get_object", skip_all)]
    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<Range<usize>>,
    ) -> Result<GetObjectOutput, S3Error> {
        // Rust ranges are half-open and HTTP ranges are inclusive at both ends; the client does
        // that conversion, so a caller's range means what it says here.
        self.client
            .get_object(bucket, key, range)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("get_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    /// Store an object, optionally attaching object metadata carried as `x-amz-meta-*` headers.
    ///
    /// Object metadata is part of the object version rather than a separate record, so a reader
    /// always observes the metadata that was written with the bytes it is reading. Writing the
    /// two together is what makes them impossible to tear apart.
    ///
    /// The body is [`Bytes`]: callers already hold the payload in a refcounted buffer, and it
    /// reaches the socket without being copied — including across a retry, which costs a refcount.
    #[tracing::instrument(name = "S3Impl::put_object", skip_all)]
    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: Bytes,
        metadata: Option<ObjectMetadata>,
    ) -> Result<WriteOutput, S3Error> {
        self.client
            .put_object(bucket, key, body, metadata.as_ref())
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("put_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    /// Every version of every key under the prefix `key`, across as many pages as the listing
    /// takes.
    ///
    /// `truncated` on the output is set when the listing stopped short of the end.
    pub async fn list_versions(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<ListObjectVersionsOutput, S3Error> {
        self.client
            .list_object_versions(bucket, key)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("list_versions"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }

    /// Delete one version of `key`, or with no version the key itself.
    #[allow(
        clippy::needless_lifetimes,
        reason = "the generated mock cannot express a lifetime elided inside `Option`"
    )]
    #[cfg_attr(
        feature = "test-util",
        allow(
            clippy::ref_option_ref,
            reason = "the generated mock's matchers borrow each argument"
        )
    )]
    #[tracing::instrument(name = "S3Impl::delete_object", skip_all)]
    pub async fn delete_object<'v>(
        &self,
        bucket: &str,
        key: &str,
        version: Option<&'v str>,
    ) -> Result<WriteOutput, S3Error> {
        self.client
            .delete_object(bucket, key, version)
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("delete_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::Aws)
    }
}
