// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroUsize;
use std::time::Duration;

use bytes::Bytes;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use lore_aws::aws::error::TransportError;
use lore_aws::aws::http::*;
use lore_aws::clients::HttpClientSettings;
use lore_base::runtime::LORE_CONTEXT;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use url::Url;

use crate::aws::test_server::StubResponse;
use crate::aws::test_server::StubServer;

/// A cap low enough that a test reaches it without moving 64 MiB.
const CAP: u64 = 4 * 1024;

/// A service client whose body cap is [`CAP`].
fn capped_client() -> HttpClient {
    HttpClient::new(&HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: None,
        connection_acquire_timeout_millis: None,
    })
    .expect("a client builds")
    .with_body_cap(CAP)
}

/// A chunked reply four times the cap, so its length is known to no one but the reader.
fn oversized_chunked_reply() -> StubResponse {
    StubResponse::chunked(200, vec![b'x'; CAP as usize * 4], 512)
}

/// The settings of a client that has at most `max` connections in use at once.
fn limited_settings(max: usize) -> HttpClientSettings {
    HttpClientSettings {
        pool_idle_timeout_seconds: 5,
        nodelay: true,
        max_connections: Some(NonZeroUsize::new(max).expect("a limit is at least one")),
        connection_acquire_timeout_millis: None,
    }
}

/// A client that has at most `max` connections in use at once.
fn limited_client(max: usize) -> HttpClient {
    HttpClient::new(&limited_settings(max)).expect("a client builds")
}

/// A client that has at most `max` connections in use at once, and whose requests wait for one no
/// longer than `acquire_timeout`.
fn limited_client_waiting(max: usize, acquire_timeout: Duration) -> HttpClient {
    HttpClient::new(&HttpClientSettings {
        connection_acquire_timeout_millis: Some(
            u64::try_from(acquire_timeout.as_millis()).expect("a test timeout fits"),
        ),
        ..limited_settings(max)
    })
    .expect("a client builds")
}

/// An operation deadline `after` from now.
fn deadline_in(after: Duration) -> Instant {
    Instant::now() + after
}

/// An operation deadline far enough off that no test reaches it.
fn distant_deadline() -> Instant {
    deadline_in(Duration::from_secs(30))
}

#[test]
fn a_client_builds_from_the_store_settings() {
    let settings = HttpClientSettings {
        pool_idle_timeout_seconds: 30,
        nodelay: true,
        max_connections: None,
        connection_acquire_timeout_millis: None,
    };

    assert!(HttpClient::new(&settings).is_ok());
    assert!(HttpClient::for_metadata().is_ok());
    assert!(HttpClient::for_sts().is_ok());
}

/// A limit of zero would let no request through, so the settings refuse it when they are read.
/// Settings that leave the limit out set none.
#[test]
fn settings_refuse_a_connection_limit_of_zero() {
    assert!(serde_json::from_str::<HttpClientSettings>(r#"{"max_connections": 0}"#).is_err());

    let settings: HttpClientSettings = serde_json::from_str("{}").expect("the settings parse");
    assert_eq!(settings.max_connections, None);
}

/// A semaphore cannot hold a limit past `MAX_PERMITS`, so such a limit fails when the client is
/// built rather than when a request is sent.
#[test]
fn a_connection_limit_past_what_a_semaphore_holds_fails_the_build() {
    assert!(matches!(
        HttpClient::new(&limited_settings(Semaphore::MAX_PERMITS + 1)),
        Err(TransportError::Build(_))
    ));
    assert!(HttpClient::new(&limited_settings(Semaphore::MAX_PERMITS)).is_ok());
}

/// A stream reads its status and headers from the response it holds, so it is smaller than that
/// response and a second header map.
#[test]
fn a_stream_holds_its_headers_once() {
    assert!(
        size_of::<HttpStream>() < size_of::<reqwest::Response>() + size_of::<HeaderMap>(),
        "a stream of {} bytes is no smaller than its response of {} bytes and a header map of {}",
        size_of::<HttpStream>(),
        size_of::<reqwest::Response>(),
        size_of::<HeaderMap>()
    );
}

/// A request past the connection limit is not sent while every connection is in use, and fails as
/// held back by the limit when none is released by its operation's deadline.
#[tokio::test]
async fn a_request_past_the_connection_limit_is_not_sent() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(2);

            let _held = [
                client.send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(30),
                ),
                client.send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(30),
                ),
            ];
            server.wait_for_requests(2).await;

            let error = client
                .send_by(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(30),
                    deadline_in(Duration::from_millis(200)),
                )
                .await
                .expect_err("no connection is released by the deadline");

            assert!(
                matches!(error, TransportError::ConnectionLimit),
                "unexpected failure: {error:?}"
            );
            assert_eq!(
                server.request_count(),
                2,
                "a request past the limit must not be sent"
            );
        })
        .await;
}

/// A request past the connection limit waits for a connection past its attempt's timeout, until its
/// operation's deadline, and is answered once one is released.
#[tokio::test]
async fn a_request_waits_for_a_connection_past_its_attempt_timeout() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let silent = StubServer::silent().await;
            let answering = StubServer::start(vec![StubResponse::ok("{}")]).await;
            let client = limited_client(1);

            let _held = client.send(
                HttpRequest::new(Method::GET, silent.endpoint()),
                Duration::from_millis(300),
            );
            silent.wait_for_requests(1).await;

            let response = client
                .send_by(
                    HttpRequest::new(Method::GET, answering.endpoint()),
                    Duration::from_millis(100),
                    deadline_in(Duration::from_secs(2)),
                )
                .await
                .expect("a connection is released before the deadline");

            assert_eq!(response.status, StatusCode::OK);
            assert_eq!(answering.request_count(), 1);
        })
        .await;
}

/// A request past the connection limit waits no longer than the client's acquire timeout, though
/// its operation's deadline is far off, and is shed as held back by the limit without being sent.
#[tokio::test]
async fn a_request_past_the_connection_limit_is_shed_at_its_acquire_timeout() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client_waiting(1, Duration::from_millis(100));

            let _held = client.send(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_secs(30),
            );
            server.wait_for_requests(1).await;

            let started = Instant::now();
            let error = client
                .send_by(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(30),
                    distant_deadline(),
                )
                .await
                .expect_err("no connection is released within the acquire timeout");

            assert!(
                matches!(error, TransportError::ConnectionLimit),
                "unexpected failure: {error:?}"
            );
            let waited = started.elapsed();
            assert!(
                waited >= Duration::from_millis(100) && waited < Duration::from_secs(5),
                "the wait ends at the acquire timeout, not the deadline: waited {waited:?}"
            );
            assert_eq!(server.request_count(), 1);
        })
        .await;
}

/// An acquire timeout of zero sheds a request at once when every connection is in use.
#[tokio::test]
async fn an_acquire_timeout_of_zero_sheds_a_request_when_no_connection_is_free() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client_waiting(1, Duration::ZERO);

            let _held = client.send(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_secs(30),
            );
            server.wait_for_requests(1).await;

            let error = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(30),
                    distant_deadline(),
                )
                .await
                .expect_err("no connection is free");

            assert!(
                matches!(error, TransportError::ConnectionLimit),
                "unexpected failure: {error:?}"
            );
            assert_eq!(server.request_count(), 1);
        })
        .await;
}

/// A request past the connection limit is sent once a connection is released within its acquire
/// timeout.
#[tokio::test]
async fn a_connection_released_within_the_acquire_timeout_is_taken() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let silent = StubServer::silent().await;
            let answering = StubServer::start(vec![StubResponse::ok("{}")]).await;
            let client = limited_client_waiting(1, Duration::from_secs(2));

            let _held = client.send(
                HttpRequest::new(Method::GET, silent.endpoint()),
                Duration::from_millis(200),
            );
            silent.wait_for_requests(1).await;

            let response = client
                .send_by(
                    HttpRequest::new(Method::GET, answering.endpoint()),
                    Duration::from_secs(5),
                    distant_deadline(),
                )
                .await
                .expect("the held connection is released within the acquire timeout");

            assert_eq!(response.status, StatusCode::OK);
        })
        .await;
}

/// A request whose caller drops it while it waits for a connection is never sent, and a request
/// whose caller drops it once sent releases its connection, so the next request takes it.
#[tokio::test]
async fn a_dropped_request_waiting_for_a_connection_is_never_sent() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let silent = StubServer::silent().await;
            let answering = StubServer::start(vec![StubResponse::ok("{}")]).await;
            let client = limited_client(1);

            let held = client.send(
                HttpRequest::new(Method::GET, silent.endpoint()),
                Duration::from_secs(30),
            );
            silent.wait_for_requests(1).await;

            let abandoned = client.send(
                HttpRequest::new(Method::GET, answering.endpoint()),
                Duration::from_secs(30),
            );
            drop(abandoned);
            drop(held);

            let response = client
                .send(
                    HttpRequest::new(Method::GET, answering.endpoint()),
                    Duration::from_secs(2),
                )
                .await
                .expect("the dropped requests released the connection");

            assert_eq!(response.status, StatusCode::OK);
            assert_eq!(
                answering.request_count(),
                1,
                "the dropped request must never be sent"
            );
        })
        .await;
}

/// A connection is released once its body has been read, so requests waiting behind a limit of
/// one are all answered.
#[tokio::test]
async fn requests_past_the_connection_limit_are_sent_as_connections_are_released() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::ok("{}")]).await;
            let client = limited_client(1);
            let send = || {
                client.send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                )
            };

            let (first, second, third) = tokio::join!(send(), send(), send());

            for response in [first, second, third] {
                assert_eq!(
                    response.expect("each request is answered").status,
                    StatusCode::OK
                );
            }
            assert_eq!(server.request_count(), 3);
        })
        .await;
}

/// A request whose whole timeout fit before its deadline when it began to wait for a connection,
/// and that the wait left with less, has what is left for the exchange, and running out of it is
/// reported as the connection limit's doing.
#[tokio::test]
async fn an_exchange_that_queued_ends_by_the_deadline_as_a_connection_limit_timeout() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let _held = client.send(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_millis(400),
            );
            server.wait_for_requests(1).await;

            let started = Instant::now();
            let error = client
                .send_by(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_millis(500),
                    deadline_in(Duration::from_millis(600)),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::ConnectionLimitTimeout),
                "unexpected failure: {error:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the exchange ran {:?}, past its deadline",
                started.elapsed()
            );
            assert_eq!(server.request_count(), 2);
        })
        .await;
}

/// A stream whose whole timeout fit before its deadline when it began to wait for a connection,
/// and that the wait left with less, has what is left for the response's headers, and running out
/// of it is reported as the connection limit's doing.
#[tokio::test]
async fn a_stream_that_queued_ends_by_the_deadline_as_a_connection_limit_timeout() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let _held = client.send_streaming(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_millis(400),
                deadline_in(Duration::from_millis(400)),
            );
            server.wait_for_requests(1).await;

            let started = Instant::now();
            let error = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_millis(500),
                    deadline_in(Duration::from_millis(600)),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::ConnectionLimitTimeout),
                "unexpected failure: {error:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the stream waited {:?}, past its deadline",
                started.elapsed()
            );
            assert_eq!(server.request_count(), 2);
        })
        .await;
}

/// A request whose timeout reached past its deadline before it waited for a connection, as a late
/// resend's does, had its time cut by the deadline rather than by the wait, so running out of it is
/// the service's doing however long it queued.
#[tokio::test]
async fn an_exchange_whose_timeout_the_deadline_had_already_cut_times_out_as_a_transport_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let _held = client.send(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_millis(300),
            );
            server.wait_for_requests(1).await;

            let error = client
                .send_by(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(10),
                    deadline_in(Duration::from_millis(600)),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::Io(_)),
                "unexpected failure: {error:?}"
            );
            assert_eq!(server.request_count(), 2);
        })
        .await;
}

/// A request whose wait cut its timeout short and whose body then stops arriving runs out of that
/// time reading the body, which is reported as the connection limit's doing as a silent service is.
#[tokio::test]
async fn an_exchange_whose_body_stalls_after_a_wait_ends_as_a_connection_limit_timeout() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let silent = StubServer::silent().await;
            let stalling = StubServer::start(vec![
                StubResponse::chunked(200, vec![b'x'; 1024], 256).paced(Duration::from_secs(5)),
            ])
            .await;
            let client = limited_client(1);

            let _held = client.send(
                HttpRequest::new(Method::GET, silent.endpoint()),
                Duration::from_millis(400),
            );
            silent.wait_for_requests(1).await;

            let error = client
                .send_by(
                    HttpRequest::new(Method::GET, stalling.endpoint()),
                    Duration::from_millis(500),
                    deadline_in(Duration::from_millis(600)),
                )
                .await
                .expect_err("the body never arrives in time");

            assert!(
                matches!(error, TransportError::ConnectionLimitTimeout),
                "unexpected failure: {error:?}"
            );
            assert_eq!(stalling.request_count(), 1);
        })
        .await;
}

/// A request that found a connection free did not queue, so running out of time is the service's
/// doing, even when its timeout reaches past the operation's deadline and is cut to it.
#[tokio::test]
async fn an_exchange_that_found_a_connection_free_times_out_as_a_transport_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let error = client
                .send_by(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(10),
                    deadline_in(Duration::from_millis(300)),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::Io(_)),
                "unexpected failure: {error:?}"
            );
        })
        .await;
}

/// A request whose wait for a connection left it all of its timeout ran out of time the service
/// was given in full, so that is the service's doing.
#[tokio::test]
async fn an_exchange_whose_wait_left_its_timeout_whole_times_out_as_a_transport_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let _held = client.send(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_millis(100),
            );
            server.wait_for_requests(1).await;

            let error = client
                .send_by(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_millis(200),
                    distant_deadline(),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::Io(_)),
                "unexpected failure: {error:?}"
            );
            assert_eq!(server.request_count(), 2);
        })
        .await;
}

/// A stream whose wait for a connection left it all of its timeout ran out of time the service was
/// given in full, so its headers not arriving is the service's doing.
#[tokio::test]
async fn a_stream_whose_wait_left_its_timeout_whole_times_out_as_a_transport_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let _held = client.send_streaming(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_millis(100),
                deadline_in(Duration::from_millis(100)),
            );
            server.wait_for_requests(1).await;

            let error = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_millis(200),
                    distant_deadline(),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::Io(_)),
                "unexpected failure: {error:?}"
            );
            assert_eq!(server.request_count(), 2);
        })
        .await;
}

/// A timeout counts as cut by the wait for a connection only when the whole of it fit before the
/// deadline as the wait began and no longer does once it ends.
#[test]
fn only_a_wait_that_cut_a_whole_timeout_short_counts_against_the_limit() {
    let ms = Duration::from_millis;

    assert!(
        shortened_by_wait(ms(500), ms(300), ms(300)),
        "500 fit in the 600 left before the wait, and 300 are left after it"
    );
    assert!(
        !shortened_by_wait(ms(500), Duration::ZERO, ms(300)),
        "a request that found a connection free did not wait"
    );
    assert!(
        !shortened_by_wait(ms(500), ms(5), ms(490)),
        "the 495 left before the wait had already cut the timeout"
    );
    assert!(
        !shortened_by_wait(ms(500), ms(200), ms(300)),
        "a timeout exactly the time left before the wait was cut by the deadline, not by the wait"
    );
    assert!(
        !shortened_by_wait(ms(200), ms(100), ms(29_900)),
        "the wait left the whole timeout"
    );
}

/// A stream whose timeout reached past its deadline before it waited for a connection had its time
/// cut by the deadline rather than by the wait, so its headers not arriving is the service's doing
/// however long it queued.
#[tokio::test]
async fn a_stream_whose_timeout_the_deadline_had_already_cut_times_out_as_a_transport_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let _held = client.send_streaming(
                HttpRequest::new(Method::GET, server.endpoint()),
                Duration::from_millis(300),
                deadline_in(Duration::from_millis(300)),
            );
            server.wait_for_requests(1).await;

            let error = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(10),
                    deadline_in(Duration::from_millis(600)),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::Io(_)),
                "unexpected failure: {error:?}"
            );
            assert_eq!(server.request_count(), 2);
        })
        .await;
}

/// Every request names Lore and its version as its user agent, so the service's own logs can tell
/// Lore's traffic apart.
#[tokio::test]
async fn a_request_names_lore_as_its_user_agent() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::ok("{}")]).await;

            capped_client()
                .send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                )
                .await
                .expect("the stub answers");

            let requests = server.requests();
            assert_eq!(
                requests[0].header("user-agent"),
                Some(concat!("lore/", env!("CARGO_PKG_VERSION")))
            );
        })
        .await;
}

/// A stream that found a connection free did not queue, so its headers not arriving in time is the
/// service's doing, even when its timeout reaches past the operation's deadline and is cut to it.
#[tokio::test]
async fn a_stream_that_found_a_connection_free_times_out_as_a_transport_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::silent().await;
            let client = limited_client(1);

            let error = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(10),
                    deadline_in(Duration::from_millis(300)),
                )
                .await
                .expect_err("the stub never answers");

            assert!(
                matches!(error, TransportError::Io(_)),
                "unexpected failure: {error:?}"
            );
        })
        .await;
}

/// A stream's body is still on its connection, so the stream holds the connection until it is
/// dropped, and a request waiting for it is sent after that.
#[tokio::test]
async fn a_stream_holds_its_connection_until_it_is_dropped() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::ok("payload")]).await;
            let client = limited_client(1);

            let stream = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                    distant_deadline(),
                )
                .await
                .expect("the first stream opens");

            let error = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                    deadline_in(Duration::from_millis(200)),
                )
                .await
                .expect_err("the only connection is the open stream's");

            assert!(
                matches!(error, TransportError::ConnectionLimit),
                "unexpected failure: {error:?}"
            );
            assert_eq!(server.request_count(), 1);

            drop(stream);

            let stream = client
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                    distant_deadline(),
                )
                .await
                .expect("the dropped stream released its connection");

            assert_eq!(
                stream.bytes().await.expect("the body is read").as_ref(),
                b"payload"
            );
            assert_eq!(server.request_count(), 2);
        })
        .await;
}

/// A stream request whose caller drops it before it opens is aborted, so its connection goes to
/// the next request rather than staying held until the abandoned one's own timeout.
#[tokio::test]
async fn a_dropped_stream_request_releases_its_connection() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let silent = StubServer::silent().await;
            let answering = StubServer::start(vec![StubResponse::ok("payload")]).await;
            let client = limited_client(1);

            let abandoned = client.send_streaming(
                HttpRequest::new(Method::GET, silent.endpoint()),
                Duration::from_secs(30),
                distant_deadline(),
            );
            silent.wait_for_requests(1).await;
            drop(abandoned);

            let stream = client
                .send_streaming(
                    HttpRequest::new(Method::GET, answering.endpoint()),
                    Duration::from_secs(5),
                    deadline_in(Duration::from_secs(2)),
                )
                .await
                .expect("the abandoned request released its connection");

            assert_eq!(
                stream.bytes().await.expect("the body is read").as_ref(),
                b"payload"
            );
        })
        .await;
}

#[test]
fn a_header_that_is_not_text_reads_as_absent() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-amz-meta-lore-fragment",
        HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
    );

    let response = HttpResponse {
        status: StatusCode::OK,
        headers,
        body: Bytes::new(),
    };

    assert_eq!(response.header("x-amz-meta-lore-fragment"), None);
    assert_eq!(response.header("absent"), None);
}

/// A body can carry a credential, so `Debug` shows how long it is rather than what it says.
#[test]
fn debug_shows_a_body_length_rather_than_its_bytes() {
    let request = HttpRequest::new(Method::POST, Url::parse("https://sts.example/").unwrap())
        .body(Bytes::from_static(b"WebIdentityToken=SECRET-JWT"));
    let response = HttpResponse {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        body: Bytes::from_static(b"{\"SecretAccessKey\":\"SECRET-KEY\"}"),
    };

    let rendered = format!("{request:?} {response:?}");

    assert!(!rendered.contains("SECRET-JWT"), "{rendered}");
    assert!(!rendered.contains("SECRET-KEY"), "{rendered}");
}

/// The cap bounds a body whose length is not declared.
///
/// `Content-Length` is absent on any chunked reply, so the cap is enforced on the reads themselves:
/// that is what bounds an endpoint that is not the one we think it is.
#[tokio::test]
async fn a_chunked_body_past_the_cap_is_refused() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![oversized_chunked_reply()]).await;

            let error = capped_client()
                .send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                )
                .await
                .expect_err("a body past the cap is not an answer");

            assert!(
                matches!(error, TransportError::Protocol(_)),
                "unexpected failure: {error:?}"
            );
        })
        .await;
}

/// A declared length past the cap is refused before its body is read.
#[tokio::test]
async fn a_declared_body_past_the_cap_is_refused() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let body = "x".repeat(CAP as usize + 1);
            let server = StubServer::start(vec![StubResponse::ok(&body)]).await;

            let error = capped_client()
                .send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                )
                .await
                .expect_err("a body past the cap is not an answer");

            assert!(
                matches!(error, TransportError::Protocol(_)),
                "unexpected failure: {error:?}"
            );
        })
        .await;
}

/// The same path delivers a chunked body that fits, whole.
#[tokio::test]
async fn a_chunked_body_inside_the_cap_arrives_whole() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let body = vec![b'y'; 3_000];
            let server =
                StubServer::start(vec![StubResponse::chunked(200, body.clone(), 512)]).await;

            let response = capped_client()
                .send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                )
                .await
                .expect("a body inside the cap is read");

            assert_eq!(response.body.as_ref(), body.as_slice());
        })
        .await;
}

/// A declared body that arrives over several reads is delivered whole.
#[tokio::test]
async fn a_declared_body_read_in_pieces_arrives_whole() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let body = "z".repeat(256 * 1024);
            let server = StubServer::start(vec![StubResponse::ok(&body)]).await;
            let client = HttpClient::new(&HttpClientSettings {
                pool_idle_timeout_seconds: 5,
                nodelay: true,
                max_connections: None,
                connection_acquire_timeout_millis: None,
            })
            .expect("a client builds");

            let response = client
                .send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                )
                .await
                .expect("a body inside the cap is read");

            assert_eq!(response.body.as_ref(), body.as_bytes());
        })
        .await;
}

/// The streamed path carries the same cap, enforced on the read that crosses it.
///
/// `GetObject` leaves its body on the wire, so the reader above is the only thing counting. The cap
/// holds as a limit: the crossing read's bytes are not handed over.
#[tokio::test]
async fn a_streamed_body_past_the_cap_is_refused() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![oversized_chunked_reply()]).await;

            let mut stream = capped_client()
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await
                .expect("the response arrives before its body is read");

            let mut read = 0usize;
            let error = loop {
                match stream.chunk().await {
                    Ok(Some(chunk)) => read += chunk.len(),
                    Ok(None) => panic!("a body past the cap must not read to its end"),
                    Err(error) => break error,
                }
            };

            assert!(
                matches!(error, TransportError::Protocol(_)),
                "unexpected failure: {error:?}"
            );
            assert!(
                read as u64 <= CAP,
                "the cap bounds what the reader is handed, so {read} bytes is too many"
            );
        })
        .await;
}

/// The attempt's timeout bounds a streamed reply until its headers arrive; a body that keeps
/// arriving after that has until the operation's deadline.
#[tokio::test]
async fn a_streamed_body_may_outlast_the_attempt_timeout_within_the_deadline() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let body = vec![b's'; 2_048];
            let server = StubServer::start(vec![
                StubResponse::chunked(200, body.clone(), 512).paced(Duration::from_millis(150)),
            ])
            .await;

            let stream = capped_client()
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_millis(200),
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await
                .expect("the headers arrive inside the attempt's timeout");

            let read = stream
                .bytes()
                .await
                .expect("a body arriving steadily has until the deadline");

            assert_eq!(read.as_ref(), body.as_slice());
        })
        .await;
}

/// A streamed body still arriving when the operation's deadline passes fails as a body failure, as
/// one that stops partway does.
#[tokio::test]
async fn a_streamed_body_still_arriving_at_the_deadline_fails() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![
                StubResponse::chunked(200, vec![b's'; 2_048], 512)
                    .paced(Duration::from_millis(300)),
            ])
            .await;

            let mut stream = capped_client()
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(1),
                    tokio::time::Instant::now() + Duration::from_millis(500),
                )
                .await
                .expect("the headers arrive inside the attempt's timeout");

            let error = loop {
                match stream.chunk().await {
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("a body outlasting the deadline must not read to its end"),
                    Err(error) => break error,
                }
            };

            assert!(
                matches!(error, TransportError::Body(_)),
                "unexpected failure: {error:?}"
            );
        })
        .await;
}

/// Reading part of a body in chunks and the rest into one buffer is bounded by the same cap.
///
/// The two readers count separately, so the buffered one starts from what the chunked one left;
/// starting from the whole allowance would hand over twice the cap for one body.
#[tokio::test]
async fn a_body_read_by_both_paths_is_capped_once() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![oversized_chunked_reply()]).await;

            let mut stream = capped_client()
                .send_streaming(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await
                .expect("the response arrives before its body is read");

            let mut read = 0u64;
            while read <= CAP / 2 {
                read += stream
                    .chunk()
                    .await
                    .expect("a read inside the cap is not refused")
                    .expect("the body is longer than half the cap")
                    .len() as u64;
            }

            let error = stream
                .bytes()
                .await
                .expect_err("the rest of the body passes what is left of the cap");

            assert!(
                matches!(error, TransportError::Protocol(_)),
                "unexpected failure: {error:?}"
            );
        })
        .await;
}
