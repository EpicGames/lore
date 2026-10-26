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
    use lore_transport::AuthSessionPoll;
    use lore_transport::Authentication;
    use lore_transport::AuthenticationToken;
    use lore_transport::AuthorizationToken;
    use lore_transport::ProtocolError;
    use lore_transport::ResolvedUser;
    use lore_transport::UserService;
    use lore_transport::auth::authentication;

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
        ) -> Result<AuthSessionPoll, ProtocolError> {
            Ok(AuthSessionPoll::Pending)
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

        async fn authenticate_client(
            &self,
            _auth_url: &str,
            _client_id: &str,
            _client_secret: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(ProtocolError::from(NotSupported {
                operation: "authenticate_client".into(),
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

        async fn revoke_refresh_token(
            &self,
            _auth_url: &str,
            _refresh_token: &str,
            _correlation_id: &str,
        ) -> Result<(), ProtocolError> {
            Err(ProtocolError::from(NotSupported {
                operation: "revoke_refresh_token".into(),
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
