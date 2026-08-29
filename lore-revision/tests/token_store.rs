// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod tests {
    use std::sync::Arc;
    use std::sync::OnceLock;

    use keyring::mock;
    use keyring::set_default_credential_builder;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_credential::token_store;
    use lore_credential::token_store::tokens_only_for_recipient_domain;
    use lore_credential::token_store::vulnerable_all_tokens;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::lore::RepositoryId;

    include!("helper.rs");

    const AUTH_ENDPOINT: &str = "http://storeload.auth.example.com";
    static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    async fn sequential_mutex_lock() -> tokio::sync::MutexGuard<'static, ()> {
        TEST_MUTEX
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    async fn setup_test_env() -> (Arc<ExecutionContext>, TempDir) {
        let auth_dir = generate_tempdir();
        unsafe {
            std::env::set_var("LORE_AUTH_PATH", auth_dir.display().to_string());
        }
        set_default_credential_builder(mock::default_credential_builder());
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let _repository_id = RepositoryId::default();
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities");
                for identity in identities {
                    token_store::remove_user_token(AUTH_ENDPOINT, identity.as_str())
                        .await
                        .expect("Failed to remove previous identity");
                }
            }))
            .await
            .expect("Task failure");
        (execution, auth_dir)
    }

    #[tokio::test]
    async fn load_identities() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity0";
                let same_identity = "identity0";

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities");
                assert_eq!(identities.len(), 0);

                token_store::store_user_token(AUTH_ENDPOINT, identity, "some-token-0", vec![])
                    .await
                    .expect("Failed to store first token");

                token_store::store_user_token(AUTH_ENDPOINT, same_identity, "some-token-1", vec![])
                    .await
                    .expect("Failed to store second token");

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities after store");
                assert_eq!(identities.len(), 1);
                assert!(identities.iter().any(|item| item.as_str() == identity));

                token_store::remove_user_token(AUTH_ENDPOINT, identity)
                    .await
                    .expect("Failed to remove token");

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities after store");
                assert_eq!(identities.len(), 0);
            }))
            .await
            .expect("Task failure");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity0";
                let other_identity = "identity1";

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities");
                assert_eq!(identities.len(), 0);

                token_store::store_user_token(AUTH_ENDPOINT, identity, "some-token-0", vec![])
                    .await
                    .expect("Failed to store first token");

                token_store::store_user_token(
                    AUTH_ENDPOINT,
                    other_identity,
                    "some-token-1",
                    vec![],
                )
                .await
                .expect("Failed to store second token");

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities after store");
                assert_eq!(identities.len(), 2);
                assert!(identities.iter().any(|item| item.as_str() == identity));
                assert!(
                    identities
                        .iter()
                        .any(|item| item.as_str() == other_identity)
                );

                token_store::remove_user_token(AUTH_ENDPOINT, identity)
                    .await
                    .expect("Failed to remove 'identity' token");
                token_store::remove_user_token(AUTH_ENDPOINT, other_identity)
                    .await
                    .expect("Failed to remove 'other_identity' token");

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities after store");
                assert_eq!(identities.len(), 0);
            }))
            .await
            .expect("Task failure");
    }

    #[tokio::test]
    async fn store_load_token() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity0";
                let same_identity = "identity0";
                let token = "some-token-0";
                let replaced_token = "some-token-0";

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities");
                assert_eq!(identities.len(), 0);

                token_store::store_user_token(AUTH_ENDPOINT, identity, token, vec![])
                    .await
                    .expect("Failed to store first token");

                token_store::store_user_token(AUTH_ENDPOINT, same_identity, replaced_token, vec![])
                    .await
                    .expect("Failed to store first token");

                let token = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect("Failed to load token after store same identity");
                assert_eq!(token.as_str(), replaced_token);

                token_store::remove_user_token(AUTH_ENDPOINT, identity)
                    .await
                    .expect("Failed to remove token");

                let _ = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect_err("Removed token still available");
            }))
            .await
            .expect("Task failure");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity0";
                let other_identity = "identity1";
                let token = "some-token-0";
                let other_token = "some-token-1";

                let identities = token_store::load_identities(AUTH_ENDPOINT)
                    .await
                    .expect("Failed to get identities");
                assert_eq!(identities.len(), 0);

                token_store::store_user_token(AUTH_ENDPOINT, identity, token, vec![])
                    .await
                    .expect("Failed to store 'identity' token");

                token_store::store_user_token(AUTH_ENDPOINT, other_identity, other_token, vec![])
                    .await
                    .expect("Failed to store 'other_identity' token");

                let found_token = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect("Failed to load token after store other identity");
                assert_eq!(found_token.as_str(), token);

                let found_token = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    other_identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect("Failed to load other identity token after store");
                assert_eq!(found_token.as_str(), other_token);

                token_store::remove_user_token(AUTH_ENDPOINT, identity)
                    .await
                    .expect("Failed to remove 'identity token");
                token_store::remove_user_token(AUTH_ENDPOINT, other_identity)
                    .await
                    .expect("Failed to remove 'other_identity' token");

                let _ = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect_err(format!("Removed token is still available for {identity}").as_str());

                let _ = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    other_identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect_err(
                    format!("Removed token is still available for {other_identity}").as_str(),
                );
            }))
            .await
            .expect("Task failure");
    }

    #[tokio::test]
    async fn can_filter_tokens_by_specific_domain() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity1";
                let audiences = vec![
                    "repo-service.example.com".into(),
                    "notification-service.example.com".into(),
                    "auth-service.example.com".into(),
                ];

                token_store::store_user_token(
                    AUTH_ENDPOINT,
                    identity,
                    "some-token",
                    audiences.clone(),
                )
                .await
                .expect("Failed to store token");

                // prove we do not get the token for a wrong audience
                let load_error = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("aud1".into()),
                )
                .await
                .unwrap_err();
                assert!(load_error.is_token_not_found());

                // get token by its audience
                for domain in audiences {
                    token_store::load_user_token_from_store(
                        AUTH_ENDPOINT,
                        identity,
                        tokens_only_for_recipient_domain(domain.clone()),
                    )
                    .await
                    .unwrap_or_else(|_| panic!("Failed to load token for audience {domain}"));
                }

                //get token by its issuing endpoint
                token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("storeload.auth.example.com".to_string()),
                )
                .await
                .expect("Failed to get token by issuing endpoint domain")
            }))
            .await
            .expect("Task failure");
    }

    #[tokio::test]
    async fn can_get_token_by_subdomain() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity1";
                let audiences = vec![".example.com".into()];

                token_store::store_user_token(
                    AUTH_ENDPOINT,
                    identity,
                    "some-token",
                    audiences.clone(),
                )
                .await
                .expect("Failed to store token");

                // prove we do not get the token for a wrong audience
                let load_error = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("aud1".into()),
                )
                .await
                .unwrap_err();
                assert!(load_error.is_token_not_found());

                token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("lore.example.com".to_string()),
                )
                .await
                .expect("Could not load token")
            }))
            .await
            .expect("Task failure");
    }

    #[tokio::test]
    async fn can_get_tokens_regardless_of_domain() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity4";
                let audiences = vec![
                    "repo-service.example.com".into(),
                    "notification-service.example.com".into(),
                    "auth-service.example.com".into(),
                ];

                token_store::store_user_token(
                    AUTH_ENDPOINT,
                    identity,
                    "some-token",
                    audiences.clone(),
                )
                .await
                .expect("Failed to store token");

                token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect("Failed to load token");
            }))
            .await
            .expect("Task failure");
    }

    #[tokio::test]
    async fn can_upsert_identity() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "identity2";
                let token = "some-token-0";

                token_store::store_user_token(AUTH_ENDPOINT, identity, token, vec!["aud1".into()])
                    .await
                    .expect("Failed to store first token");

                // prove the token gets updated by first trying to load it filtering for 'aud2'
                let load_error = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("aud2".into()),
                )
                .await
                .unwrap_err();
                assert!(load_error.is_token_not_found());

                // upset the same token with new audience
                token_store::store_user_token(AUTH_ENDPOINT, identity, token, vec!["aud2".into()])
                    .await
                    .expect("Failed to store second token");

                // token can now be loaded
                let loaded_token = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("aud2".into()),
                )
                .await
                .expect("Failed to load token after store same identity");
                assert_eq!(loaded_token.as_str(), token);
            }))
            .await
            .expect("Task failure");
    }

    /// A JWT naming "alice", unexpired (`exp` 2000000000).
    /// `{"iss":"lore","sub":"alice","name":"Alice","exp":2000000000,"aud":["example.com"]}`
    const SUPPLIED_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

    /// An access token with no identity token beside it must not fall back to
    /// the store: the caller is working from tokens they supplied, and the
    /// stored identities came from a login this call is not using. Proven
    /// against a store that does hold a token for the identity, so a regression
    /// to the fallback is visible here.
    #[tokio::test]
    async fn access_token_alone_does_not_fall_back_to_the_store() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "supplied-credentials-identity";

                token_store::store_user_token(
                    AUTH_ENDPOINT,
                    identity,
                    "stored-token",
                    vec!["example.com".into()],
                )
                .await
                .expect("Failed to store token");

                // Nothing supplied: the stored token answers, so the fixture is real.
                let stored = token_store::load_user_token(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("example.com".into()),
                    "",
                    "",
                )
                .await
                .expect("the stored token is found when nothing is supplied");
                assert_eq!(stored, "stored-token");

                // An access token supplied instead: the same lookup refuses.
                let refused = token_store::load_user_token(
                    AUTH_ENDPOINT,
                    identity,
                    tokens_only_for_recipient_domain("example.com".into()),
                    "",
                    "access-token",
                )
                .await
                .unwrap_err();
                assert!(
                    refused.is_token_not_found(),
                    "an access token must not fall back to the stored identity"
                );
            }))
            .await
            .expect("Task failure");
    }

    /// When called with an externally supplied `identity_token` or access token,
    /// the on-disk token store is bypassed for both reads and writes. We do not
    /// want to store authz exchange results for supplied identity tokens, and
    /// do not want to read on-disk authz tokens for externally supplied tokens,
    /// since the stored tokens have all originated from other login mechanisms.
    /// We do not want to mix externally provided tokens with internal login
    /// results.
    ///
    /// `AUTH_ENDPOINT` has an `http` scheme, which no `Authentication`
    /// implementation is registered for, so the exchange the supplied call falls
    /// through to fails before any network call.
    #[tokio::test]
    async fn a_supplied_identity_token_bypasses_the_stored_authorization_token() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let identity = "supplied-authz-identity";
                let repository: RepositoryId = "00112233445566778899aabbccddeeff"
                    .parse()
                    .expect("a valid repository id");

                // An authorization token in the store, keyed as `exchange` keys them.
                let store_key = format!("{AUTH_ENDPOINT}/{repository}");
                token_store::store_user_token(
                    &store_key,
                    identity,
                    SUPPLIED_JWT,
                    vec!["example.com".into()],
                )
                .await
                .expect("Failed to store authorization token");

                // Nothing supplied: the stored authorization token answers.
                let stored = lore_transport::auth::exchange::exchange(
                    AUTH_ENDPOINT,
                    identity,
                    repository,
                    "example.com".to_string(),
                    "",
                    "",
                )
                .await
                .expect("the stored authorization token is used");
                assert_eq!(stored, SUPPLIED_JWT);

                // An identity token supplied: neither the store nor the entry the
                // call above cached answers, so this goes on to exchange and
                // fails on the unregistered scheme.
                let result = lore_transport::auth::exchange::exchange(
                    AUTH_ENDPOINT,
                    identity,
                    repository,
                    "example.com".to_string(),
                    SUPPLIED_JWT,
                    "",
                )
                .await;
                assert!(
                    result.is_err(),
                    "a supplied identity token must not be served a stored authorization token"
                );
            }))
            .await
            .expect("Task failure");
    }
}
