// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::aws::creds::CredentialsError;
use lore_aws::aws::creds::ProvideCredentials;
use lore_aws::aws::http::HttpClient;
use lore_aws::aws::imds::*;

#[test]
#[serial_test::serial]
fn a_relative_container_uri_resolves_against_the_task_endpoint() {
    temp_env::with_vars(
        [
            ("AWS_CONTAINER_CREDENTIALS_FULL_URI", None),
            (
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                Some("/v2/credentials/abc"),
            ),
        ],
        || {
            assert_eq!(
                ContainerProvider::endpoint().as_deref(),
                Some("http://169.254.170.2/v2/credentials/abc")
            );
        },
    );
}

#[test]
#[serial_test::serial]
fn a_full_container_uri_is_used_as_given() {
    temp_env::with_vars(
        [(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            Some("http://localhost:8080/creds"),
        )],
        || {
            assert_eq!(
                ContainerProvider::endpoint().as_deref(),
                Some("http://localhost:8080/creds")
            );
        },
    );
}

#[tokio::test]
#[serial_test::serial]
async fn a_container_uri_pointing_somewhere_else_sends_nothing() {
    temp_env::async_with_vars(
        [
            (
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                Some("http://attacker.example.com/creds"),
            ),
            ("AWS_CONTAINER_AUTHORIZATION_TOKEN", Some("a-real-secret")),
        ],
        async {
            let provider = ContainerProvider::new(HttpClient::for_metadata().unwrap());

            let error = provider.provide().await.expect_err("refused");

            assert!(
                matches!(error, CredentialsError::Failed { .. }),
                "got: {error:?}"
            );
        },
    )
    .await;
}

#[test]
#[serial_test::serial]
fn no_container_variables_means_no_endpoint() {
    temp_env::with_vars(
        [
            ("AWS_CONTAINER_CREDENTIALS_FULL_URI", None::<&str>),
            ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", None::<&str>),
        ],
        || {
            assert_eq!(ContainerProvider::endpoint(), None);
        },
    );
}

#[test]
#[serial_test::serial]
fn imds_can_be_switched_off() {
    temp_env::with_vars([("AWS_EC2_METADATA_DISABLED", Some("true"))], || {
        assert!(ImdsProvider::disabled());
    });
    temp_env::with_vars([("AWS_EC2_METADATA_DISABLED", Some("TRUE"))], || {
        assert!(ImdsProvider::disabled());
    });
    temp_env::with_vars([("AWS_EC2_METADATA_DISABLED", None::<&str>)], || {
        assert!(!ImdsProvider::disabled());
    });
}

/// Off EC2 the metadata address answers nothing, which is not a failure to report: the chain would
/// otherwise name a link-local timeout as the reason a working profile was reached.
#[tokio::test]
#[serial_test::serial]
async fn a_metadata_endpoint_that_does_not_answer_declines() {
    temp_env::async_with_vars(
        [
            ("AWS_EC2_METADATA_DISABLED", None::<&str>),
            // Port 1 refuses at once, so the test measures the classification rather than the
            // two-second timeout that produces it in a deployment.
            (
                "AWS_EC2_METADATA_SERVICE_ENDPOINT",
                Some("http://127.0.0.1:1"),
            ),
        ],
        async {
            let provider = ImdsProvider::new(HttpClient::for_metadata().unwrap());

            let error = provider.provide().await.expect_err("nothing answers");

            assert!(
                matches!(error, CredentialsError::NotProvided("imds")),
                "got: {error:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_disabled_imds_declines_without_touching_the_network() {
    temp_env::async_with_vars([("AWS_EC2_METADATA_DISABLED", Some("true"))], async {
        let provider = ImdsProvider::new(HttpClient::for_metadata().unwrap());

        let error = provider.provide().await.expect_err("disabled");

        assert!(matches!(error, CredentialsError::NotProvided("imds")));
        assert_eq!(provider.region().await, None);
    })
    .await;
}
