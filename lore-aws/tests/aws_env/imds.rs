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
        assert_eq!(
            provider
                .region(&std::sync::atomic::AtomicBool::new(false))
                .await
                .expect("disabled declines"),
            None
        );
    })
    .await;
}

mod instance_region_resolve {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_aws::aws::error::TransportError;

    use super::*;
    use crate::test_server::StubResponse;
    use crate::test_server::StubServer;

    /// A stub metadata service that issues a token and then names `region`.
    async fn metadata_service(region: &str) -> StubServer {
        StubServer::start(vec![
            StubResponse::ok("imds-token"),
            StubResponse::ok(region),
        ])
        .await
    }

    /// A provider for the metadata service at `base`, counting each time one is built.
    fn provider_at<'a>(
        base: &'a str,
        built: &'a AtomicUsize,
    ) -> impl FnOnce() -> Result<ImdsProvider, TransportError> + 'a {
        move || {
            built.fetch_add(1, Ordering::Relaxed);
            Ok(ImdsProvider::at(
                HttpClient::for_metadata()?,
                base.trim_end_matches('/').to_owned(),
            ))
        }
    }

    /// The setting names the region over the environment, and the environment over the metadata
    /// service, which is asked only once neither names one.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_setting_then_the_environment_come_before_the_metadata_service() {
        let server = metadata_service("eu-west-3").await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", Some("env-region")),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                assert_eq!(
                    region
                        .resolve(Some("setting-region"), provider_at(&base, &built))
                        .await
                        .as_deref(),
                    Some("setting-region")
                );
                assert_eq!(
                    region
                        .resolve(None, provider_at(&base, &built))
                        .await
                        .as_deref(),
                    Some("env-region")
                );
            },
        )
        .await;
        assert_eq!(server.request_count(), 0);

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                assert_eq!(
                    region
                        .resolve(None, provider_at(&base, &built))
                        .await
                        .as_deref(),
                    Some("eu-west-3")
                );
            },
        )
        .await;
        assert_eq!(built.load(Ordering::Relaxed), 1);
    }

    /// Resolutions in flight together, and those after them, share the one answer.
    #[tokio::test]
    #[serial_test::serial]
    async fn one_lookup_serves_every_resolution() {
        let server = metadata_service("eu-west-3").await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                let (first, second, third) = tokio::join!(
                    region.resolve(None, provider_at(&base, &built)),
                    region.resolve(None, provider_at(&base, &built)),
                    region.resolve(None, provider_at(&base, &built)),
                );
                let later = region.resolve(None, provider_at(&base, &built)).await;

                for resolved in [first, second, third, later] {
                    assert_eq!(resolved.as_deref(), Some("eu-west-3"));
                }
            },
        )
        .await;

        assert_eq!(built.load(Ordering::Relaxed), 1);
        assert_eq!(server.request_count(), 2);
    }

    /// `AWS_EC2_METADATA_DISABLED` holds even over an answer the lookup already kept.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_disabled_metadata_service_gives_no_region() {
        let server = metadata_service("eu-west-3").await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        for (disabled, expected) in [
            (Some("true"), None),
            (None, Some("eu-west-3")),
            (Some("TRUE"), None),
        ] {
            temp_env::async_with_vars(
                [
                    ("AWS_REGION", None::<&str>),
                    ("AWS_DEFAULT_REGION", None),
                    ("AWS_EC2_METADATA_DISABLED", disabled),
                ],
                async {
                    assert_eq!(
                        region
                            .resolve(None, provider_at(&base, &built))
                            .await
                            .as_deref(),
                        expected,
                        "with AWS_EC2_METADATA_DISABLED={disabled:?}"
                    );
                },
            )
            .await;
        }

        assert_eq!(built.load(Ordering::Relaxed), 1);
        assert_eq!(server.request_count(), 2);
    }

    /// A region read the metadata service sheds is retried within the same resolution.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_shed_read_is_retried() {
        let server = StubServer::start(vec![
            StubResponse::ok("imds-token"),
            StubResponse::empty(503),
            StubResponse::ok("imds-token"),
            StubResponse::ok("eu-west-3"),
        ])
        .await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                assert_eq!(
                    region
                        .resolve(None, provider_at(&base, &built))
                        .await
                        .as_deref(),
                    Some("eu-west-3")
                );
            },
        )
        .await;

        assert_eq!(built.load(Ordering::Relaxed), 1);
        assert_eq!(server.request_count(), 4);
    }

    /// A resolution whose every attempt is shed gives no region, and that failure is not kept: the
    /// next resolution asks again.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_lookup_shed_on_every_attempt_is_not_kept() {
        let mut script = Vec::new();
        for _ in 0..3 {
            script.push(StubResponse::ok("imds-token"));
            script.push(StubResponse::empty(503));
        }
        script.push(StubResponse::ok("imds-token"));
        script.push(StubResponse::ok("eu-west-3"));
        let server = StubServer::start(script).await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                assert_eq!(region.resolve(None, provider_at(&base, &built)).await, None);
                assert_eq!(server.request_count(), 6);

                assert_eq!(
                    region
                        .resolve(None, provider_at(&base, &built))
                        .await
                        .as_deref(),
                    Some("eu-west-3")
                );
            },
        )
        .await;

        assert_eq!(built.load(Ordering::Relaxed), 2);
        assert_eq!(server.request_count(), 8);
    }

    /// A token request the service sheds is retried: an answer of any kind is a host on EC2.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_shed_token_request_is_retried() {
        let server = StubServer::start(vec![
            StubResponse::empty(503),
            StubResponse::ok("imds-token"),
            StubResponse::ok("eu-west-3"),
        ])
        .await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                assert_eq!(
                    region
                        .resolve(None, provider_at(&base, &built))
                        .await
                        .as_deref(),
                    Some("eu-west-3")
                );
            },
        )
        .await;

        assert_eq!(server.request_count(), 3);
    }

    /// Once the service has answered, a token request it leaves unanswered is the service being
    /// slow, not a host off EC2, so it is retried rather than kept as no region.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_unanswered_token_request_after_an_answer_is_retried() {
        let server = StubServer::start(vec![
            StubResponse::ok("imds-token"),
            StubResponse::empty(503),
            StubResponse::hangup(),
            StubResponse::ok("imds-token"),
            StubResponse::ok("eu-west-3"),
        ])
        .await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                assert_eq!(
                    region
                        .resolve(None, provider_at(&base, &built))
                        .await
                        .as_deref(),
                    Some("eu-west-3")
                );
            },
        )
        .await;

        assert_eq!(built.load(Ordering::Relaxed), 1);
    }

    /// A lookup that was shed on every attempt leaves the service known to be there, so the next
    /// lookup takes an unanswered token request for a slow service and keeps nothing.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_service_that_answered_once_is_never_taken_for_absent() {
        let mut script = Vec::new();
        for _ in 0..3 {
            script.push(StubResponse::empty(503));
        }
        script.push(StubResponse::hangup());
        let server = StubServer::start(script).await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                // Every attempt is shed, then every request goes unanswered: neither lookup keeps
                // anything, so each resolution builds a provider of its own.
                for _ in 0..3 {
                    assert_eq!(region.resolve(None, provider_at(&base, &built)).await, None);
                }
            },
        )
        .await;

        assert_eq!(built.load(Ordering::Relaxed), 3);
        assert_eq!(server.request_count(), 9);
    }

    /// A refusal is an answer asking again would not change, so it is kept as no region without a
    /// retry.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_refused_read_is_kept_as_no_region() {
        let server = StubServer::start(vec![
            StubResponse::ok("imds-token"),
            StubResponse::empty(404),
        ])
        .await;
        let base = server.endpoint().to_string();
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                for _ in 0..2 {
                    assert_eq!(region.resolve(None, provider_at(&base, &built)).await, None);
                }
            },
        )
        .await;

        assert_eq!(built.load(Ordering::Relaxed), 1);
        assert_eq!(server.request_count(), 2);
    }

    /// Off EC2 the metadata address answers nothing, and that absence is kept, so later
    /// resolutions do not wait on the address again.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_metadata_service_gives_no_region_and_is_asked_once() {
        let built = AtomicUsize::new(0);
        let region = InstanceRegion::new();

        temp_env::async_with_vars(
            [
                ("AWS_REGION", None::<&str>),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_EC2_METADATA_DISABLED", None),
            ],
            async {
                // Port 1 refuses at once, where a missing link-local address waits out the
                // metadata timeout.
                for _ in 0..2 {
                    assert_eq!(
                        region
                            .resolve(None, provider_at("http://127.0.0.1:1", &built))
                            .await,
                        None
                    );
                }
            },
        )
        .await;

        assert_eq!(built.load(Ordering::Relaxed), 1);
    }
}
