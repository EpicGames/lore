// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::aws::creds::CredentialsError;
use lore_aws::aws::creds::ProvideCredentials;
use lore_aws::aws::http::HttpClient;
use lore_aws::aws::sts::*;

#[test]
#[serial_test::serial]
fn the_endpoint_is_regional_unless_overridden() {
    temp_env::with_vars(
        [
            ("AWS_ENDPOINT_URL_STS", None::<&str>),
            ("AWS_ENDPOINT_URL", None::<&str>),
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", None::<&str>),
        ],
        || {
            let provider = WebIdentityProvider::new(HttpClient::for_sts().unwrap(), "eu-west-1");
            assert_eq!(provider.endpoint(), "https://sts.eu-west-1.amazonaws.com");

            let regionless = WebIdentityProvider::new(HttpClient::for_sts().unwrap(), "");
            assert_eq!(regionless.endpoint(), "https://sts.amazonaws.com");

            let china = WebIdentityProvider::new(HttpClient::for_sts().unwrap(), "cn-north-1");
            assert_eq!(china.endpoint(), "https://sts.cn-north-1.amazonaws.com.cn");
        },
    );

    temp_env::with_vars(
        [
            ("AWS_ENDPOINT_URL_STS", None),
            ("AWS_ENDPOINT_URL", Some("http://localhost:4566")),
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", None),
        ],
        || {
            let provider = WebIdentityProvider::new(HttpClient::for_sts().unwrap(), "eu-west-1");
            assert_eq!(provider.endpoint(), "http://localhost:4566");
        },
    );

    temp_env::with_vars(
        [
            ("AWS_ENDPOINT_URL_STS", Some("http://localhost:4567")),
            ("AWS_ENDPOINT_URL", Some("http://localhost:4566")),
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", None),
        ],
        || {
            let provider = WebIdentityProvider::new(HttpClient::for_sts().unwrap(), "eu-west-1");
            assert_eq!(provider.endpoint(), "http://localhost:4567");
        },
    );
}

#[tokio::test]
#[serial_test::serial]
async fn no_token_file_declines_and_a_token_without_a_role_fails() {
    temp_env::async_with_vars(
        [
            ("AWS_WEB_IDENTITY_TOKEN_FILE", None::<&str>),
            ("AWS_ROLE_ARN", None::<&str>),
        ],
        async {
            let provider = WebIdentityProvider::new(HttpClient::for_sts().unwrap(), "eu-west-1");

            assert!(matches!(
                provider.provide().await,
                Err(CredentialsError::NotProvided("web-identity"))
            ));
        },
    )
    .await;

    temp_env::async_with_vars(
        [
            ("AWS_WEB_IDENTITY_TOKEN_FILE", Some("/nonexistent/token")),
            ("AWS_ROLE_ARN", None),
        ],
        async {
            let provider = WebIdentityProvider::new(HttpClient::for_sts().unwrap(), "eu-west-1");

            let error = provider.provide().await.expect_err("half configured");
            assert!(
                matches!(
                    error,
                    CredentialsError::Failed {
                        provider: "web-identity",
                        ..
                    }
                ),
                "got: {error:?}"
            );
        },
    )
    .await;
}
