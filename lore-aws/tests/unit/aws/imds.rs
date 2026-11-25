// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use http::HeaderValue;
use http::StatusCode;
use lore_aws::aws::creds::CredentialsError;
use lore_aws::aws::http::HttpClient;
use lore_aws::aws::imds::*;
use lore_base::runtime::LORE_CONTEXT;
use url::Url;

use crate::aws::test_server::StubResponse;
use crate::aws::test_server::StubServer;
use crate::aws::utc;

/// An address that refuses connections at once, so a test measures how an unreachable endpoint is
/// classified rather than how long a link-local timeout takes.
const REFUSING: &str = "http://127.0.0.1:1";

/// The stub's endpoint as an IMDS base, which is written without a trailing slash.
fn imds_base(server: &StubServer) -> String {
    server.endpoint().as_str().trim_end_matches('/').to_owned()
}

#[test]
fn parses_an_imds_credentials_response() {
    let body = br#"{
        "Code": "Success",
        "LastUpdated": "2026-08-12T09:00:00Z",
        "Type": "AWS-HMAC",
        "AccessKeyId": "AKIDIMDS",
        "SecretAccessKey": "secret-imds",
        "Token": "token-imds",
        "Expiration": "2026-08-12T15:00:00Z"
    }"#;

    let credentials = parse_metadata_credentials("imds", body).unwrap();

    assert_eq!(&*credentials.access_key_id, "AKIDIMDS");
    assert_eq!(&*credentials.secret_access_key, "secret-imds");
    assert_eq!(credentials.session_token.as_deref(), Some("token-imds"));
    assert_eq!(credentials.expires_at, Some(utc(2026, 8, 12, 15, 0, 0)));
}

/// The container endpoint sends no `Code`. Its absence must not read as failure.
#[test]
fn parses_a_container_credentials_response_without_a_code() {
    let body = br#"{
        "AccessKeyId": "AKIDECS",
        "SecretAccessKey": "secret-ecs",
        "Token": "token-ecs",
        "Expiration": "2026-08-12T15:00:00Z"
    }"#;

    let credentials = parse_metadata_credentials("container", body).unwrap();

    assert_eq!(&*credentials.access_key_id, "AKIDECS");
}

#[test]
fn a_failure_code_is_reported_rather_than_ignored() {
    let body = br#"{"Code": "AssumeRoleUnauthorizedAccess", "Message": "nope"}"#;

    let error = parse_metadata_credentials("imds", body).expect_err("not a success");

    assert!(
        matches!(
            error,
            CredentialsError::Failed {
                provider: "imds",
                ..
            }
        ),
        "got: {error:?}"
    );
}

#[test]
fn a_response_missing_its_keys_is_a_failure_not_empty_credentials() {
    for body in [
        &br#"{"Code": "Success"}"#[..],
        &br#"{"AccessKeyId": "A"}"#[..],
        &br#"{"SecretAccessKey": "S"}"#[..],
    ] {
        assert!(parse_metadata_credentials("imds", body).is_err());
    }
}

#[test]
fn hostile_metadata_bodies_are_errors_not_panics() {
    for body in [
        &b""[..],
        &b"not json"[..],
        &b"{"[..],
        &br#"{"AccessKeyId": 7, "SecretAccessKey": "s"}"#[..],
        &br#"[]"#[..],
    ] {
        assert!(parse_metadata_credentials("imds", body).is_err());
    }
}

/// An unreadable expiry must not become "never expires", which would pin a process to
/// credentials it never refreshes and that the service stops accepting when the session ends.
#[test]
fn an_unreadable_expiry_is_a_failure() {
    let body = br#"{"AccessKeyId":"A","SecretAccessKey":"S","Expiration":"soon"}"#;

    let error = parse_metadata_credentials("imds", body).expect_err("not an expiry");

    assert!(matches!(error, CredentialsError::Failed { .. }), "{error}");
}

/// The container endpoint may answer without an expiry, which is the one way credentials that
/// do not expire are read.
#[test]
fn an_absent_expiry_is_accepted_as_no_expiry() {
    let body = br#"{"AccessKeyId":"A","SecretAccessKey":"S"}"#;

    let credentials = parse_metadata_credentials("container", body).unwrap();

    assert_eq!(credentials.expires_at, None);
}

/// The provider sends its authorization token to whatever the environment names, so an
/// unrestricted host would make one environment variable enough to leak the role.
#[test]
fn only_loopback_and_the_agent_addresses_may_be_read_over_plain_http() {
    for permitted in [
        "http://127.0.0.1:8080/creds",
        "http://localhost:8080/creds",
        "http://[::1]:8080/creds",
        "http://169.254.170.2/v2/credentials/abc",
        "http://169.254.170.23/v1/credentials",
        "http://[fd00:ec2::23]/v1/credentials",
        // TLS makes the host the caller's problem rather than ours.
        "https://credentials.example.com/creds",
    ] {
        assert!(
            ContainerProvider::is_permitted(&Url::parse(permitted).unwrap()),
            "{permitted} should be permitted"
        );
    }

    for refused in [
        "http://attacker.example.com/creds",
        "http://169.254.169.254/creds",
        "http://10.0.0.1/creds",
        "http://192.168.1.1/creds",
        // A name that resolves to loopback is still a name, and DNS can be changed.
        "http://localhost.attacker.example.com/creds",
    ] {
        assert!(
            !ContainerProvider::is_permitted(&Url::parse(refused).unwrap()),
            "{refused} must be refused"
        );
    }
}

/// A metadata endpoint answering with a shedding status is backed off from, as a shed STS answer
/// is. Any other refusal is the endpoint's decision and fails.
#[test]
fn a_shed_metadata_answer_is_transient_and_a_refusal_is_not() {
    for status in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
    ] {
        let error = refused("imds", status, String::new());

        assert!(
            matches!(
                error,
                CredentialsError::Transient {
                    provider: "imds",
                    ..
                }
            ),
            "{status} is the endpoint shedding: {error:?}"
        );
    }

    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
    ] {
        let error = refused("container", status, String::new());

        assert!(
            matches!(
                error,
                CredentialsError::Failed {
                    provider: "container",
                    ..
                }
            ),
            "{status} is the endpoint refusing: {error:?}"
        );
    }
}

/// A container endpoint that cannot be reached may be reachable later, so the caller backs off.
#[tokio::test]
async fn an_unreachable_container_endpoint_is_transient() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let provider = ContainerProvider::new(HttpClient::for_metadata().unwrap());

            let error = provider
                .fetch(Url::parse(REFUSING).unwrap(), None)
                .await
                .expect_err("nothing answers");

            assert!(
                matches!(
                    error,
                    CredentialsError::Transient {
                        provider: "container",
                        ..
                    }
                ),
                "got: {error:?}"
            );
        })
        .await;
}

/// A container endpoint that sheds the request is backed off from rather than reported.
#[tokio::test]
async fn a_shed_container_answer_is_transient() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::empty(503)]).await;
            let provider = ContainerProvider::new(HttpClient::for_metadata().unwrap());

            let error = provider
                .fetch(server.endpoint(), Some("container-token".to_owned()))
                .await
                .expect_err("the endpoint shed the request");

            assert!(error.is_transient(), "got: {error:?}");
        })
        .await;
}

/// The IMDS token request is the first call a provider makes, and a shed answer to it is backed
/// off from like any other.
#[tokio::test]
async fn a_shed_imds_token_answer_is_transient() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::empty(503)]).await;
            let provider =
                ImdsProvider::at(HttpClient::for_metadata().unwrap(), imds_base(&server));

            let error = provider
                .token()
                .await
                .expect_err("the endpoint shed the request");

            assert!(
                matches!(
                    error,
                    CredentialsError::Transient {
                        provider: "imds",
                        ..
                    }
                ),
                "got: {error:?}"
            );
        })
        .await;
}

/// Once a token was issued the host is on EC2, so a metadata read that cannot reach the service
/// is a transient failure rather than a decline.
#[tokio::test]
async fn an_unreachable_imds_read_is_transient() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let provider =
                ImdsProvider::at(HttpClient::for_metadata().unwrap(), REFUSING.to_owned());
            let token = HeaderValue::from_static("imds-token");

            let error = provider
                .get("/latest/meta-data/iam/security-credentials/", &token)
                .await
                .expect_err("nothing answers");

            assert!(
                matches!(
                    error,
                    CredentialsError::Transient {
                        provider: "imds",
                        ..
                    }
                ),
                "got: {error:?}"
            );
        })
        .await;
}

/// The token IMDS issues is marked sensitive, so no request carrying it shows it in `Debug`.
#[tokio::test]
async fn the_imds_token_is_marked_sensitive() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::ok("imds-session-token")]).await;
            let provider =
                ImdsProvider::at(HttpClient::for_metadata().unwrap(), imds_base(&server));

            let token = provider.token().await.expect("the stub issues a token");

            assert_eq!(token, "imds-session-token");
            assert!(token.is_sensitive());
        })
        .await;
}
