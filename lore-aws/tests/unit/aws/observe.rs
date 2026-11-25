// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use http::StatusCode;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::DynamoErrorCode;
use lore_aws::aws::error::ErrorPayload;
use lore_aws::aws::error::RequestId;
use lore_aws::aws::error::ServiceError;
use lore_aws::aws::error::ServiceErrorCode;
use lore_aws::aws::error::TransportError;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::observe::*;
use lore_telemetry::LabelArray;
use opentelemetry::KeyValue;
use smallvec::smallvec;

/// An output that reports a request id, standing in for a real operation's.
#[derive(Debug)]
struct Output(Option<&'static str>);

impl RequestId for Output {
    fn request_id(&self) -> Option<&str> {
        self.0
    }
}

fn service_error(code: DynamoErrorCode) -> AwsCallError<DynamoErrorCode> {
    AwsCallError::Service(Box::new(ServiceError {
        status: StatusCode::BAD_REQUEST,
        code,
        message: None,
        request_id: Some(ByteStr::from_static("REQ-1")),
        payload: ErrorPayload::None,
    }))
}

fn observe(result: &AwsResult<Output, DynamoErrorCode>, elapsed: Duration) -> LabelArray {
    let mut labels: LabelArray = smallvec![];
    let callback =
        observe_aws_operation_callback::<Output, DynamoErrorCode>(Duration::from_millis(10));
    callback(result, &elapsed, &mut labels);
    labels
}

#[test]
fn observes_fast_success() {
    let labels = observe(&Ok(Output(None)), Duration::from_millis(1));

    let expected: LabelArray =
        smallvec![KeyValue::new("success", true), KeyValue::new("slow", false)];
    assert_eq!(labels, expected);
}

#[test]
fn observes_slow_success() {
    let labels = observe(&Ok(Output(Some("REQ-OK"))), Duration::from_millis(100));

    let expected: LabelArray =
        smallvec![KeyValue::new("success", true), KeyValue::new("slow", true)];
    assert_eq!(labels, expected);
}

#[test]
fn observes_slow_error_with_code() {
    let labels = observe(
        &Err(service_error(DynamoErrorCode::ValidationException)),
        Duration::from_millis(100),
    );

    let expected: LabelArray = smallvec![
        KeyValue::new("success", false),
        KeyValue::new("aws_code", "ValidationException"),
        KeyValue::new("slow", true),
    ];
    assert_eq!(labels, expected);
}

#[test]
fn observes_fast_error_with_code() {
    let labels = observe(
        &Err(service_error(DynamoErrorCode::ThrottlingException)),
        Duration::from_millis(1),
    );

    let expected: LabelArray = smallvec![
        KeyValue::new("success", false),
        KeyValue::new("aws_code", "ThrottlingException"),
        KeyValue::new("slow", false),
    ];
    assert_eq!(labels, expected);
}

/// A known code labels the metric without allocating.
///
/// Every failed call is labelled, and a throttle is the failure this client sees most, so a copy
/// per failure is a copy on exactly the path under load. The label points at the same literal the
/// code itself names; an owned copy could not.
#[test]
fn a_known_code_labels_without_copying() {
    let labels = observe(
        &Err(service_error(DynamoErrorCode::ThrottlingException)),
        Duration::from_millis(1),
    );

    let opentelemetry::Value::String(labelled) = &labels
        .iter()
        .find(|label| label.key.as_str() == "aws_code")
        .expect("a failure is labelled with its code")
        .value
    else {
        panic!("a code labels as text");
    };

    assert_eq!(
        labelled.as_str().as_ptr(),
        DynamoErrorCode::ThrottlingException
            .as_static_str()
            .expect("a known code is a literal")
            .as_ptr(),
        "the label copied the code instead of borrowing the literal"
    );
}

/// An unknown code is labelled with what the service actually said, so a new exception shows up in
/// the metric rather than collapsing into "unknown".
#[test]
fn observes_an_unknown_service_code_verbatim() {
    let labels = observe(
        &Err(service_error(DynamoErrorCode::from_wire(
            "SomeFutureException",
            StatusCode::BAD_REQUEST,
        ))),
        Duration::from_millis(1),
    );

    assert!(
        labels.contains(&KeyValue::new("aws_code", "SomeFutureException")),
        "{labels:?}"
    );
}

#[test]
fn observes_a_timeout_by_kind() {
    let labels = observe(
        &Err(AwsCallError::<DynamoErrorCode>::Timeout),
        Duration::from_millis(1),
    );

    let expected: LabelArray = smallvec![
        KeyValue::new("success", false),
        KeyValue::new("aws_code", "Timeout"),
        KeyValue::new("slow", false),
    ];
    assert_eq!(labels, expected);
}

#[test]
fn observes_each_transport_failure_by_kind() {
    for (error, expected) in [
        (TransportError::Io("reset".into()), "TransportIo"),
        (TransportError::Body("short read".into()), "TransportBody"),
        (TransportError::Build("bad url".into()), "TransportBuild"),
        (
            TransportError::Protocol("truncated".into()),
            "TransportProtocol",
        ),
        (TransportError::ConnectionLimit, "ConnectionLimit"),
        (
            TransportError::ConnectionLimitTimeout,
            "ConnectionLimitTimeout",
        ),
    ] {
        let labels = observe(
            &Err(AwsCallError::<DynamoErrorCode>::Transport(error)),
            Duration::from_millis(1),
        );

        assert!(
            labels.contains(&KeyValue::new("aws_code", expected)),
            "expected {expected}, got {labels:?}"
        );
    }
}

#[test]
fn observes_a_credential_failure_by_kind() {
    let labels = observe(
        &Err(AwsCallError::<DynamoErrorCode>::Credentials {
            detail: "no provider".into(),
            transient: false,
        }),
        Duration::from_millis(1),
    );

    assert!(
        labels.contains(&KeyValue::new("aws_code", "Credentials")),
        "{labels:?}"
    );
}

/// A credential provider that could not be asked is labelled apart from one that answered, so a
/// dashboard separates a shed or unreachable token endpoint from a misconfigured deployment.
#[test]
fn observes_an_unavailable_credential_provider_apart() {
    let labels = observe(
        &Err(AwsCallError::<DynamoErrorCode>::Credentials {
            detail: "STS answered 400 Throttling: slow down".into(),
            transient: true,
        }),
        Duration::from_millis(1),
    );

    assert!(
        labels.contains(&KeyValue::new("aws_code", "CredentialsTransient")),
        "{labels:?}"
    );
}
