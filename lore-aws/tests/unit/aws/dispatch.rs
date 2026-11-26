// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! End-to-end tests for [`ClientCore`]'s attempt loop, against a scripted local service.
//!
//! Everything below the loop has its own unit tests. What only shows up here is how they compose:
//! that a request is actually signed with what it carries, that a dropped connection is resent
//! while a throttle or a decision goes back to the caller, that a skewed clock recovers, that a
//! spent budget reports a timeout, and that a retried body is byte-identical — which is what makes
//! a transaction's idempotency token worth having.
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use lore_aws::aws::ClientCore;
use lore_aws::aws::creds::CachedCredentials;
use lore_aws::aws::creds::Credentials;
use lore_aws::aws::creds::CredentialsError;
use lore_aws::aws::creds::ProvideCredentials;
use lore_aws::aws::creds::StaticProvider;
use lore_aws::aws::dynamodb::DynamoClient;
use lore_aws::aws::dynamodb::DynamoProtocol;
use lore_aws::aws::dynamodb::SINGLE_RESPONSE_CAP;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::DynamoErrorCode;
use lore_aws::aws::error::TransportError;
use lore_aws::aws::error::is_overloaded;
use lore_aws::aws::http::HttpClient;
use lore_aws::aws::http::HttpRequest;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::sign::SignScratch;
use lore_aws::aws::sign::SigningContext;
use lore_aws::aws::sign::SigningKeyCache;
use lore_aws::aws::sign::payload_sha256;
use lore_aws::aws::sign::sign_request;
use lore_aws::clients::HttpClientSettings;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::timestamp;
use time::OffsetDateTime;
use time::SignedDuration;
use time::UtcDateTime;
use time::format_description::well_known::Rfc3339;

use crate::aws::test_server::RecordedRequest;
use crate::aws::test_server::StubResponse;
use crate::aws::test_server::StubServer;

/// A client pointed at `server`, with a short budget so a timeout test does not take five seconds.
fn client_for(server: &StubServer, timeout: Duration) -> DynamoClient {
    client_with_credentials(
        server,
        timeout,
        Arc::new(StaticProvider::new(Credentials::new(
            "AKIDTEST",
            "secret-for-tests",
            None,
            None,
        ))),
    )
}

fn client_with_credentials(
    server: &StubServer,
    timeout: Duration,
    provider: Arc<dyn ProvideCredentials>,
) -> DynamoClient {
    let http = HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: None,
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds");

    client_over(http, server, timeout, provider)
}

/// A client pointed at `server` that sends through `http`.
fn client_over(
    http: HttpClient,
    server: &StubServer,
    timeout: Duration,
    provider: Arc<dyn ProvideCredentials>,
) -> DynamoClient {
    DynamoClient::new(Arc::new(ClientCore::new(
        http,
        Arc::new(CachedCredentials::new(provider)),
        Arc::from("eu-west-1"),
        server.endpoint(),
        timeout,
        false,
    )))
}

fn key() -> Item {
    let mut key = Item::new();
    key.insert(
        ByteStr::from_static("hash"),
        AttributeValue::blob([7u8; 32]),
    );
    key
}

/// The `x-amz-date` a request was signed with, read as the RFC 3339 time its digits spell.
fn signed_at(request: &RecordedRequest) -> UtcDateTime {
    let raw = request
        .header("x-amz-date")
        .expect("every signed request carries x-amz-date");
    let field = |range: std::ops::Range<usize>| {
        raw.get(range).expect("x-amz-date is in the signing format")
    };
    let text = format!(
        "{}-{}-{}T{}:{}:{}Z",
        field(0..4),
        field(4..6),
        field(6..8),
        field(9..11),
        field(11..13),
        field(13..15)
    );

    OffsetDateTime::parse(&text, &Rfc3339)
        .expect("x-amz-date is in the signing format")
        .to_utc()
}

/// `at` as an HTTP `Date` header, in the GMT form AWS sends.
fn http_date(at: UtcDateTime) -> String {
    timestamp::rfc2822(at)
        .expect("the test clock is inside four-digit years")
        .replace("+0000", "GMT")
}

/// The whole shape of a signed request, asserted once so the other tests can be about behaviour.
#[tokio::test]
async fn a_request_arrives_signed_with_what_it_carries() {
    let server = StubServer::start(vec![StubResponse::ok(r#"{"Item":{"a":{"N":"1"}}}"#)]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let output = client
        .get_item("fragments", &key(), true)
        .await
        .expect("the stub answers successfully");

    assert!(output.item.is_some(), "the response body was parsed");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];

    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/");
    assert_eq!(
        request.header("x-amz-target"),
        Some("DynamoDB_20120810.GetItem")
    );
    assert_eq!(
        request.header("content-type"),
        Some("application/x-amz-json-1.0")
    );

    // The body hash is over the body that actually arrived, which is what binds the two together.
    let mut digest = [0u8; 64];
    assert_eq!(
        request.header("x-amz-content-sha256"),
        Some(payload_sha256(&request.body, &mut digest)),
        "the signed payload hash must describe the body sent"
    );

    let authorization = request
        .header("authorization")
        .expect("a signed request carries Authorization");
    assert!(
        authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/"),
        "unexpected credential: {authorization}"
    );
    assert!(
        authorization.contains("/eu-west-1/dynamodb/aws4_request"),
        "the scope must name the region and service: {authorization}"
    );
    // Every x-amz-* header the request carries has to be in SignedHeaders, or it could be changed
    // in flight without invalidating the signature.
    for signed in ["host", "x-amz-content-sha256", "x-amz-date", "x-amz-target"] {
        assert!(
            authorization.contains(signed),
            "{signed} must be signed: {authorization}"
        );
    }

    // And the body is the JSON the operation meant to send.
    assert!(request.body_str().contains(r#""TableName":"fragments""#));
    assert!(request.body_str().contains(r#""ConsistentRead":true"#));
}

#[tokio::test]
async fn a_throttle_goes_straight_back_to_the_caller() {
    let server = StubServer::start(vec![
        StubResponse::dynamo_error(400, "ProvisionedThroughputExceededException"),
        StubResponse::ok(r#"{"Item":{"a":{"N":"1"}}}"#),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("a throttle is the caller's to wait out");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(DynamoErrorCode::ProvisionedThroughputExceededException)
    );
    assert_eq!(
        server.request_count(),
        1,
        "the storage layer holds the ladder for congestion; this one must not add a second"
    );
}

/// A request that never got an answer is the client's own to redo: one request, against the whole
/// store operation the layer above would have to repeat.
#[tokio::test]
async fn a_dropped_connection_is_retried_here() {
    let server = StubServer::start(vec![
        StubResponse::hangup(),
        StubResponse::ok(r#"{"Item":{"a":{"N":"1"}}}"#),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let output = client
        .get_item("fragments", &key(), true)
        .await
        .expect("the retry succeeds");

    assert!(output.item.is_some());
    assert_eq!(server.request_count(), 2, "the dropped request was redone");

    let requests = server.requests();
    assert_eq!(
        requests[0].body, requests[1].body,
        "a retry re-sends the identical body"
    );
    assert!(requests[1].header("authorization").is_some());
}

/// A connection that keeps dropping ends as a failure, not as a hang, and not as an infinite retry.
#[tokio::test]
async fn a_connection_that_never_answers_gives_up_within_the_budget() {
    let server = StubServer::start(vec![StubResponse::hangup()]).await;
    let client = client_for(&server, Duration::from_millis(400));

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("a peer that never answers fails");

    // Either shape is correct here: the budget ran out, or the attempt limit did with the last
    // transport failure in hand.
    match error {
        AwsCallError::Timeout | AwsCallError::Transport(_) => {}
        other => panic!("unexpected failure: {other:?}"),
    }

    assert!(
        server.request_count() > 1,
        "a transport failure must be retried at least once"
    );
}

/// A decision is not a hiccup. Retrying a `ValidationException` would turn one bad request into
/// several.
#[tokio::test]
async fn a_validation_error_is_not_retried() {
    let server =
        StubServer::start(vec![StubResponse::dynamo_error(400, "ValidationException")]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("a validation error fails");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(DynamoErrorCode::ValidationException)
    );
    assert_eq!(server.request_count(), 1, "a decision must not be retried");
}

/// An operation answering with at most one item has a cap of its own, far under the client's, so a
/// misdirected endpoint cannot hand it a body the size of a batch to decode.
#[tokio::test]
async fn a_single_item_answer_past_its_cap_is_refused() {
    let padding = "x".repeat(SINGLE_RESPONSE_CAP as usize);
    let server = StubServer::start(vec![StubResponse::ok(&format!(
        r#"{{"Item":{{"a":{{"S":"{padding}"}}}}}}"#
    ))])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("a body past the cap is not an answer");

    assert!(
        matches!(error, AwsCallError::Transport(TransportError::Protocol(_))),
        "unexpected failure: {error:?}"
    );
}

/// A conditional write that lost gets the row it lost against, all the way through the parse.
#[tokio::test]
async fn a_failed_condition_carries_the_losing_row_back() {
    let server = StubServer::start(vec![StubResponse::new(
        400,
        r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException",
            "Item":{"hash":{"B":"Bw=="},"state":{"N":"2"}}}"#,
    )])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let condition = lore_aws::aws::dynamodb::Condition {
        expression: "attribute_not_exists(#pk)",
        names: &[("#pk", "hash")],
        values: &[],
        return_values_on_failure: true,
    };

    let error = client
        .put_item("state", &key(), Some(&condition))
        .await
        .expect_err("the condition fails");

    let service = error.as_service_error().expect("a service error");
    assert!(service.code.is_conditional_check_failed_exception());
    assert_eq!(
        service.item().and_then(|item| item.get("state")),
        Some(&AttributeValue::number(2u32)),
        "the losing row must reach the caller"
    );
    assert_eq!(server.request_count(), 1);
}

/// A host whose clock has drifted must adopt the service's time and re-sign, or every request
/// fails.
#[tokio::test]
async fn a_skewed_clock_is_corrected_and_the_request_re_signed() {
    // The service is half an hour ahead of this host, and says so in its Date header.
    let server_now = UtcDateTime::now() + SignedDuration::minutes(30);
    let server = StubServer::start(vec![
        StubResponse::dynamo_error(403, "RequestTimeTooSkewed")
            .header("Date", &http_date(server_now)),
        StubResponse::ok(r#"{"Item":{"a":{"N":"1"}}}"#),
    ])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    let output = client
        .get_item("fragments", &key(), true)
        .await
        .expect("the re-signed request succeeds");

    assert!(output.item.is_some());
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the skew was corrected and retried once");

    let drift = signed_at(&requests[1]) - signed_at(&requests[0]);
    assert!(
        drift >= SignedDuration::minutes(29) && drift <= SignedDuration::minutes(31),
        "the second signature must use the service's clock, drifted by {drift}"
    );
}

/// A skew the service keeps rejecting must stop, not loop. Correction gets one attempt.
#[tokio::test]
async fn a_clock_skew_that_does_not_recover_is_reported() {
    let server_now = UtcDateTime::now() + SignedDuration::minutes(30);
    let skewed = StubResponse::dynamo_error(403, "RequestTimeTooSkewed")
        .header("Date", &http_date(server_now));
    let server = StubServer::start(vec![skewed]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("an unrecoverable skew fails");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(DynamoErrorCode::RequestTimeTooSkewed)
    );
    assert_eq!(
        server.request_count(),
        2,
        "one correction attempt, then the failure is the caller's"
    );
}

/// A service that accepts a request and never answers must end as a timeout, bounded by the budget.
#[tokio::test]
async fn a_silent_service_exhausts_the_budget_as_a_timeout() {
    let server = StubServer::silent().await;
    let client = client_for(&server, Duration::from_millis(300));

    let started = std::time::Instant::now();
    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("a silent service cannot succeed");
    let elapsed = started.elapsed();

    assert!(
        matches!(error, AwsCallError::Timeout),
        "unexpected failure: {error:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the budget must bound the wait, took {elapsed:?}"
    );
    assert!(
        server.request_count() >= 1,
        "the request did reach the service"
    );
}

#[derive(Debug)]
struct NoCredentials;

#[async_trait::async_trait]
impl ProvideCredentials for NoCredentials {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        Err(CredentialsError::NoProvider)
    }

    fn name(&self) -> &'static str {
        "none"
    }
}

/// Without credentials there is nothing to sign, so nothing should be sent.
#[tokio::test]
async fn a_credential_failure_never_reaches_the_network() {
    let server = StubServer::start(vec![StubResponse::ok("{}")]).await;
    let client = client_with_credentials(&server, Duration::from_secs(5), Arc::new(NoCredentials));

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("no credentials, no call");

    assert!(
        matches!(error, AwsCallError::Credentials { .. }),
        "unexpected failure: {error:?}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "an unsigned request must never be sent"
    );
}

/// The load-shedding path: a 200 that leaves keys unprocessed, read end to end.
#[tokio::test]
async fn unprocessed_keys_survive_the_round_trip() {
    let server = StubServer::start(vec![StubResponse::ok(
        r#"{"Responses":{"fragments":[{"hash":{"B":"AQ=="}}]},
            "UnprocessedKeys":{"fragments":{"Keys":[{"hash":{"B":"Ag=="}}],"ConsistentRead":true}}}"#,
    )])
    .await;
    let client = client_for(&server, Duration::from_secs(5));
    let mut second = Item::new();
    second.insert(ByteStr::from_static("hash"), AttributeValue::blob([2u8]));

    let output = client
        .batch_get_item("fragments", &[key(), second], true)
        .await
        .expect("a 200 with unprocessed keys is not an error");

    assert_eq!(output.items.len(), 1);
    assert_eq!(
        output.unprocessed_keys.len(),
        1,
        "shed keys must reach the caller so it can re-submit them"
    );
    assert_eq!(
        output.unprocessed_keys[0]
            .get("hash")
            .and_then(AttributeValue::as_blob),
        Some(&[2u8][..])
    );
}

/// A transaction is only safely retryable under a stable request token, so a retry must re-send the
/// same one rather than mint a second.
#[tokio::test]
async fn a_retried_transaction_keeps_its_request_token() {
    let server = StubServer::start(vec![StubResponse::hangup(), StubResponse::ok("{}")]).await;
    let client = client_for(&server, Duration::from_secs(5));

    let put = lore_aws::aws::dynamodb::TransactPut::new(Arc::from("locks"), key())
        .condition("attribute_not_exists(#pk)")
        .name("#pk", "hash");

    client
        .transact_write_items(
            &[lore_aws::aws::dynamodb::TransactWriteItem::Put(put)],
            "token-that-must-not-change",
        )
        .await
        .expect("the retry succeeds");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "a dropped connection is resent");
    for request in &requests {
        assert!(
            request
                .body_str()
                .contains(r#""ClientRequestToken":"token-that-must-not-change""#),
            "every attempt must carry the same token: {}",
            request.body_str()
        );
    }
    assert_eq!(
        requests[0].body, requests[1].body,
        "a retried transaction must be byte-identical, or it is not the same transaction"
    );
}

/// `DescribeTable` answering `ResourceNotFoundException` is the "no such table" answer, not a
/// failure.
#[tokio::test]
async fn a_missing_table_reads_as_absent_rather_than_failing() {
    let server = StubServer::start(vec![StubResponse::dynamo_error(
        400,
        "ResourceNotFoundException",
    )])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    assert!(!client.table_exists("nope").await.expect("not an error"));
    assert_eq!(server.request_count(), 1, "absence must not be retried");
}

#[tokio::test]
async fn an_existing_table_reads_as_present() {
    let server = StubServer::start(vec![StubResponse::ok(
        r#"{"Table":{"TableStatus":"ACTIVE"}}"#,
    )])
    .await;
    let client = client_for(&server, Duration::from_secs(5));

    assert!(
        client
            .table_exists("fragments")
            .await
            .expect("not an error")
    );
}

/// A body the service could not have sent is a protocol failure, not a service error — and it goes
/// straight back, because a body that arrived whole and did not parse will not parse next time
/// either. A body cut short on the wire reaches the caller as [`TransportError::Body`] instead, and
/// that one is resent.
#[tokio::test]
async fn a_malformed_success_body_is_a_protocol_failure() {
    let server = StubServer::start(vec![StubResponse::ok(r#"{"Item":{"a":{"N":"#)]).await;
    let client = client_for(&server, Duration::from_millis(400));

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("a truncated body cannot be parsed");

    assert!(
        matches!(error, AwsCallError::Transport(TransportError::Protocol(_))),
        "unexpected failure: {error:?}"
    );
    assert_eq!(
        server.request_count(),
        1,
        "a body the client cannot read must not be asked for again"
    );
}

/// A provider that hands out a different key every time, so a test can tell which credentials
/// signed which request.
#[derive(Debug, Default)]
struct RotatingProvider {
    issued: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ProvideCredentials for RotatingProvider {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        let issued = self
            .issued
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        Ok(Credentials::new(
            format!("AKIDROTATE{issued}"),
            "secret-for-tests",
            None,
            // Far enough out that the cache has no reason of its own to refresh.
            Some(UtcDateTime::now() + SignedDuration::hours(1)),
        ))
    }

    fn name(&self) -> &'static str {
        "rotating"
    }
}

/// The access key id a request was signed with.
fn signing_key_id(request: &RecordedRequest) -> &str {
    request
        .header("authorization")
        .expect("a signed request carries Authorization")
        .split("Credential=")
        .nth(1)
        .and_then(|credential| credential.split('/').next())
        .expect("the credential scope names the access key id")
}

/// Credentials the cache still believes in, which the service has already expired, must be dropped
/// and re-fetched — otherwise every call inside the rotation window fails until the cache's own
/// refresh margin comes round, and re-sending the same rejected signature cannot help.
#[tokio::test]
async fn expired_credentials_are_refreshed_and_the_request_re_signed() {
    let server = StubServer::start(vec![
        StubResponse::dynamo_error(400, "ExpiredTokenException"),
        StubResponse::ok(r#"{"Item":{"a":{"N":"1"}}}"#),
    ])
    .await;
    let client = client_with_credentials(
        &server,
        Duration::from_secs(5),
        Arc::new(RotatingProvider::default()),
    );

    let output = client
        .get_item("fragments", &key(), true)
        .await
        .expect("the re-signed request succeeds");

    assert!(output.item.is_some());
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the rejection earns exactly one retry");
    assert_eq!(signing_key_id(&requests[0]), "AKIDROTATE0");
    assert_eq!(
        signing_key_id(&requests[1]),
        "AKIDROTATE1",
        "the retry must be signed with freshly fetched credentials"
    );
}

/// Refreshing gets one attempt. A service that keeps calling the credentials expired is telling us
/// something the client cannot fix, and looping on it would burn the whole budget.
#[tokio::test]
async fn credentials_the_service_keeps_rejecting_are_reported() {
    let server = StubServer::start(vec![StubResponse::dynamo_error(
        400,
        "ExpiredTokenException",
    )])
    .await;
    let client = client_with_credentials(
        &server,
        Duration::from_secs(5),
        Arc::new(RotatingProvider::default()),
    );

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("credentials the service will not accept cannot succeed");

    assert_eq!(
        error.as_service_error().map(|service| service.code.clone()),
        Some(DynamoErrorCode::ExpiredTokenException)
    );
    assert_eq!(
        server.request_count(),
        2,
        "one refresh attempt, then the failure is the caller's"
    );
}

/// A call that succeeds builds its request once: the transport takes ownership of the URL and
/// header map, and cloning them up front to serve a retry that does not happen is what this avoids.
#[tokio::test]
async fn a_successful_request_is_built_once() {
    let server = StubServer::start(vec![StubResponse::ok("{}")]).await;
    let client = client_for(&server, Duration::from_secs(5));
    let core = client.core().clone();

    let built = AtomicUsize::new(0);
    core.send::<DynamoProtocol>(|| {
        built.fetch_add(1, Ordering::SeqCst);
        HttpRequest::new(Method::POST, core.url("/", None)).body(Bytes::from_static(b"{}"))
    })
    .await
    .expect("the stub answers");

    assert_eq!(built.load(Ordering::SeqCst), 1);
    assert_eq!(server.request_count(), 1);
}

/// A retried call builds one request per attempt, each signed for its own timestamp.
#[tokio::test]
async fn a_retried_request_is_built_once_per_attempt() {
    let server = StubServer::start(vec![StubResponse::hangup(), StubResponse::ok("{}")]).await;
    let client = client_for(&server, Duration::from_secs(5));
    let core = client.core().clone();

    let built = AtomicUsize::new(0);
    core.send::<DynamoProtocol>(|| {
        built.fetch_add(1, Ordering::SeqCst);
        HttpRequest::new(Method::POST, core.url("/", None)).body(Bytes::from_static(b"{}"))
    })
    .await
    .expect("the retry succeeds");

    assert_eq!(built.load(Ordering::SeqCst), 2, "one build per attempt");
    assert_eq!(server.request_count(), 2);

    let requests = server.requests();
    assert_eq!(
        requests[0].body, requests[1].body,
        "a rebuilt request must carry the identical body, or its signature covers a different one"
    );
    assert!(requests[1].header("authorization").is_some());
}

/// Session credentials put a token on the wire, and it has to be signed along with everything else.
#[tokio::test]
async fn session_credentials_sign_their_token() {
    let server = StubServer::start(vec![StubResponse::ok("{}")]).await;
    let client = client_with_credentials(
        &server,
        Duration::from_secs(5),
        Arc::new(StaticProvider::new(Credentials::new(
            "ASIATEST",
            "secret-for-tests",
            Some("session-token-value".into()),
            None,
        ))),
    );

    client
        .delete_item("state", &key(), None)
        .await
        .expect("the stub answers");

    let requests = server.requests();
    assert_eq!(
        requests[0].header("x-amz-security-token"),
        Some("session-token-value")
    );
    assert!(
        requests[0]
            .header("authorization")
            .expect("signed")
            .contains("x-amz-security-token"),
        "an unsigned session token could be stripped in flight"
    );
}

/// A key unique to one call of one task, so every request a test sends has a body of its own.
fn key_for(task: usize, call: usize) -> Item {
    let mut key = Item::new();
    key.insert(
        ByteStr::from_static("hash"),
        AttributeValue::blob([task as u8, call as u8]),
    );
    key
}

/// The `Authorization` a recorded request should carry, recomputed from what arrived with the
/// credentials [`client_for`] signs with.
fn expected_authorization(server: &StubServer, request: &RecordedRequest) -> String {
    let url = server
        .endpoint()
        .join(&request.target)
        .expect("the target is a path");
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        headers.append(
            HeaderName::from_bytes(name.as_bytes()).expect("a header name arrived"),
            HeaderValue::from_str(value).expect("a header value arrived as text"),
        );
    }

    let context = SigningContext {
        access_key_id: "AKIDTEST",
        secret_access_key: "secret-for-tests",
        session_token: None,
        region: "eu-west-1",
        service: "dynamodb",
        timestamp: signed_at(request),
    };
    let mut digest = [0u8; 64];
    sign_request(
        &request.method,
        &url,
        &mut headers,
        payload_sha256(&request.body, &mut digest),
        &context,
        &SigningKeyCache::new(),
        &mut SignScratch::new(),
    )
    .expect("a recorded request signs again");

    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .expect("signing sets Authorization")
        .to_owned()
}

/// The signing and body buffers are thread-locals, and a task does not stay on one thread.
///
/// Both are borrowed only across a synchronous call and cleared on entry, so which thread's buffer
/// an attempt lands on does not matter. Enough concurrent work on a multi-threaded runtime, with
/// yields between calls, that tasks are rescheduled across workers. Every call sends a body of its
/// own, and every request must arrive with that body and a signature computed over exactly it.
#[test]
fn signing_and_body_buffers_survive_tasks_moving_between_worker_threads() {
    const TASKS: usize = 16;
    const PER_TASK: usize = 8;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("a multi-threaded runtime builds");

    runtime.block_on(async {
        let script = (0..TASKS * PER_TASK)
            .map(|_| StubResponse::ok(r#"{"Item":{"a":{"N":"1"}}}"#))
            .collect();
        let server = StubServer::start(script).await;
        let client = client_for(&server, Duration::from_secs(10));

        let mut tasks = tokio::task::JoinSet::new();
        for task in 0..TASKS {
            let client = client.clone();
            lore_base::lore_spawn!(tasks, async move {
                for call in 0..PER_TASK {
                    tokio::task::yield_now().await;
                    client
                        .get_item("fragments", &key_for(task, call), true)
                        .await
                        .expect("the stub answers successfully");
                }
            });
        }

        while let Some(finished) = tasks.join_next().await {
            finished.expect("no task panicked");
        }

        let requests = server.requests();
        assert_eq!(requests.len(), TASKS * PER_TASK);

        let bodies: HashSet<&[u8]> = requests
            .iter()
            .map(|request| request.body.as_slice())
            .collect();
        assert_eq!(
            bodies.len(),
            TASKS * PER_TASK,
            "every call sent a body of its own, so no two requests may carry the same one"
        );

        for request in &requests {
            let mut digest = [0u8; 64];
            assert_eq!(
                request.header("x-amz-content-sha256"),
                Some(payload_sha256(&request.body, &mut digest)),
                "a request was signed for a body other than its own"
            );
            assert_eq!(
                request.header("authorization"),
                Some(expected_authorization(&server, request).as_str()),
                "a request carries a signature computed over something other than what it sent"
            );
        }
    });
}

/// Several requests signed under the same stale clock are each rejected, and each is re-signed once
/// the correction is learned — including those whose rejection teaches nothing new because another
/// request already learned it.
///
/// The three calls are driven on one task, so all three are signed before any response is read.
#[tokio::test]
async fn requests_signed_under_a_stale_clock_all_recover_once_one_learns_it() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            // The service is half an hour ahead of this host, and rejects any signature more than
            // five minutes from its own clock.
            let server = StubServer::responding(|request| {
                let server_now = UtcDateTime::now() + SignedDuration::minutes(30);
                if (server_now - signed_at(request)).abs() > SignedDuration::minutes(5) {
                    StubResponse::dynamo_error(403, "RequestTimeTooSkewed")
                        .header("Date", &http_date(server_now))
                } else {
                    StubResponse::ok(r#"{"Item":{"a":{"N":"1"}}}"#)
                }
            })
            .await;
            let client = client_for(&server, Duration::from_secs(5));
            let (first, second, third) = (key_for(0, 0), key_for(0, 1), key_for(0, 2));

            let outcomes = tokio::join!(
                client.get_item("fragments", &first, true),
                client.get_item("fragments", &second, true),
                client.get_item("fragments", &third, true),
            );

            outcomes
                .0
                .expect("the first request re-signs against the learned clock");
            outcomes
                .1
                .expect("the second request re-signs against the learned clock");
            outcomes
                .2
                .expect("the third request re-signs against the learned clock");
            assert_eq!(
                server.request_count(),
                6,
                "each request is rejected once and re-signed once"
            );
        })
        .await;
}

#[derive(Debug)]
struct NeverAnswers;

#[async_trait::async_trait]
impl ProvideCredentials for NeverAnswers {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        std::future::pending().await
    }

    fn name(&self) -> &'static str {
        "never-answers"
    }
}

/// Waiting for credentials is part of the operation, so a provider that never answers ends the
/// operation as a timeout within its budget, with nothing sent.
#[tokio::test]
async fn a_credential_provider_that_never_answers_ends_as_a_timeout() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::ok("{}")]).await;
            let client = client_with_credentials(
                &server,
                Duration::from_millis(200),
                Arc::new(NeverAnswers),
            );

            let error = tokio::time::timeout(
                Duration::from_secs(10),
                client.get_item("fragments", &key(), true),
            )
            .await
            .expect("the operation budget bounds the wait for credentials")
            .expect_err("no credentials, no call");

            assert!(
                matches!(error, AwsCallError::Timeout),
                "unexpected failure: {error:?}"
            );
            assert_eq!(
                server.request_count(),
                0,
                "an unsigned request must never be sent"
            );
        })
        .await;
}

/// An operation whose budget runs out while every connection its client may use stays in use ends
/// as held back by the limit. Its request is neither sent nor resent, and the caller backs off.
#[tokio::test]
async fn an_operation_that_finds_no_connection_by_its_deadline_is_not_resent() {
    let server = StubServer::silent().await;
    let http = HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: NonZeroUsize::new(1),
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds");

    let _held = http.send(
        HttpRequest::new(Method::GET, server.endpoint()),
        Duration::from_secs(30),
    );
    server.wait_for_requests(1).await;

    let client = client_over(
        http.clone(),
        &server,
        Duration::from_millis(300),
        Arc::new(StaticProvider::new(Credentials::new(
            "AKIDTEST",
            "secret-for-tests",
            None,
            None,
        ))),
    );

    let started = std::time::Instant::now();
    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("the only connection stays in use");

    assert!(
        matches!(
            error,
            AwsCallError::Transport(TransportError::ConnectionLimit)
        ),
        "unexpected failure: {error:?}"
    );
    assert!(is_overloaded(&error), "the caller backs off from it");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the operation ran {:?}, past its budget",
        started.elapsed()
    );
    assert_eq!(
        server.request_count(),
        1,
        "the operation's request must not be sent"
    );
}

/// An operation whose first attempt's wait for a connection cut its timeout short, and that was
/// then not answered, ends as kept waiting by the limit rather than as a timeout. It is not resent,
/// and the caller backs off.
#[tokio::test]
async fn a_first_attempt_that_queued_and_ran_out_of_time_ends_as_a_connection_limit_timeout() {
    let server = StubServer::silent().await;
    let http = HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: NonZeroUsize::new(1),
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds");

    let _held = http.send(
        HttpRequest::new(Method::GET, server.endpoint()),
        Duration::from_millis(700),
    );
    server.wait_for_requests(1).await;

    let client = client_over(
        http.clone(),
        &server,
        Duration::from_millis(1000),
        Arc::new(StaticProvider::new(Credentials::new(
            "AKIDTEST",
            "secret-for-tests",
            None,
            None,
        ))),
    );

    let error = client
        .get_item("fragments", &key(), true)
        .await
        .expect_err("the stub never answers");

    assert!(
        matches!(
            error,
            AwsCallError::Transport(TransportError::ConnectionLimitTimeout)
        ),
        "unexpected failure: {error:?}"
    );
    assert!(is_overloaded(&error), "the caller backs off from it");
    assert_eq!(
        server.request_count(),
        2,
        "the held request and the operation's one attempt are sent, and nothing is resent"
    );
}

/// A resend's timeout is already what is left of the operation's budget, so the deadline rather
/// than a brief wait for a connection is what cuts it: a resend that queued and then ran out of
/// time ends as a timeout, as it would without the limit.
#[tokio::test]
async fn a_resend_that_queued_for_a_connection_and_ran_out_of_time_ends_as_a_timeout() {
    let server = StubServer::silent().await;
    let http = HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: NonZeroUsize::new(1),
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds");

    let client = client_over(
        http.clone(),
        &server,
        Duration::from_millis(1000),
        Arc::new(StaticProvider::new(Credentials::new(
            "AKIDTEST",
            "secret-for-tests",
            None,
            None,
        ))),
    );

    // Queued behind the first attempt, this takes the connection when that attempt times out and
    // holds it past the first resend's backoff, so the resend queues behind it.
    let take_the_connection_next = async {
        server.wait_for_requests(1).await;
        http.send_by(
            HttpRequest::new(Method::GET, server.endpoint()),
            Duration::from_millis(150),
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await
    };

    let fragment_key = key();
    let (result, held) = tokio::join!(
        client.get_item("fragments", &fragment_key, true),
        take_the_connection_next
    );
    let error = result.expect_err("the stub never answers");

    assert!(
        held.is_err(),
        "the stub never answers the request holding the connection"
    );
    assert!(
        matches!(error, AwsCallError::Timeout),
        "unexpected failure: {error:?}"
    );
    assert_eq!(
        server.request_count(),
        3,
        "the first attempt, the request that took the connection, and the resend are sent"
    );
}

/// An operation whose resend the connection limit held back, after its first attempt was sent, ends
/// as a timeout rather than as never sent: that first attempt may have been applied.
#[tokio::test]
async fn a_resend_held_back_by_the_connection_limit_ends_as_a_timeout() {
    let server = StubServer::silent().await;
    let http = HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: NonZeroUsize::new(1),
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds");

    let client = client_over(
        http.clone(),
        &server,
        Duration::from_millis(1000),
        Arc::new(StaticProvider::new(Credentials::new(
            "AKIDTEST",
            "secret-for-tests",
            None,
            None,
        ))),
    );

    let take_the_connection_next = async {
        server.wait_for_requests(1).await;
        http.send(
            HttpRequest::new(Method::GET, server.endpoint()),
            Duration::from_secs(2),
        )
        .await
    };

    let fragment_key = key();
    let (result, held) = tokio::join!(
        client.get_item("fragments", &fragment_key, true),
        take_the_connection_next
    );
    let error = result.expect_err("the stub never answers");

    assert!(
        held.is_err(),
        "the stub never answers the request holding the connection"
    );

    assert!(
        matches!(error, AwsCallError::Timeout),
        "unexpected failure: {error:?}"
    );
    assert_eq!(
        server.request_count(),
        2,
        "the first attempt and the request that took the connection are sent, and the resend is not"
    );
}
