// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::aws::creds::*;

/// A variable set to nothing is how a wrapper clears it, so it reads as unset. Reading it as an
/// id whose secret has gone missing would warn on every resolution and, when nothing later in
/// the chain answers either, report that as the cause.
#[tokio::test]
#[serial_test::serial]
async fn the_environment_declines_when_cleared_and_fails_when_half_set() {
    temp_env::async_with_vars(
        [
            ("AWS_ACCESS_KEY_ID", Some("AKIDENV")),
            ("AWS_SECRET_ACCESS_KEY", Some("secret-env")),
            ("AWS_SESSION_TOKEN", None),
        ],
        async {
            let resolved = EnvProvider.provide().await.expect("a full pair resolves");

            assert_eq!(&*resolved.access_key_id, "AKIDENV");
            assert_eq!(resolved.session_token, None);
        },
    )
    .await;

    temp_env::async_with_vars(
        [
            ("AWS_ACCESS_KEY_ID", Some("")),
            ("AWS_SECRET_ACCESS_KEY", None),
        ],
        async {
            assert!(matches!(
                EnvProvider.provide().await,
                Err(CredentialsError::NotProvided("env"))
            ));
        },
    )
    .await;

    temp_env::async_with_vars(
        [
            ("AWS_ACCESS_KEY_ID", Some("AKIDENV")),
            ("AWS_SECRET_ACCESS_KEY", None),
        ],
        async {
            let error = EnvProvider.provide().await.expect_err("half configured");

            assert!(
                matches!(
                    error,
                    CredentialsError::Failed {
                        provider: "env",
                        ..
                    }
                ),
                "got: {error:?}"
            );
        },
    )
    .await;
}
