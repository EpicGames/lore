// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
/// Auth exchange integration tests.
///
/// These tests verify the `Authentication` trait's error handling patterns
/// that the orchestration layer depends on for identity probing and
/// authorization exchange, and the `UserService` lookup beside it. They use
/// `TestAuthentication` registered in the global scheme registries.
///
/// The original `AuthExchange` trait tests (identity selection, domain
/// filtering) operated on an in-memory mock of the token store. Those
/// scenarios are now covered by:
/// - Unit tests in `protocol::tests` (mock trait method responses)
/// - Smoke tests (end-to-end with real token store)
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use lore_base::error::NotAuthenticated;
    use lore_base::error::NotAuthorized;
    use lore_base::error::NotSupported;
    use lore_revision::lore::RepositoryId;
    use lore_transport::AuthSession;
    use lore_transport::Authentication;
    use lore_transport::AuthenticationToken;
    use lore_transport::AuthorizationToken;
    use lore_transport::ProtocolError;
    use lore_transport::ResolvedUser;
    use lore_transport::UserService;
    use lore_transport::auth::authentication;
    use lore_transport::auth::user_service;

    struct TestAuthentication {
        exchange_result:
            Box<dyn Fn(RepositoryId) -> Result<AuthorizationToken, ProtocolError> + Send + Sync>,
    }

    impl TestAuthentication {
        fn always_succeed() -> Self {
            Self {
                exchange_result: Box::new(|_| {
                    Ok(AuthorizationToken {
                        token: "authz-token".into(),
                        expires_ms: u64::MAX,
                        acceptable_root_domains: vec![],
                    })
                }),
            }
        }

        fn always_not_authorized() -> Self {
            Self {
                exchange_result: Box::new(|_| Err(ProtocolError::from(NotAuthorized))),
            }
        }

        fn always_not_authenticated() -> Self {
            Self {
                exchange_result: Box::new(|_| Err(ProtocolError::from(NotAuthenticated))),
            }
        }
    }

    #[async_trait]
    impl Authentication for TestAuthentication {
        async fn start_auth_session(
            &self,
            _auth_url: &str,
            _client_state: &str,
            _correlation_id: &str,
        ) -> Result<AuthSession, ProtocolError> {
            Err(ProtocolError::from(NotSupported {
                operation: "start_auth_session".into(),
            }))
        }

        async fn poll_auth_session(
            &self,
            _auth_url: &str,
            _client_state: &str,
            _session_code: &str,
            _correlation_id: &str,
        ) -> Result<Option<AuthenticationToken>, ProtocolError> {
            Ok(None)
        }

        async fn exchange_external_token(
            &self,
            _auth_url: &str,
            _token: &str,
            _token_type: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(ProtocolError::from(NotSupported {
                operation: "exchange_external_token".into(),
            }))
        }

        async fn refresh_authentication(
            &self,
            _auth_url: &str,
            _refresh_token: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(ProtocolError::from(NotSupported {
                operation: "refresh_authentication".into(),
            }))
        }

        async fn exchange_for_repository(
            &self,
            _auth_url: &str,
            _authn_token: &str,
            repository: RepositoryId,
            _correlation_id: &str,
        ) -> Result<AuthorizationToken, ProtocolError> {
            (self.exchange_result)(repository)
        }

        async fn exchange_for_custom_resource(
            &self,
            _auth_url: &str,
            _authn_token: &str,
            _resource_id: &str,
            _correlation_id: &str,
        ) -> Result<AuthorizationToken, ProtocolError> {
            (self.exchange_result)(RepositoryId::default())
        }
    }

    #[async_trait]
    impl UserService for TestAuthentication {
        async fn get_user_info(
            &self,
            _user_url: &str,
            _authz_token: &str,
            _repository: RepositoryId,
            user_ids: &[String],
            _correlation_id: &str,
        ) -> Result<Vec<ResolvedUser>, ProtocolError> {
            Ok(user_ids
                .iter()
                .map(|id| ResolvedUser {
                    user_id: id.clone(),
                    user_name: format!("User {id}"),
                })
                .collect())
        }

        async fn get_user_id(
            &self,
            _user_url: &str,
            _authz_token: &str,
            _repository: RepositoryId,
            display_name: &str,
            _correlation_id: &str,
        ) -> Result<Option<ResolvedUser>, ProtocolError> {
            Ok(Some(ResolvedUser {
                user_id: format!("id-for-{display_name}"),
                user_name: display_name.to_string(),
            }))
        }
    }

    #[test]
    fn test_auth_registration_and_lookup() {
        let scheme = "test-auth-exchange";
        let mock = Arc::new(TestAuthentication::always_succeed());
        authentication::add(scheme, mock).unwrap();

        let found = authentication::find(&format!("{scheme}://auth.test.com"));
        assert!(found.is_ok());
    }

    #[tokio::test]
    async fn exchange_for_repository_success_returns_token() {
        let scheme = "test-exchange-success";
        authentication::add(scheme, Arc::new(TestAuthentication::always_succeed())).unwrap();

        let auth = authentication::find(&format!("{scheme}://auth.test.com")).unwrap();
        let result = auth
            .exchange_for_repository(
                &format!("{scheme}://auth.test.com"),
                "authn-tok",
                RepositoryId::default(),
                "corr",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().token, "authz-token");
    }

    #[tokio::test]
    async fn exchange_for_custom_resource_success_returns_token() {
        let scheme = "test-exchange-custom-success";
        authentication::add(scheme, Arc::new(TestAuthentication::always_succeed())).unwrap();

        let auth = authentication::find(&format!("{scheme}://auth.test.com")).unwrap();
        let result = auth
            .exchange_for_custom_resource(
                &format!("{scheme}://auth.test.com"),
                "authn-tok",
                "bespoke-urc:uefn:some-stream",
                "corr",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().token, "authz-token");
    }

    #[tokio::test]
    async fn exchange_not_authorized_is_matchable() {
        let scheme = "test-exchange-not-authz";
        authentication::add(
            scheme,
            Arc::new(TestAuthentication::always_not_authorized()),
        )
        .unwrap();

        let auth = authentication::find(&format!("{scheme}://auth.test.com")).unwrap();
        let result = auth
            .exchange_for_repository(
                &format!("{scheme}://auth.test.com"),
                "authn-tok",
                RepositoryId::default(),
                "corr",
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_authorized());
    }

    #[tokio::test]
    async fn exchange_not_authenticated_is_matchable() {
        let scheme = "test-exchange-not-authn";
        authentication::add(
            scheme,
            Arc::new(TestAuthentication::always_not_authenticated()),
        )
        .unwrap();

        let auth = authentication::find(&format!("{scheme}://auth.test.com")).unwrap();
        let result = auth
            .exchange_for_repository(
                &format!("{scheme}://auth.test.com"),
                "authn-tok",
                RepositoryId::default(),
                "corr",
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_authenticated());
    }

    #[tokio::test]
    async fn get_user_info_returns_resolved_users() {
        let scheme = "test-userinfo";
        user_service::add(scheme, Arc::new(TestAuthentication::always_succeed())).unwrap();

        let service = user_service::find(&format!("{scheme}://auth.test.com"));
        let users = service
            .get_user_info(
                &format!("{scheme}://auth.test.com"),
                "authz-tok",
                RepositoryId::default(),
                &["u1".into(), "u2".into()],
                "corr",
            )
            .await
            .unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].user_id, "u1");
        assert_eq!(users[1].user_name, "User u2");
    }

    #[tokio::test]
    async fn get_user_id_returns_resolved_user() {
        let scheme = "test-userid";
        user_service::add(scheme, Arc::new(TestAuthentication::always_succeed())).unwrap();

        let service = user_service::find(&format!("{scheme}://auth.test.com"));
        let user = service
            .get_user_id(
                &format!("{scheme}://auth.test.com"),
                "authz-tok",
                RepositoryId::default(),
                "Alice",
                "corr",
            )
            .await
            .unwrap();
        assert!(user.is_some());
        assert_eq!(user.unwrap().user_id, "id-for-Alice");
    }

    /// An authentication registered without a user service still resolves users:
    /// the user's own name is resolved from the token, the other user names
    /// are resolved as their IDs.
    #[tokio::test]
    async fn authentication_without_a_user_service_falls_back_to_the_token() {
        /// `{"iss":"lore","sub":"alice","name":"Alice","exp":2000000000,"aud":["example.com"]}`
        const ALICE_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";
        let scheme = "test-authn-only";
        authentication::add(scheme, Arc::new(TestAuthentication::always_succeed())).unwrap();

        let auth_url = format!("{scheme}://auth.test.com");
        let users = user_service::find(&auth_url)
            .get_user_info(
                &auth_url,
                ALICE_TOKEN,
                RepositoryId::default(),
                &["alice".into(), "bob".into()],
                "corr",
            )
            .await
            .unwrap();
        let names: Vec<&str> = users.iter().map(|u| u.user_name.as_str()).collect();
        assert_eq!(names, ["Alice", "bob"]);

        let bob = user_service::find(&auth_url)
            .get_user_id(
                &auth_url,
                ALICE_TOKEN,
                RepositoryId::default(),
                "bob",
                "corr",
            )
            .await
            .unwrap();
        assert!(bob.is_none(), "the token names nobody but its bearer");
    }

    #[tokio::test]
    async fn not_supported_interactive_login() {
        let scheme = "test-no-interactive";
        authentication::add(scheme, Arc::new(TestAuthentication::always_succeed())).unwrap();

        let auth = authentication::find(&format!("{scheme}://auth.test.com")).unwrap();
        let result = auth
            .start_auth_session(&format!("{scheme}://auth.test.com"), "state", "corr")
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_supported());
    }
}
