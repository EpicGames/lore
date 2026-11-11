// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod tests {
    use std::sync::Arc;
    use std::sync::OnceLock;

    use lore_base::env::CallEnvironment;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_credential::token_store;
    use lore_credential::token_store::tokens_only_for_recipient_domain;
    use lore_credential::token_store::vulnerable_all_tokens;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::lore::RepositoryId;
    use lore_revision::relay::EventDispatcher;

    include!("helper.rs");

    const AUTH_ENDPOINT: &str = "http://storeload.auth.example.com";

    /// `AUTH_ENDPOINT` behind an implementation that refuses every grant, so an
    /// exchange that misses the cache and the store fails before any network call.
    fn refusing_endpoint() -> lore_transport::auth::AuthEndpoint {
        lore_transport::auth::AuthEndpoint::new(
            AUTH_ENDPOINT.to_string(),
            lore_transport::auth::AuthEndpoint::none().auth,
        )
    }
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
        keyring_core::set_default_store(
            keyring_core::mock::Store::new().expect("Failed to create mock store"),
        );
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
    async fn load_identities_ignores_trailing_slash() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                token_store::store_user_token(
                    &format!("{AUTH_ENDPOINT}/"),
                    "identity0",
                    "some-token-0",
                    vec![],
                )
                .await
                .expect("Failed to store token");

                let identities = token_store::load_identities(&format!("{AUTH_ENDPOINT}/"))
                    .await
                    .expect("Failed to get identities");
                assert_eq!(identities, vec!["identity0".to_string()]);
            }))
            .await
            .expect("Task failure");
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

    /// Entries are keyed by the endpoint that issued them, so a user who logs
    /// in on the gRPC path, then on the OIDC path, and then switches back
    /// finds the first credentials intact and need not log in a third time.
    #[tokio::test]
    async fn a_token_from_one_endpoint_survives_a_login_at_another() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                const OIDC_ISSUER: &str = "https://issuer.storeload.example.com";
                let identity = "identity0";
                let grpc_token = "grpc-token";
                let oidc_token = "oidc-token";
                let _ = token_store::remove_user_token(OIDC_ISSUER, identity).await;

                token_store::store_user_token(AUTH_ENDPOINT, identity, grpc_token, vec![])
                    .await
                    .expect("Failed to store the gRPC token");
                token_store::store_user_token(OIDC_ISSUER, identity, oidc_token, vec![])
                    .await
                    .expect("Failed to store the OIDC token");

                let found = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect("The gRPC token must still load");
                assert_eq!(found.as_str(), grpc_token);

                let found = token_store::load_user_token_from_store(
                    OIDC_ISSUER,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect("The OIDC token must load");
                assert_eq!(found.as_str(), oidc_token);

                token_store::remove_user_token(OIDC_ISSUER, identity)
                    .await
                    .expect("Failed to remove the OIDC token");
                let found = token_store::load_user_token_from_store(
                    AUTH_ENDPOINT,
                    identity,
                    vulnerable_all_tokens(),
                )
                .await
                .expect("Removing the OIDC token must leave the gRPC token");
                assert_eq!(found.as_str(), grpc_token);

                token_store::remove_user_token(AUTH_ENDPOINT, identity)
                    .await
                    .expect("Failed to remove the gRPC token");
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

    /// A JWT naming "alice", expired (`exp` 1).
    /// `{"iss":"lore","sub":"alice","name":"Alice","exp":1,"aud":["example.com"]}`
    const EXPIRED_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjEsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

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
    /// The endpoint's implementation refuses every grant, so the exchange the
    /// supplied call falls through to fails before any network call.
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
                    &refusing_endpoint(),
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
                // is refused by the endpoint's implementation.
                let result = lore_transport::auth::exchange::exchange(
                    &refusing_endpoint(),
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

    /// The context of a call relayed from a process whose token store is `auth_dir`.
    fn relayed_from(auth_dir: &TempDir) -> Arc<ExecutionContext> {
        Arc::new(
            ExecutionContext::new_client(Default::default(), EventDispatcher::new(None))
                .with_environment(CallEnvironment {
                    values: [None, Some(auth_dir.display().to_string()), None],
                }),
        )
    }

    /// A call relayed from another process reads the token store its caller names, so one
    /// process caches each store apart: an identity stored through one caller's store is not
    /// served to a caller naming another.
    #[tokio::test]
    async fn a_relayed_call_reads_only_its_callers_token_store() {
        let _lock = sequential_mutex_lock().await;

        let (_execution, _auth_dir) = setup_test_env().await;
        let (first_dir, second_dir) = (generate_tempdir(), generate_tempdir());
        let (first, second) = (relayed_from(&first_dir), relayed_from(&second_dir));

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(async move {
                LORE_CONTEXT
                    .scope(first, async {
                        token_store::store_user_token(AUTH_ENDPOINT, "first", "token", vec![])
                            .await
                            .expect("Failed to store token");
                        assert_eq!(
                            token_store::load_identities(AUTH_ENDPOINT)
                                .await
                                .expect("Failed to get identities"),
                            ["first"]
                        );
                    })
                    .await;
                let identities = LORE_CONTEXT
                    .scope(second, token_store::load_identities(AUTH_ENDPOINT))
                    .await
                    .expect("Failed to get identities");
                assert!(
                    identities.is_empty(),
                    "a caller naming another store must not be served this one's identities"
                );
            })
            .await
            .expect("Task failure");
    }

    /// An authorization one caller's token store earned is cached for that store alone: a caller
    /// naming a store that holds nothing for the identity is not served it.
    ///
    /// The endpoint's implementation refuses every grant, so a call that misses the cache and
    /// the store fails before any network call.
    #[tokio::test]
    async fn a_relayed_call_is_served_only_its_callers_authorization() {
        let _lock = sequential_mutex_lock().await;

        let (_execution, _auth_dir) = setup_test_env().await;
        let (first_dir, second_dir) = (generate_tempdir(), generate_tempdir());
        let (first, second) = (relayed_from(&first_dir), relayed_from(&second_dir));
        let identity = "relayed-authz-identity";
        let repository: RepositoryId = "00112233445566778899aabbccddeeff"
            .parse()
            .expect("a valid repository id");
        let exchange = move || async move {
            lore_transport::auth::exchange::exchange(
                &refusing_endpoint(),
                identity,
                repository,
                "example.com".to_string(),
                "",
                "",
            )
            .await
        };

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(async move {
                LORE_CONTEXT
                    .scope(first, async {
                        token_store::store_user_token(
                            &format!("{AUTH_ENDPOINT}/{repository}"),
                            identity,
                            SUPPLIED_JWT,
                            vec!["example.com".into()],
                        )
                        .await
                        .expect("Failed to store authorization token");
                        assert_eq!(
                            exchange().await.expect("the stored authorization is used"),
                            SUPPLIED_JWT
                        );
                    })
                    .await;
                assert!(
                    LORE_CONTEXT.scope(second, exchange()).await.is_err(),
                    "a caller naming another store must not be served this one's authorization"
                );
            })
            .await
            .expect("Task failure");
    }

    /// The stored refresh token's deadline is resolved beside the stored token: the deadline,
    /// 0 for a refresh token without one, and none without a refresh token.
    #[tokio::test]
    async fn the_refresh_deadline_is_resolved_with_the_stored_token() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let users = ["alice".to_string()];
                for (refresh_token, refresh_expires, resolved) in [
                    (
                        Some("refresh-1"),
                        1_900_000_000_000,
                        Some(1_900_000_000_000),
                    ),
                    (Some("refresh-1"), 0, Some(0)),
                    (None, 1_900_000_000_000, None),
                ] {
                    token_store::store_user_token_and_refresh_token(
                        AUTH_ENDPOINT,
                        "alice",
                        SUPPLIED_JWT,
                        vec!["example.com".into()],
                        refresh_token,
                        refresh_expires,
                        None,
                    )
                    .await
                    .expect("Failed to store the login");

                    let [identity] = lore_revision::auth::userinfo::resolve_local_user_info(
                        AUTH_ENDPOINT,
                        &users,
                    )
                    .await
                    .try_into()
                    .expect("one identity");
                    assert!(identity.local_user_info.is_some());
                    assert_eq!(identity.refresh_expires, resolved, "{refresh_token:?}");
                }
            }))
            .await
            .expect("Task failure");
    }

    /// A call that keeps its key in the file `sec-tokenstore_encryption_key` of a new token store
    /// directory, with the directory's guard and the key file's path.
    fn fallback_key_call() -> (Arc<ExecutionContext>, TempDir, std::path::PathBuf) {
        let store_dir = generate_tempdir();
        let key_file = store_dir.child("sec-tokenstore_encryption_key");
        let fallback = Arc::new(
            ExecutionContext::new_client(Default::default(), EventDispatcher::new(None))
                .with_environment(CallEnvironment {
                    values: [
                        None,
                        Some(store_dir.display().to_string()),
                        Some("fallback".to_string()),
                    ],
                }),
        );
        (fallback, store_dir, key_file)
    }

    /// The mode of `file`'s permission bits.
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods)]
    fn mode_of(file: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(file)
            .expect("the key file exists")
            .permissions()
            .mode()
            & 0o777
    }

    /// A key file the token store writes is readable by its owner alone, even one an earlier
    /// version left readable by others.
    #[cfg(unix)]
    #[tokio::test]
    // The fixture writes the key file directly, as an earlier version would have.
    #[allow(clippy::disallowed_methods)]
    async fn the_fallback_key_file_is_restricted_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let (fallback, _store_dir, key_file) = fallback_key_call();

        // A file readable by everyone that holds no usable key, so the store writes a new one.
        std::fs::write(&key_file, [7u8; 3]).expect("Failed to write the key file");
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o644))
            .expect("Failed to widen the key file");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(fallback, async move {
                token_store::store_user_token(AUTH_ENDPOINT, "owner-only", "token", vec![])
                    .await
                    .expect("Failed to store token");
                assert_eq!(
                    std::fs::read(&key_file).expect("the key file exists").len(),
                    32,
                    "the store wrote a new key"
                );
                assert_eq!(
                    mode_of(&key_file),
                    0o600,
                    "the written key is the owner's alone"
                );
            }))
            .await
            .expect("Task failure");
    }

    /// A call carrying `store` for the token store in `store_dir`, a fresh directory.
    fn store_call(store: Option<&str>) -> (Arc<ExecutionContext>, TempDir, std::path::PathBuf) {
        let store_dir = generate_tempdir();
        let key_file = store_dir.child("sec-tokenstore_encryption_key");
        let call = Arc::new(
            ExecutionContext::new_client(Default::default(), EventDispatcher::new(None))
                .with_environment(CallEnvironment {
                    values: [
                        None,
                        Some(store_dir.display().to_string()),
                        store.map(str::to_string),
                    ],
                }),
        );
        (call, store_dir, key_file)
    }

    /// Restores this process's `LORE_AUTH_STORE` when dropped.
    struct ProcessAuthStore(Option<String>);

    impl ProcessAuthStore {
        fn set(value: &str) -> Self {
            let previous = std::env::var("LORE_AUTH_STORE").ok();
            // SAFETY: the tests of this binary run one at a time under `sequential_mutex_lock`.
            unsafe { std::env::set_var("LORE_AUTH_STORE", value) };
            Self(previous)
        }
    }

    impl Drop for ProcessAuthStore {
        fn drop(&mut self) {
            // SAFETY: as in `set`.
            unsafe {
                match &self.0 {
                    Some(value) => std::env::set_var("LORE_AUTH_STORE", value),
                    None => std::env::remove_var("LORE_AUTH_STORE"),
                }
            }
        }
    }

    /// A relayed call that makes no store choice, or chooses the secure store, keeps its key in the
    /// secure store though the process carrying it out has `LORE_AUTH_STORE=fallback`.
    #[tokio::test]
    async fn a_relayed_call_without_a_store_choice_ignores_the_services_choice() {
        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let _process = ProcessAuthStore::set("fallback");

        for store in [None, Some("secure")] {
            let (call, _store_dir, key_file) = store_call(store);
            #[allow(clippy::disallowed_methods)]
            runtime()
                .spawn(LORE_CONTEXT.scope(call, async move {
                    token_store::store_user_token(AUTH_ENDPOINT, "relayed", "token", vec![])
                        .await
                        .expect("Failed to store token");
                    assert!(
                        !key_file.exists(),
                        "a call carrying {store:?} keeps no key file"
                    );
                }))
                .await
                .expect("Task failure");
        }
    }

    /// Calls choosing the secure store and the fallback for one token store directory each use the
    /// key their choice keeps, not one cached for the other.
    #[tokio::test]
    // The fixture writes the fallback key directly.
    #[allow(clippy::disallowed_methods)]
    async fn a_store_choice_has_a_key_of_its_own_for_one_directory() {
        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let (secure, store_dir, key_file) = store_call(Some("secure"));
        let fallback = Arc::new(
            ExecutionContext::new_client(Default::default(), EventDispatcher::new(None))
                .with_environment(CallEnvironment {
                    values: [
                        None,
                        Some(store_dir.display().to_string()),
                        Some("fallback".to_string()),
                    ],
                }),
        );

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(secure.clone(), async move {
                token_store::store_user_token(AUTH_ENDPOINT, "secure-user", "token-s", vec![])
                    .await
                    .expect("Failed to store token");
            }))
            .await
            .expect("Task failure");
        std::fs::write(&key_file, [7u8; 32]).expect("Failed to write the fallback key");
        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(fallback, async move {
                token_store::store_user_token(AUTH_ENDPOINT, "fallback-user", "token-f", vec![])
                    .await
                    .expect("Failed to store token");
            }))
            .await
            .expect("Task failure");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(secure, async move {
                let identities = token_store::load_all_identities(true)
                    .await
                    .expect("Failed to load identities");
                let sealed_by_fallback = identities
                    .iter()
                    .find(|identity| identity.user_id == "fallback-user")
                    .expect("the fallback call's identity is stored");
                assert!(
                    sealed_by_fallback.token.is_empty(),
                    "the secure key doesn't open what the fallback key sealed"
                );
            }))
            .await
            .expect("Task failure");
        assert_eq!(
            std::fs::read(&key_file).expect("the key file exists"),
            [7u8; 32],
            "the fallback call kept its key"
        );
    }

    /// Reading a key from its file leaves the file as it is, though others can read it: the file
    /// is restricted only when the store writes it.
    #[cfg(unix)]
    #[tokio::test]
    // The fixture writes the key file directly, as an earlier version would have.
    #[allow(clippy::disallowed_methods)]
    async fn reading_the_fallback_key_leaves_its_file_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let (fallback, _store_dir, key_file) = fallback_key_call();
        std::fs::write(&key_file, [7u8; 32]).expect("Failed to write the key file");
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o644))
            .expect("Failed to widen the key file");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(fallback, async move {
                token_store::store_user_token(AUTH_ENDPOINT, "reader", "token", vec![])
                    .await
                    .expect("Failed to store token");
                assert_eq!(
                    std::fs::read(&key_file).expect("the key file exists"),
                    [7u8; 32],
                    "the store used the key it read"
                );
                assert_eq!(mode_of(&key_file), 0o644, "a read leaves the permissions");
            }))
            .await
            .expect("Task failure");
    }

    /// A temporary key file an interrupted write left, readable by others, is made its owner's
    /// alone before the next key is written into it.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn a_left_temporary_key_file_is_restricted_before_it_is_written() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let (fallback, store_dir, key_file) = fallback_key_call();
        let temporary = store_dir.child("sec-tokenstore_encryption_key.new");
        std::fs::write(&temporary, b"left").expect("Failed to write the temporary file");
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))
            .expect("Failed to widen the temporary file");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(fallback, async move {
                token_store::store_user_token(AUTH_ENDPOINT, "after", "token", vec![])
                    .await
                    .expect("Failed to store token");
                assert_eq!(mode_of(&key_file), 0o600, "the key is its owner's alone");
                assert!(!temporary.exists(), "the temporary file became the key");
            }))
            .await
            .expect("Task failure");
    }

    /// A link in place of the fallback key file is refused rather than followed, and its target
    /// is left as it is.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn a_link_in_place_of_the_fallback_key_is_refused() {
        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let (fallback, store_dir, key_file) = fallback_key_call();
        let target = store_dir.child("elsewhere");
        std::fs::write(&target, b"not a key").expect("Failed to write the link target");
        std::os::unix::fs::symlink(&target, &key_file).expect("Failed to link the key file");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(fallback, async move {
                let stored =
                    token_store::store_user_token(AUTH_ENDPOINT, "linked", "token", vec![]).await;
                assert!(stored.is_err(), "the store refuses the link");
                assert_eq!(
                    std::fs::read(&target).expect("the link target"),
                    b"not a key",
                    "the link target is left as it is"
                );
            }))
            .await
            .expect("Task failure");
    }

    /// The token store directory the store creates is its owner's alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_token_store_directory_is_its_owners_alone() {
        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let (fallback, store_dir, _key_file) = fallback_key_call();
        let nested = store_dir.child("nested");
        let call = Arc::new(
            ExecutionContext::new_client(Default::default(), EventDispatcher::new(None))
                .with_environment(CallEnvironment {
                    values: [
                        None,
                        Some(nested.display().to_string()),
                        Some("fallback".to_string()),
                    ],
                }),
        );
        drop(fallback);

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(call, async move {
                token_store::store_user_token(AUTH_ENDPOINT, "nested", "token", vec![])
                    .await
                    .expect("Failed to store token");
                assert_eq!(
                    mode_of(&nested),
                    0o700,
                    "the directory is its owner's alone"
                );
            }))
            .await
            .expect("Task failure");
    }

    /// A new key that can't be written leaves the token store as it is: the tokens are reset only
    /// once the key that replaces theirs is on disk.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn a_key_that_cannot_be_written_resets_no_token() {
        use std::os::unix::fs::PermissionsExt;

        // Safety: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root writes into a directory it cannot write to");
            return;
        }
        let _lock = sequential_mutex_lock().await;
        let (_execution, _auth_dir) = setup_test_env().await;
        let (fallback, store_dir, key_file) = fallback_key_call();
        let token_store_file = store_dir.child("tokenstore.toml");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(fallback.clone(), async move {
                token_store::store_user_token(AUTH_ENDPOINT, "kept", "token", vec![])
                    .await
                    .expect("Failed to store token");
            }))
            .await
            .expect("Task failure");
        let stored = std::fs::read(&token_store_file).expect("the token store");
        std::fs::remove_file(&key_file).expect("Failed to remove the key file");
        std::fs::set_permissions(store_dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("Failed to make the directory read-only");

        // A fresh directory name for the key cache: the key of `store_dir` is cached.
        let moved = store_dir.child("..").join(
            store_dir
                .path()
                .file_name()
                .expect("the directory has a name"),
        );
        let reread = Arc::new(
            ExecutionContext::new_client(Default::default(), EventDispatcher::new(None))
                .with_environment(CallEnvironment {
                    values: [
                        None,
                        Some(moved.display().to_string()),
                        Some("fallback".to_string()),
                    ],
                }),
        );
        let result = runtime()
            .spawn(LORE_CONTEXT.scope(reread, async move {
                token_store::store_user_token(AUTH_ENDPOINT, "other", "token", vec![]).await
            }))
            .await
            .expect("Task failure");

        std::fs::set_permissions(store_dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("Failed to make the directory writable again");
        assert!(result.is_err(), "the new key can't be written");
        assert_eq!(
            std::fs::read(&token_store_file).expect("the token store"),
            stored,
            "the token store is left as it is"
        );
    }

    #[tokio::test]
    async fn a_stored_login_is_reused_while_its_session_is_valid() {
        let _lock = sequential_mutex_lock().await;

        let (execution, _auth_dir) = setup_test_env().await;

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                const LIVE: u64 = 1_900_000_000_000;
                const DEAD: u64 = 1;
                for (token, refresh_token, refresh_expires, remote_domain, reused) in [
                    (SUPPLIED_JWT, None, 0, "example.com", true),
                    (EXPIRED_JWT, Some("refresh-1"), LIVE, "example.com", true),
                    (EXPIRED_JWT, Some("refresh-1"), 0, "example.com", true),
                    (EXPIRED_JWT, Some("refresh-1"), DEAD, "example.com", false),
                    (EXPIRED_JWT, None, 0, "example.com", false),
                    (SUPPLIED_JWT, None, 0, "other.org", false),
                ] {
                    token_store::store_user_token_and_refresh_token(
                        AUTH_ENDPOINT,
                        "alice",
                        token,
                        vec!["example.com".into()],
                        refresh_token,
                        refresh_expires,
                        None,
                    )
                    .await
                    .expect("Failed to store the login");

                    let login = lore_revision::auth::login::valid_stored_login(
                        AUTH_ENDPOINT,
                        remote_domain.to_string(),
                    )
                    .await;
                    assert_eq!(
                        login.map(|info| info.id),
                        reused.then(|| "alice".to_string()),
                        "token expired {}, refresh token {refresh_token:?} until {refresh_expires}, \
                         remote {remote_domain}",
                        token == EXPIRED_JWT,
                    );
                }
            }))
            .await
            .expect("Task failure");
    }
}
