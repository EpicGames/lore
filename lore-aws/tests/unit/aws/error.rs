// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use http::StatusCode;
use lore_aws::aws::error::*;
use lore_aws::aws::item::Item;

#[test]
fn an_unknown_dynamo_code_survives_verbatim_and_is_not_retried() {
    let code = DynamoErrorCode::from_wire("SomeFutureException", StatusCode::BAD_REQUEST);

    assert_eq!(code.as_str(), "SomeFutureException");
    assert_eq!(code.retryable(), Retryable::No);
}

/// A 5xx is a server-side failure whether or not it names a code we know, and the status is the
/// only thing that says so when the body is empty.
#[test]
fn a_bodyless_server_error_is_retryable() {
    let code = DynamoErrorCode::from_wire("", StatusCode::BAD_GATEWAY);

    assert_eq!(code, DynamoErrorCode::InternalServerError);
    assert_eq!(code.retryable(), Retryable::Yes);
}

/// Any other answer without a code is named by its status, so a log line or a metric label never
/// carries an empty code, and it is not retried.
#[test]
fn a_bodyless_client_error_is_named_by_its_status() {
    let code = DynamoErrorCode::from_wire("", StatusCode::FORBIDDEN);

    assert_eq!(code.as_str(), "403");
    assert_eq!(code.retryable(), Retryable::No);
}

#[test]
fn dynamo_throttles_are_retryable_and_decisions_are_not() {
    for code in [
        DynamoErrorCode::ThrottlingException,
        DynamoErrorCode::ProvisionedThroughputExceededException,
        DynamoErrorCode::RequestLimitExceeded,
        DynamoErrorCode::InternalServerError,
        DynamoErrorCode::ServiceUnavailable,
        DynamoErrorCode::TransactionInProgressException,
    ] {
        assert_eq!(code.retryable(), Retryable::Yes, "{code:?} is a throttle");
    }

    for code in [
        DynamoErrorCode::ConditionalCheckFailedException,
        DynamoErrorCode::TransactionCanceledException,
        DynamoErrorCode::ValidationException,
        DynamoErrorCode::ResourceNotFoundException,
        DynamoErrorCode::AccessDeniedException,
    ] {
        assert_eq!(code.retryable(), Retryable::No, "{code:?} is a decision");
    }
}

#[test]
fn a_skewed_clock_asks_for_a_re_sign_rather_than_a_wait() {
    assert_eq!(
        DynamoErrorCode::RequestTimeTooSkewed.retryable(),
        Retryable::ClockSkew
    );
    assert_eq!(
        S3ErrorCode::SignatureDoesNotMatch.retryable(),
        Retryable::ClockSkew
    );
}

/// `HeadObject` reports a missing key as a bare 404, `GetObject` as `NoSuchKey`. Both reach the
/// same store branch, so both must answer `is_not_found`.
#[test]
fn s3_reads_a_bodyless_404_as_not_found() {
    let head = S3ErrorCode::from_wire("", StatusCode::NOT_FOUND);
    let get = S3ErrorCode::from_wire("NoSuchKey", StatusCode::NOT_FOUND);

    assert_eq!(head, S3ErrorCode::NotFound);
    assert!(head.is_not_found());
    assert!(get.is_not_found());
    assert!(get.is_no_such_key());
    assert!(head.is_no_such_key());
}

/// Only a named `ExpiredToken` buys the refresh and re-sign. A HEAD's bare 403 is as often a denied
/// or missing key as an expired session, and a named `AccessDenied` says what it means.
#[test]
fn only_a_named_expired_token_is_worth_a_re_sign() {
    let bodyless = S3ErrorCode::from_wire("", StatusCode::FORBIDDEN);

    assert_eq!(bodyless, S3ErrorCode::Forbidden);
    assert_eq!(bodyless.retryable(), Retryable::No);
    assert!(!bodyless.is_expired_credentials());

    let named = S3ErrorCode::from_wire("AccessDenied", StatusCode::FORBIDDEN);

    assert_eq!(named, S3ErrorCode::AccessDenied);
    assert!(!named.is_expired_credentials());

    let expired = S3ErrorCode::from_wire("ExpiredToken", StatusCode::BAD_REQUEST);

    assert_eq!(expired, S3ErrorCode::ExpiredToken);
    assert!(expired.is_expired_credentials());
}

#[test]
fn throttle_spellings_collapse_onto_one_code() {
    for spelling in ["ThrottlingException", "ThrottledException", "Throttling"] {
        assert_eq!(
            DynamoErrorCode::from_wire(spelling, StatusCode::BAD_REQUEST),
            DynamoErrorCode::ThrottlingException,
            "{spelling} is a throttle"
        );
    }

    for spelling in [
        "SlowDown",
        "TooManyRequests",
        "RequestThrottled",
        "ThrottlingException",
    ] {
        assert_eq!(
            S3ErrorCode::from_wire(spelling, StatusCode::BAD_REQUEST),
            S3ErrorCode::SlowDown,
            "{spelling} is a store shedding"
        );
    }
}

/// A 400 saying the body did not arrive intact is the one 4xx class worth resending unchanged:
/// the checksum and the payload hash are computed from the bytes in hand, so a mismatch is damage
/// on the way rather than a request the store will keep refusing. `MinIO` names a checksum mismatch
/// differently from S3, and both spellings count.
#[test]
fn a_body_that_did_not_arrive_intact_is_retryable() {
    for (wire, expected) in [
        ("BadDigest", S3ErrorCode::BadDigest),
        (
            "XAmzContentSHA256Mismatch",
            S3ErrorCode::XAmzContentSha256Mismatch,
        ),
        (
            "XAmzContentChecksumMismatch",
            S3ErrorCode::XAmzContentChecksumMismatch,
        ),
        ("IncompleteBody", S3ErrorCode::IncompleteBody),
    ] {
        let code = S3ErrorCode::from_wire(wire, StatusCode::BAD_REQUEST);

        assert_eq!(code, expected);
        assert_eq!(code.as_str(), wire, "the wire spelling survives");
        assert_eq!(code.retryable(), Retryable::Yes);
    }
}

/// The error half of every operation's `Result` is on every call's return path, so its size is a
/// property worth holding still.
#[test]
fn a_call_error_stays_small() {
    assert!(
        size_of::<AwsCallError<DynamoErrorCode>>() <= 40,
        "an unboxed service error would put hundreds of bytes on every Result: {}",
        size_of::<AwsCallError<DynamoErrorCode>>()
    );
}

/// A code belongs to a service's answer, and a transport failure is the absence of one.
#[test]
fn a_failure_before_the_service_answered_carries_no_code() {
    let transport: AwsCallError<DynamoErrorCode> =
        AwsCallError::Transport(TransportError::Io("reset".into()));

    assert_eq!(transport.code(), None);
    assert_eq!(AwsCallError::<DynamoErrorCode>::Timeout.code(), None);
}

/// Text the service chose reaches a log line and an error cloned up through the stores. A
/// body-sized one must not.
#[test]
fn service_text_is_bounded_and_stays_text() {
    assert_eq!(service_text("short"), "short");

    let long = "x".repeat(MAX_SERVICE_TEXT * 4);
    assert_eq!(service_text(&long).len(), MAX_SERVICE_TEXT);

    // A multi-byte character straddling the limit must be dropped whole, not split.
    let multibyte = "é".repeat(MAX_SERVICE_TEXT);
    let cut = service_text(&multibyte);
    assert!(cut.len() <= MAX_SERVICE_TEXT);
    assert!(
        cut.as_str().chars().all(|character| character == 'é'),
        "truncation must leave valid text"
    );
}

/// A code goes further than the rest of it: into a metric label. So it is bounded more tightly,
/// and on a character boundary for the same reason.
#[test]
fn an_error_code_is_bounded_more_tightly_than_the_rest() {
    assert_eq!(error_code("ThrottlingException"), "ThrottlingException");

    let long = "X".repeat(MAX_ERROR_CODE * 8);
    assert_eq!(error_code(&long).len(), MAX_ERROR_CODE);

    let multibyte = "é".repeat(MAX_ERROR_CODE);
    let cut = error_code(&multibyte);
    assert!(cut.len() <= MAX_ERROR_CODE);
    assert!(cut.as_str().chars().all(|character| character == 'é'));
}

/// The bound has to reach the code that is *kept*, which is the one a metric is labelled with.
///
/// It applies after the known spellings are matched, so it can only ever shorten a code that was
/// already unrecognised — every real exception name is far inside it.
#[test]
fn an_unknown_code_kept_from_the_wire_is_bounded() {
    let code = DynamoErrorCode::from_wire(&"X".repeat(4_096), StatusCode::BAD_REQUEST);
    assert_eq!(code.as_str().len(), MAX_ERROR_CODE);
    assert_eq!(code.retryable(), Retryable::No);

    let s3 = S3ErrorCode::from_wire(&"X".repeat(4_096), StatusCode::BAD_REQUEST);
    assert_eq!(s3.as_str().len(), MAX_ERROR_CODE);

    let sts = StsErrorCode::from_wire(&"X".repeat(4_096), StatusCode::BAD_REQUEST);
    assert_eq!(sts.as_str().len(), MAX_ERROR_CODE);

    // The longest name either service sends must survive whole, or the label names the wrong thing.
    for name in [
        "ItemCollectionSizeLimitExceededException",
        "ServerSideEncryptionConfigurationNotFoundError",
    ] {
        assert_eq!(
            DynamoErrorCode::from_wire(name, StatusCode::BAD_REQUEST).as_str(),
            name
        );
    }
}

#[test]
fn cancellation_reason_codes_map_to_their_meaning() {
    assert_eq!(
        BatchStatementErrorCode::from("ConditionalCheckFailed"),
        BatchStatementErrorCode::ConditionalCheckFailed
    );
    assert_eq!(
        BatchStatementErrorCode::from("ThrottlingError"),
        BatchStatementErrorCode::ThrottlingError
    );
    assert_eq!(
        BatchStatementErrorCode::from("None"),
        BatchStatementErrorCode::Unknown
    );
}

#[test]
fn payload_accessors_answer_only_for_their_own_payload() {
    let with_item: ServiceError<DynamoErrorCode> = ServiceError {
        status: StatusCode::BAD_REQUEST,
        code: DynamoErrorCode::ConditionalCheckFailedException,
        message: None,
        request_id: None,
        payload: ErrorPayload::Item(Box::new(Item::new())),
    };
    let with_reasons: ServiceError<DynamoErrorCode> = ServiceError {
        status: StatusCode::BAD_REQUEST,
        code: DynamoErrorCode::TransactionCanceledException,
        message: None,
        request_id: None,
        payload: ErrorPayload::CancellationReasons(vec![CancellationReason::default()]),
    };

    assert!(with_item.item().is_some());
    assert!(with_item.cancellation_reasons().is_empty());
    assert!(with_reasons.item().is_none());
    assert_eq!(with_reasons.cancellation_reasons().len(), 1);
}

#[test]
fn only_a_transport_failure_of_the_moment_is_worth_resending() {
    assert!(TransportError::Io("connection reset".to_owned()).is_worth_resending());
    assert!(
        TransportError::Body("the connection dropped mid-body".to_owned()).is_worth_resending()
    );

    assert!(!TransportError::Build("the URL has no host".to_owned()).is_worth_resending());
    assert!(!TransportError::Protocol("the body is not JSON".to_owned()).is_worth_resending());
    assert!(!TransportError::ConnectionLimit.is_worth_resending());
    assert!(!TransportError::ConnectionLimitTimeout.is_worth_resending());
}

/// A request the client's connection limit held back until its deadline is load shed by this
/// process, so a caller backs off from it as from a service that shed it, although it is not resent.
#[test]
fn a_request_held_back_by_the_connection_limit_counts_as_overload() {
    assert!(is_overloaded(&AwsCallError::<DynamoErrorCode>::Transport(
        TransportError::ConnectionLimit
    )));
}

/// A request the client's connection limit kept queued until it ran out of time is load shed by
/// this process too, so a caller backs off from it.
#[test]
fn a_request_kept_queued_by_the_connection_limit_until_it_ran_out_of_time_counts_as_overload() {
    assert!(is_overloaded(&AwsCallError::<DynamoErrorCode>::Transport(
        TransportError::ConnectionLimitTimeout
    )));
}

/// A connection or a read that failed is backpressure. A request that could not be built, or a
/// response this client will not read, fails the same way every time, so it is not.
#[test]
fn only_a_failed_connection_or_read_is_transport_overload() {
    let overloaded =
        |error: TransportError| is_overloaded(&AwsCallError::<DynamoErrorCode>::Transport(error));

    assert!(overloaded(TransportError::Io(
        "connection reset".to_owned()
    )));
    assert!(overloaded(TransportError::Body(
        "the connection dropped".to_owned()
    )));

    assert!(!overloaded(TransportError::Build(
        "the URL has no host".to_owned()
    )));
    assert!(!overloaded(TransportError::Protocol(
        "the body is not JSON".to_owned()
    )));
}
