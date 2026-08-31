// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![cfg_attr(test, allow(clippy::result_large_err))]

use std::fmt::Debug;
use std::time::Duration;

use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_types::request_id::RequestId;
use lore_telemetry::LabelArray;
use lore_telemetry::observe::observe_result;
use opentelemetry::KeyValue;
use tracing::warn;

pub mod aws_error;
pub mod clients;
pub mod dynamodb;
pub mod net_http_client;
pub mod s3;
pub mod store;
pub mod telemetry;

pub type SdkError<E> = ::aws_smithy_runtime_api::client::result::SdkError<
    E,
    ::aws_smithy_runtime_api::client::orchestrator::HttpResponse,
>;
pub type AwsResult<T, E> = Result<T, SdkError<E>>;

fn default_aws_timeout_millis() -> u64 {
    5_000
}

const REQUEST_ID_FIELD: &str = "request_id";
const ELAPSED_MS_FIELD: &str = "elapsed_ms";

const AWS_ERROR_CODE_LABEL_KEY: &str = "aws_code";

pub fn observe_aws_operation_callback<T, E>(
    slow_threshold: Duration,
) -> impl Fn(&AwsResult<T, E>, &Duration, &mut LabelArray) + Copy
where
    T: aws_types::request_id::RequestId + Debug,
    E: aws_types::request_id::RequestId + ProvideErrorMetadata + Debug,
{
    move |result: &AwsResult<T, E>, elapsed: &Duration, labels: &mut LabelArray| {
        // base observability
        observe_result(result, elapsed, labels);

        // error code label
        if let Err(e) = &result {
            let label = if let Some(unwrapped_code) = e.code() {
                KeyValue::new(AWS_ERROR_CODE_LABEL_KEY, unwrapped_code.to_string())
            } else {
                // Our wrappers react off Service Error codes. If it isn't a service error
                // i.e. it doesn't have ErrorMeta, then we should log it for further investigation
                // with TAMs
                warn!(
                    {ELAPSED_MS_FIELD} = elapsed.as_millis(),
                    {REQUEST_ID_FIELD} = e.request_id(),
                    error = ?e,
                    "AWS non-ServiceError"
                );

                let label_value = match e {
                    SdkError::TimeoutError(_) => "SdkTimeoutError",
                    SdkError::DispatchFailure(dispatch) => {
                        if dispatch.is_io() {
                            "DispatchFailure_IO"
                        } else if dispatch.is_timeout() {
                            "DispatchFailure_Timeout"
                        } else if dispatch.is_other() {
                            "DispatchFailure_Other"
                        } else {
                            "DispatchFailure"
                        }
                    }
                    _ => "<unknown>",
                };
                KeyValue::new(AWS_ERROR_CODE_LABEL_KEY, label_value)
            };
            labels.push(label);
        }

        // slow label + tracing
        let is_slow = if *elapsed > slow_threshold {
            let request_id = match &result {
                Ok(r) => r.request_id(),
                Err(e) => e.request_id(),
            }
            .unwrap_or("<unknown>");

            warn!(
                {ELAPSED_MS_FIELD} = elapsed.as_millis(),
                slow_threshold = ?slow_threshold,
                {REQUEST_ID_FIELD} = request_id,
                "Operation execution time exceeded slow operation threshold"
            );

            true
        } else {
            false
        };
        labels.push(KeyValue::new("slow", is_slow));
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_dynamodb::operation::get_item::GetItemError;
    use aws_sdk_dynamodb::operation::get_item::GetItemOutput;
    use aws_sdk_dynamodb::types::error::InternalServerError;
    use aws_sdk_s3::config::http::HttpResponse;
    use aws_sdk_s3::error::ErrorMetadata;
    use aws_smithy_runtime_api::client::result::ConnectorError;
    use aws_smithy_runtime_api::client::result::DispatchFailure;
    use aws_smithy_runtime_api::client::result::ServiceError;
    use aws_smithy_types::body::SdkBody;
    use smallvec::smallvec;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum StubError {
        #[error("Stub")]
        Stub,
    }

    #[test]
    fn observes_fast_success() {
        let output = GetItemOutput::builder().build();
        let elapsed = Duration::from_millis(1);
        let mut labels: LabelArray = smallvec![];
        let callback = observe_aws_operation_callback::<GetItemOutput, GetItemError>(
            Duration::from_millis(10),
        );

        callback(&Ok(output), &elapsed, &mut labels);
        let expected: LabelArray =
            smallvec![KeyValue::new("success", true), KeyValue::new("slow", false)];
        assert_eq!(labels, expected);
    }

    #[test]
    fn observes_slow_success() {
        let output = GetItemOutput::builder().build();
        let elapsed = Duration::from_millis(100);
        let mut labels: LabelArray = smallvec![];
        let callback = observe_aws_operation_callback::<GetItemOutput, GetItemError>(
            Duration::from_millis(10),
        );

        callback(&Ok(output), &elapsed, &mut labels);
        let expected: LabelArray =
            smallvec![KeyValue::new("success", true), KeyValue::new("slow", true)];

        assert_eq!(labels, expected);
    }

    #[test]
    fn observes_slow_error_with_code() {
        let output = SdkError::ServiceError(
            ServiceError::builder()
                .source(GetItemError::InternalServerError(
                    InternalServerError::builder()
                        .meta(ErrorMetadata::builder().code("my-code").build())
                        .build(),
                ))
                .raw(HttpResponse::new(500.try_into().unwrap(), SdkBody::empty()))
                .build(),
        );
        let elapsed = Duration::from_millis(100);
        let mut labels: LabelArray = smallvec![];
        let callback = observe_aws_operation_callback::<GetItemOutput, GetItemError>(
            Duration::from_millis(10),
        );

        callback(&Err(output), &elapsed, &mut labels);
        let expected: LabelArray = smallvec![
            KeyValue::new("success", false),
            KeyValue::new("aws_code", "my-code"),
            KeyValue::new("slow", true),
        ];
        assert_eq!(labels, expected);
    }

    #[test]
    fn observes_fast_error_with_code() {
        let output = SdkError::ServiceError(
            ServiceError::builder()
                .source(GetItemError::InternalServerError(
                    InternalServerError::builder()
                        .meta(ErrorMetadata::builder().code("my-code").build())
                        .build(),
                ))
                .raw(HttpResponse::new(500.try_into().unwrap(), SdkBody::empty()))
                .build(),
        );
        let elapsed = Duration::from_millis(1);
        let mut labels: LabelArray = smallvec![];
        let callback = observe_aws_operation_callback::<GetItemOutput, GetItemError>(
            Duration::from_millis(10),
        );

        callback(&Err(output), &elapsed, &mut labels);
        let expected: LabelArray = smallvec![
            KeyValue::new("success", false),
            KeyValue::new("aws_code", "my-code"),
            KeyValue::new("slow", false),
        ];
        assert_eq!(labels, expected);
    }

    #[test]
    fn observes_error_without_code() {
        let output = SdkError::ServiceError(
            ServiceError::builder()
                .source(GetItemError::InternalServerError(
                    InternalServerError::builder()
                        .meta(ErrorMetadata::builder().build())
                        .build(),
                ))
                .raw(HttpResponse::new(500.try_into().unwrap(), SdkBody::empty()))
                .build(),
        );
        let elapsed = Duration::from_millis(1);
        let mut labels: LabelArray = smallvec![];
        let callback = observe_aws_operation_callback::<GetItemOutput, GetItemError>(
            Duration::from_millis(10),
        );

        callback(&Err(output), &elapsed, &mut labels);

        let expected: LabelArray = smallvec![
            KeyValue::new("success", false),
            KeyValue::new("aws_code", "<unknown>"),
            KeyValue::new("slow", false),
        ];
        assert_eq!(labels, expected);
    }

    #[test]
    fn observes_timeout_error() {
        let output = SdkError::TimeoutError(
            aws_smithy_runtime_api::client::result::TimeoutError::builder()
                .source(Box::new(StubError::Stub))
                .build(),
        );
        let elapsed = Duration::from_millis(1);
        let mut labels: LabelArray = smallvec![];
        let callback = observe_aws_operation_callback::<GetItemOutput, GetItemError>(
            Duration::from_millis(10),
        );

        callback(&Err(output), &elapsed, &mut labels);

        let expected: LabelArray = smallvec![
            KeyValue::new("success", false),
            KeyValue::new("aws_code", "SdkTimeoutError"),
            KeyValue::new("slow", false),
        ];
        assert_eq!(labels, expected);
    }

    #[test]
    fn observes_dispatch_timeout_error() {
        let output = SdkError::DispatchFailure(
            DispatchFailure::builder()
                // source is irrelevant for the test
                .source(ConnectorError::timeout(Box::new(StubError::Stub)))
                .build(),
        );
        let elapsed = Duration::from_millis(1);
        let mut labels: LabelArray = smallvec![];
        let callback = observe_aws_operation_callback::<GetItemOutput, GetItemError>(
            Duration::from_millis(10),
        );

        callback(&Err(output), &elapsed, &mut labels);

        let expected: LabelArray = smallvec![
            KeyValue::new("success", false),
            KeyValue::new("aws_code", "DispatchFailure_Timeout"),
            KeyValue::new("slow", false),
        ];
        assert_eq!(labels, expected);
    }
}
