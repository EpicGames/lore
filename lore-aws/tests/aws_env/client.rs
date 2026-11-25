// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::aws::*;

#[test]
#[serial_test::serial]
fn an_explicit_region_wins_over_the_environment() {
    temp_env::with_vars([("AWS_REGION", Some("env-region"))], || {
        assert_eq!(
            resolve_region(Some("explicit-region")).as_deref(),
            Some("explicit-region")
        );
    });
}

#[test]
#[serial_test::serial]
fn the_region_falls_back_through_the_environment() {
    temp_env::with_vars(
        [
            ("AWS_REGION", Some("primary")),
            ("AWS_DEFAULT_REGION", Some("fallback")),
        ],
        || {
            assert_eq!(resolve_region(None).as_deref(), Some("primary"));
        },
    );

    temp_env::with_vars(
        [
            ("AWS_REGION", None),
            ("AWS_DEFAULT_REGION", Some("fallback")),
        ],
        || {
            assert_eq!(resolve_region(None).as_deref(), Some("fallback"));
        },
    );

    temp_env::with_vars(
        [
            ("AWS_REGION", None::<&str>),
            ("AWS_DEFAULT_REGION", None::<&str>),
        ],
        || {
            assert_eq!(resolve_region(None), None);
            // An empty setting is not a region.
            assert_eq!(resolve_region(Some("")), None);
        },
    );
}

#[test]
#[serial_test::serial]
fn an_endpoint_is_the_setting_then_the_service_variable_then_the_shared_one() {
    temp_env::with_vars(
        [
            ("AWS_ENDPOINT_URL_DYNAMODB", Some("http://service:1")),
            ("AWS_ENDPOINT_URL", Some("http://shared:2")),
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", None),
        ],
        || {
            assert_eq!(
                configured_endpoint(Some("http://setting:0"), "dynamodb").as_deref(),
                Some("http://setting:0")
            );
            assert_eq!(
                configured_endpoint(None, "dynamodb").as_deref(),
                Some("http://service:1")
            );
            assert_eq!(
                configured_endpoint(None, "s3").as_deref(),
                Some("http://shared:2")
            );
        },
    );

    temp_env::with_vars(
        [
            ("AWS_ENDPOINT_URL_DYNAMODB", None::<&str>),
            ("AWS_ENDPOINT_URL", None::<&str>),
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", None::<&str>),
        ],
        || {
            assert_eq!(configured_endpoint(None, "dynamodb"), None);
            assert_eq!(configured_endpoint(Some(""), "dynamodb"), None);
        },
    );
}

#[test]
#[serial_test::serial]
fn ignoring_configured_endpoints_skips_the_variables_but_not_the_setting() {
    temp_env::with_vars(
        [
            ("AWS_ENDPOINT_URL_S3", Some("http://service:1")),
            ("AWS_ENDPOINT_URL", Some("http://shared:2")),
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", Some("TRUE")),
        ],
        || {
            assert_eq!(configured_endpoint(None, "s3"), None);
            assert_eq!(
                configured_endpoint(Some("http://setting:0"), "s3").as_deref(),
                Some("http://setting:0")
            );
        },
    );
}
