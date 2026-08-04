// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::time::Duration;

#[cfg(test)]
pub use MockS3Impl as S3;
#[cfg(not(test))]
pub use S3Impl as S3;
use aws_sdk_s3 as s3;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::delete_object::DeleteObjectError;
use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::operation::head_bucket::HeadBucketError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectOutput;
use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsError;
use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsOutput;
use aws_sdk_s3::operation::put_object::PutObjectError;
use aws_sdk_s3::operation::put_object::PutObjectOutput;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::observe::Observe;
#[cfg(test)]
use mockall::automock;
use opentelemetry::metrics::Histogram;
use s3::operation::list_objects_v2::ListObjectsV2Error;
use s3::operation::list_objects_v2::ListObjectsV2Output;
use tracing::warn;

use crate::aws_error::AwsError;
use crate::observe_aws_operation_callback;

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
    client: s3::Client,
    instruments: S3Instruments,
    slow_operation_duration: Duration,
}

#[cfg_attr(test, automock)]
impl S3Impl {
    pub fn new(client: s3::Client, slow_operation_duration: Duration) -> Self {
        Self {
            client,
            instruments: S3Instruments::new(S3InstrumentProvider {}),
            slow_operation_duration,
        }
    }

    #[tracing::instrument(name = "S3Impl::bucket_exists", skip_all)]
    pub async fn bucket_exists(
        &self,
        bucket: String,
    ) -> Result<bool, AwsError<SdkError<HeadBucketError>>> {
        match self
            .client
            .head_bucket()
            .bucket(bucket)
            .send()
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
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(err)) if err.err().is_not_found() => Ok(false),
            Err(e) => {
                warn!("Failed to check if bucket exists: {e}");
                Err(AwsError::AwsSdkError(e))
            }
        }
    }

    #[tracing::instrument(name = "S3Impl::list_objects", skip_all)]
    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        max_keys: Option<i32>,
    ) -> Result<ListObjectsV2Output, AwsError<SdkError<ListObjectsV2Error>>> {
        let mut request = self.client.list_objects_v2().bucket(bucket).prefix(prefix);

        if let Some(m) = max_keys {
            request = request.max_keys(m);
        }

        request
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("list_objects_v2"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
    }

    #[tracing::instrument(name = "S3Impl::head_object", skip_all)]
    pub async fn head_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<HeadObjectOutput, AwsError<SdkError<HeadObjectError>>> {
        self.client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("head_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
    }

    #[tracing::instrument(name = "S3Impl::get_object", skip_all)]
    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<Range<usize>>,
    ) -> Result<GetObjectOutput, AwsError<SdkError<GetObjectError>>> {
        let mut request = self.client.get_object().bucket(bucket).key(key);

        if let Some(r) = range {
            // Rust ranges are half-open (bounded inclusively on the lower bound, but exclusively on
            // the upper), whereas HTTP ranges are inclusive on both bounds, in order to not fetch
            // an extra byte we subtract 1 from the upper bound.
            request = request.range(format!("bytes={0}-{1}", r.start, r.end.saturating_sub(1)));
        }

        request
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("get_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
    }

    #[tracing::instrument(name = "S3Impl::presign_get_object", skip_all)]
    pub async fn presign_get_object(
        &self,
        bucket: &str,
        key: &str,
        expires_in: Duration,
    ) -> Result<String, String> {
        let presigning_config = PresigningConfig::expires_in(expires_in)
            .map_err(|err| format!("failed to build S3 presigning config: {err}"))?;

        self.client
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(presigning_config)
            .await
            .map(|request| request.uri().to_string())
            .map_err(|err| format!("failed to presign S3 get object: {err:?}"))
    }

    #[tracing::instrument(name = "S3Impl::put_object", skip_all)]
    pub async fn put_object<T>(
        &self,
        bucket: &str,
        key: &str,
        body: T,
    ) -> Result<PutObjectOutput, AwsError<SdkError<PutObjectError>>>
    where
        T: Into<Vec<u8>> + 'static,
    {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(Into::<Vec<u8>>::into(body)))
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("put_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
    }

    pub async fn list_versions(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<ListObjectVersionsOutput, AwsError<SdkError<ListObjectVersionsError>>> {
        self.client
            .list_object_versions()
            .bucket(bucket)
            .prefix(key)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("list_versions"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
    }

    #[tracing::instrument(name = "S3Impl::delete_object", skip_all)]
    pub async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
        version: Option<String>,
    ) -> Result<DeleteObjectOutput, AwsError<SdkError<DeleteObjectError>>> {
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .set_version_id(version)
            .send()
            .observe(
                self.instruments.operation_latency_histogram.clone(),
                self.instruments
                    .instrument_provider
                    .get_labels_for_operation_context("delete_object"),
                observe_aws_operation_callback(self.slow_operation_duration),
            )
            .await
            .output
            .map_err(AwsError::AwsSdkError)
    }

    pub fn sdk_client(&self) -> &s3::Client {
        &self.client
    }
}
