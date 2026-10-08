// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::RepositoryId;
use lore_transport::auth::authentication;
use lore_transport::auth::exchange::*;

/// A supplied access token is the authorization token, so the exchange is
/// skipped entirely -- including the checks that would otherwise reject a
/// call with no auth URL and no identity.
#[tokio::test]
async fn supplied_access_token_replaces_the_repository_exchange() {
    let token = exchange(
        "",
        "",
        RepositoryId::default(),
        "example.com".to_string(),
        "",
        "supplied-authz",
    )
    .await
    .expect("the supplied access token is used as given");
    assert_eq!(token, "supplied-authz");
}

#[tokio::test]
async fn supplied_access_token_replaces_the_resource_exchange() {
    let token =
        exchange_custom_resource("", "", "", "example.com".to_string(), "", "supplied-authz")
            .await
            .expect("the supplied access token is used as given");
    assert_eq!(token, "supplied-authz");
}

/// An access token on its own authorizes without an authentication token to
/// trade in, and without reading one from the store. The authentication slot
/// comes back empty, so the services that need one fail where they use it
/// while the authorized ones still work.
#[tokio::test]
async fn access_token_alone_authorizes_without_an_authentication_token() {
    let repository: RepositoryId = "00112233445566778899aabbccddeeff"
        .parse()
        .expect("a valid repository id");
    let (authentication_token, authorization_token, identity) = auth_exchange(
        "ucs-auth://auth.example.com",
        "example.com",
        "alice",
        repository,
        "",
        "supplied-authz",
    )
    .await;

    assert!(
        authentication_token.is_empty(),
        "no authentication token is invented, and none is read from the store"
    );
    assert_eq!(authorization_token, "supplied-authz");
    assert_eq!(identity, "alice");
}

/// Without one, the same call fails: nothing about the short circuit above
/// leaks into the normal path.
#[tokio::test]
async fn no_supplied_access_token_still_requires_an_auth_url() {
    let result = exchange(
        "",
        "",
        RepositoryId::default(),
        "example.com".to_string(),
        "",
        "",
    )
    .await;
    assert!(result.is_err());
}

/// An expired supplied token is still the credential the caller asked for.
///
/// Blanking it sends requests with no `Authorization` header at all, which a
/// server answers as an anonymous caller rather than as an expired one, so the
/// caller cannot tell its token needs renewing. Handing it over gets a straight
/// rejection instead. The expiry check that blanks a token exists to skip a
/// stale *stored* identity while picking one, which a supplied credential is
/// not.
///
/// The auth URL uses a scheme no `Authentication` implementation is registered
/// for, so the authorization exchange fails without a network call. The
/// authentication token is what this is about.
#[tokio::test]
async fn an_expired_supplied_identity_token_is_still_used() {
    /// The same claims as the fixtures above, with `exp` back in 2001.
    const EXPIRED_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjEwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

    let repository: RepositoryId = "00112233445566778899aabbccddeeff"
        .parse()
        .expect("a valid repository id");

    let (authentication_token, _authorization_token, identity) = auth_exchange(
        "no-such-scheme://auth.expired-token.test.invalid",
        "example.com",
        "alice",
        repository,
        EXPIRED_TOKEN,
        "",
    )
    .await;

    assert_eq!(
        authentication_token, EXPIRED_TOKEN,
        "the supplied token must be handed over for the server to reject"
    );
    assert_eq!(identity, "alice");
}

/// An authorization is only good for the credential that earned it. Two
/// identity tokens for one user can carry different scopes, audiences or
/// lifetimes, so a caller supplying one must never be handed the
/// authorization another caller's token produced.
///
/// A stub `Authentication` returns a distinct authorization per
/// authentication token, which makes the separation observable: were the
/// cache shared across credentials, the second caller would come back with
/// the first one's token and no second exchange would happen. Registering the
/// stub under its own scheme also keeps this off the network.
#[tokio::test]
async fn one_supplied_credential_is_never_served_anothers_authorization() {
    use lore_transport::error::ProtocolError;
    use lore_transport::traits::Authentication;
    use lore_transport::types::AuthSession;
    use lore_transport::types::AuthSessionPoll;
    use lore_transport::types::AuthenticationToken;
    use lore_transport::types::AuthorizationToken;

    /// `sub` alice, `aud` example.com, expiring in 2033.
    const AUTHN_ONE: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UifQ.signature";
    /// The same user, a different credential -- note the `scope` claim.
    const AUTHN_TWO: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UiLCJzY29wZSI6InJlYWQifQ.signature";
    const AUTHZ_ONE: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UiLCJhdXRoeiI6Im9uZSJ9.signature";
    const AUTHZ_TWO: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UiLCJhdXRoeiI6InR3byJ9.signature";
    const AUTH_URL: &str = "stub-auth://auth.credential-isolation.test.invalid";

    /// Hands back an authorization naming which authentication token asked
    /// for it, and counts the exchanges that actually happened.
    struct StubAuthentication {
        exchanges: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Authentication for StubAuthentication {
        async fn exchange_for_repository(
            &self,
            _auth_url: &str,
            authn_token: &str,
            _repository: RepositoryId,
            _correlation_id: &str,
        ) -> Result<AuthorizationToken, ProtocolError> {
            self.exchanges
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            let token = if authn_token == AUTHN_ONE {
                AUTHZ_ONE
            } else {
                AUTHZ_TWO
            };
            Ok(AuthorizationToken {
                token: token.to_string(),
                expires_ms: 0,
                acceptable_root_domains: vec!["example.com".to_string()],
            })
        }

        async fn start_auth_session(
            &self,
            _auth_url: &str,
            _client_state: &str,
            _correlation_id: &str,
        ) -> Result<AuthSession, ProtocolError> {
            Err(ProtocolError::internal(
                "the stub only serves authorization exchanges",
            ))
        }

        async fn poll_auth_session(
            &self,
            _auth_url: &str,
            _client_state: &str,
            _session_code: &str,
            _correlation_id: &str,
        ) -> Result<AuthSessionPoll, ProtocolError> {
            Err(ProtocolError::internal(
                "the stub only serves authorization exchanges",
            ))
        }

        async fn exchange_external_token(
            &self,
            _auth_url: &str,
            _token: &str,
            _token_type: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(ProtocolError::internal(
                "the stub only serves authorization exchanges",
            ))
        }

        async fn refresh_authentication(
            &self,
            _auth_url: &str,
            _refresh_token: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(ProtocolError::internal(
                "the stub only serves authorization exchanges",
            ))
        }

        async fn exchange_for_custom_resource(
            &self,
            _auth_url: &str,
            _authn_token: &str,
            _resource_id: &str,
            _correlation_id: &str,
        ) -> Result<AuthorizationToken, ProtocolError> {
            Err(ProtocolError::internal(
                "the stub only serves repository exchanges",
            ))
        }
    }

    async fn authorize(repository: RepositoryId, identity_token: &str) -> String {
        exchange(
            AUTH_URL,
            "alice",
            repository,
            "example.com".to_string(),
            identity_token,
            "",
        )
        .await
        .expect("the stub authorizes")
    }

    let stub = std::sync::Arc::new(StubAuthentication {
        exchanges: std::sync::atomic::AtomicUsize::new(0),
    });
    authentication::add("stub-auth", stub.clone()).expect("registering the stub");

    let repository: RepositoryId = "aabbccdd00112233aabbccdd00112233"
        .parse()
        .expect("a valid repository id");

    assert_eq!(authorize(repository, AUTHN_ONE).await, AUTHZ_ONE);
    assert_eq!(
        authorize(repository, AUTHN_ONE).await,
        AUTHZ_ONE,
        "the same credential reuses the authorization it earned"
    );
    assert_eq!(
        stub.exchanges.load(std::sync::atomic::Ordering::Acquire),
        1,
        "a repeat of the same credential must come from the cache"
    );

    assert_eq!(
        authorize(repository, AUTHN_TWO).await,
        AUTHZ_TWO,
        "a different credential must earn its own authorization"
    );
    assert_eq!(
        stub.exchanges.load(std::sync::atomic::Ordering::Acquire),
        2,
        "the second credential must not be served the first one's entry"
    );
}
