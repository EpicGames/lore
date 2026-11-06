// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use lore_transport::auth::oidc::MAX_RESPONSE_BYTES;
use lore_transport::auth::oidc::discovery::*;
use tokio::sync::watch;

const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";

use super::provider::*;

#[tokio::test]
async fn reads_the_endpoints_from_the_document() {
    let provider = spawn_provider(conforming).await;
    let issuer = &provider.issuer;

    let document = discover(issuer).await.expect("discovery succeeds");

    assert_eq!(&document.issuer, issuer);
    assert_eq!(document.token_endpoint, format!("{issuer}/token"));
    assert_eq!(document.jwks_uri, format!("{issuer}/jwks"));
    assert_eq!(
        document.device_authorization_endpoint,
        Some(format!("{issuer}/device"))
    );
    assert_eq!(
        document.revocation_endpoint,
        Some(format!("{issuer}/revoke"))
    );
    assert_eq!(
        document.end_session_endpoint,
        Some(format!("{issuer}/logout"))
    );
}

/// The device authorization, revocation and end-session endpoints are optional.
#[tokio::test]
async fn optional_endpoints_may_be_absent() {
    let provider = spawn_provider(|issuer, _| {
        json(&serde_json::json!({
            "issuer": issuer,
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
        }))
    })
    .await;

    let document = discover(&provider.issuer)
        .await
        .expect("discovery succeeds");

    assert_eq!(document.device_authorization_endpoint, None);
    assert_eq!(document.revocation_endpoint, None);
    assert_eq!(document.end_session_endpoint, None);
}

#[tokio::test]
async fn a_document_naming_another_issuer_is_refused() {
    let provider = spawn_provider(|_, _| {
        json(&document(
            "https://idp.example.com",
            "https://idp.example.com",
        ))
    })
    .await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::IssuerMismatch { .. })),
        "{result:?}"
    );
}

/// A trailing slash is part of the issuer, so a document that drops it names a different one.
#[tokio::test]
async fn the_issuer_must_match_exactly() {
    // A bare origin, since a parsed URL treats `http://host:port` and `http://host:port/` as
    // equal, and only a string comparison tells them apart.
    let provider = spawn_provider(|issuer, _| json(&document(&origin(issuer), issuer))).await;

    let result = discover(&format!("{}/", origin(&provider.issuer))).await;

    assert!(
        matches!(result, Err(DiscoveryError::IssuerMismatch { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_redirect_to_another_origin_is_refused_and_not_followed() {
    let elsewhere = spawn_provider(conforming).await;
    let target = format!("{}{DISCOVERY_PATH}", elsewhere.issuer);
    let provider = spawn_provider(move |_, _| {
        (StatusCode::FOUND, [(header::LOCATION, target.clone())]).into_response()
    })
    .await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::CrossOriginRedirect { .. })),
        "{result:?}"
    );
    assert_eq!(elsewhere.requests.load(Ordering::SeqCst), 0);
}

/// The error names only the target's origin, so credentials in the target's userinfo or
/// query reach neither the error nor the log.
#[tokio::test]
async fn a_cross_origin_redirect_reports_only_the_target_origin() {
    let elsewhere = spawn_provider(conforming).await;
    let target = format!(
        "{}/signed?signature=secret",
        elsewhere
            .issuer
            .replacen("http://", "http://client:secret@", 1)
    );
    let provider = spawn_provider(move |_, _| {
        (StatusCode::FOUND, [(header::LOCATION, target.clone())]).into_response()
    })
    .await;

    let result = discover(&provider.issuer).await;

    let Err(DiscoveryError::CrossOriginRedirect { target_origin, .. }) = result else {
        panic!("{result:?}");
    };
    assert_eq!(target_origin, origin(&elsewhere.issuer));
}

#[tokio::test]
async fn a_redirect_within_the_origin_is_followed() {
    let provider = spawn_provider(|issuer, request| match request {
        0 => (StatusCode::FOUND, [(header::LOCATION, "/moved")]).into_response(),
        _ => conforming(issuer, request),
    })
    .await;

    let document = discover(&provider.issuer)
        .await
        .expect("discovery succeeds");

    assert_eq!(document.issuer, provider.issuer);
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
}

/// Refused before any request is made, so the host need not exist.
#[tokio::test]
async fn a_plain_http_issuer_is_refused() {
    let result = discover("http://idp.example.com").await;

    assert!(
        matches!(result, Err(DiscoveryError::InsecureIssuer { .. })),
        "{result:?}"
    );
}

/// `http://localhost:pass@idp.example.com` names `idp.example.com`, not a loopback host.
#[tokio::test]
async fn an_issuer_with_credentials_is_refused() {
    for issuer in [
        "http://localhost:pass@idp.example.com",
        "https://user:secret@idp.example.com",
    ] {
        let result = discover(issuer).await;
        assert!(
            matches!(result, Err(DiscoveryError::InvalidIssuer)),
            "{issuer}: {result:?}"
        );
    }
}

#[tokio::test]
async fn an_issuer_with_a_query_is_refused() {
    let result = discover("https://idp.example.com?tenant=a").await;

    assert!(
        matches!(result, Err(DiscoveryError::InvalidIssuer)),
        "{result:?}"
    );
}

/// The scheme is case-insensitive, so an `HTTP` loopback issuer may advertise loopback
/// `http` endpoints like an `http` one.
#[tokio::test]
async fn a_loopback_issuer_with_an_uppercase_scheme_permits_loopback_endpoints() {
    let provider =
        spawn_provider(|issuer, _| json(&document(&issuer.replacen("http", "HTTP", 1), issuer)))
            .await;
    let issuer = provider.issuer.replacen("http", "HTTP", 1);

    let document = discover(&issuer).await.expect("discovery succeeds");

    assert_eq!(document.issuer, issuer);
}

#[tokio::test]
async fn an_endpoint_without_https_is_refused() {
    let provider =
        spawn_provider(|issuer, _| json(&document(issuer, "http://idp.example.com"))).await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::InsecureEndpoint { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_endpoint_with_credentials_is_refused() {
    let provider = spawn_provider(|issuer, _| {
        let mut document = document(issuer, issuer);
        document["token_endpoint"] = "https://user:secret@idp.example.com/token".into();
        json(&document)
    })
    .await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::InsecureEndpoint { .. })),
        "{result:?}"
    );
}

/// A 3xx that is not followed and points within the origin is reported as the status it is.
#[tokio::test]
async fn an_unfollowed_redirect_within_the_origin_is_a_status_error() {
    let provider = spawn_provider(|_, _| {
        (
            StatusCode::MULTIPLE_CHOICES,
            [(header::LOCATION, "/elsewhere")],
        )
            .into_response()
    })
    .await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::Status { status: 300, .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_oversized_document_is_refused() {
    let provider = spawn_provider(|_, _| "x".repeat(MAX_RESPONSE_BYTES + 1).into_response()).await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::Oversized { .. })),
        "{result:?}"
    );
}

/// With no `Content-Length` to refuse up front, the read itself enforces the cap.
#[tokio::test]
async fn an_oversized_chunked_document_is_refused_while_reading() {
    let provider = spawn_provider(|_, _| {
        let chunks =
            (0..(MAX_RESPONSE_BYTES / 1024) + 2).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024]));
        Response::new(Body::from_stream(futures::stream::iter(chunks)))
    })
    .await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::Oversized { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_error_status_is_refused() {
    let provider = spawn_provider(|_, _| StatusCode::NOT_FOUND.into_response()).await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::Status { status: 404, .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_malformed_document_is_refused() {
    let provider = spawn_provider(|_, _| "not json".into_response()).await;

    let result = discover(&provider.issuer).await;

    assert!(
        matches!(result, Err(DiscoveryError::Malformed { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn concurrent_calls_for_different_issuers_get_their_own_documents() {
    let one = spawn_provider(conforming).await;
    let other = spawn_provider(conforming).await;

    let (from_one, from_other) = tokio::join!(discover(&one.issuer), discover(&other.issuer));

    let from_one = from_one.expect("first issuer");
    let from_other = from_other.expect("second issuer");
    assert_eq!(from_one.token_endpoint, format!("{}/token", one.issuer));
    assert_eq!(from_other.token_endpoint, format!("{}/token", other.issuer));
}

#[tokio::test]
async fn a_failed_fetch_is_not_cached() {
    let provider = spawn_provider(|issuer, request| match request {
        0 => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        _ => conforming(issuer, request),
    })
    .await;

    let first = discover(&provider.issuer).await;
    assert!(
        matches!(first, Err(DiscoveryError::Status { status: 503, .. })),
        "{first:?}"
    );

    discover(&provider.issuer)
        .await
        .expect("the next call fetches again");
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
}

/// Cancelling the caller that is fetching aborts its request, so the next caller's fetch is
/// the only one running.
#[tokio::test]
async fn cancelling_the_fetching_caller_aborts_its_request() {
    let (open, gate) = watch::channel(false);
    let provider = spawn_gated_provider(gate, conforming).await;

    tokio::select! {
        result = discover(&provider.issuer) => panic!("the provider is gated: {result:?}"),
        () = wait_for_requests(&provider, 1) => {}
    }
    wait_until(
        || provider.in_flight.load(Ordering::SeqCst) == 0,
        "the cancelled request to be dropped",
    )
    .await;

    open.send(true).expect("provider alive");
    discover(&provider.issuer)
        .await
        .expect("the next caller fetches");
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
}
