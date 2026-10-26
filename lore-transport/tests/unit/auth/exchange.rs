// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::RepositoryId;
use lore_credential::token_store;
use lore_transport::auth::AuthEndpoint;
use lore_transport::auth::exchange::*;

use super::stub::AUTHN_ONE;
use super::stub::AUTHN_TWO;
use super::stub::AUTHZ_ONE;
use super::stub::AUTHZ_TWO;
use super::stub::StubAuthentication;

/// A supplied access token is the authorization token, so the exchange is
/// skipped entirely -- including the checks that would otherwise reject a
/// call with no auth URL and no identity.
#[tokio::test]
async fn supplied_access_token_replaces_the_repository_exchange() {
    let token = exchange(
        &AuthEndpoint::none(),
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
    let token = exchange_custom_resource(
        &AuthEndpoint::none(),
        "",
        "",
        "example.com".to_string(),
        "",
        "supplied-authz",
    )
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
        &AuthEndpoint::resolve("ucs-auth://auth.example.com").expect("a registered scheme"),
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
        &AuthEndpoint::none(),
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
/// The endpoint's implementation refuses every grant, so the authorization
/// exchange fails without a network call. The authentication token is what
/// this is about.
#[tokio::test]
async fn an_expired_supplied_identity_token_is_still_used() {
    /// The same claims as the fixtures above, with `exp` back in 2001.
    const EXPIRED_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjEwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

    let repository: RepositoryId = "00112233445566778899aabbccddeeff"
        .parse()
        .expect("a valid repository id");

    let refusing = AuthEndpoint::new(
        "stub-auth://auth.expired-token.test.invalid".to_string(),
        AuthEndpoint::none().auth,
    );
    let (authentication_token, _authorization_token, identity) = auth_exchange(
        &refusing,
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
/// the first one's token and no second exchange would happen. Binding the
/// stub as the endpoint's implementation also keeps this off the network.
#[tokio::test]
async fn one_supplied_credential_is_never_served_anothers_authorization() {
    const AUTH_URL: &str = "stub-auth://auth.credential-isolation.test.invalid";

    async fn authorize(
        endpoint: &AuthEndpoint,
        repository: RepositoryId,
        identity_token: &str,
    ) -> String {
        exchange(
            endpoint,
            "alice",
            repository,
            "example.com".to_string(),
            identity_token,
            "",
        )
        .await
        .expect("the stub authorizes")
    }

    let stub = StubAuthentication::new(|authn_token, _| {
        if authn_token == AUTHN_ONE {
            AUTHZ_ONE
        } else {
            AUTHZ_TWO
        }
    });
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());

    let repository: RepositoryId = "aabbccdd00112233aabbccdd00112233"
        .parse()
        .expect("a valid repository id");

    assert_eq!(authorize(&endpoint, repository, AUTHN_ONE).await, AUTHZ_ONE);
    assert_eq!(
        authorize(&endpoint, repository, AUTHN_ONE).await,
        AUTHZ_ONE,
        "the same credential reuses the authorization it earned"
    );
    assert_eq!(
        stub.exchanges(),
        1,
        "a repeat of the same credential must come from the cache"
    );

    assert_eq!(
        authorize(&endpoint, repository, AUTHN_TWO).await,
        AUTHZ_TWO,
        "a different credential must earn its own authorization"
    );
    assert_eq!(
        stub.exchanges(),
        2,
        "the second credential must not be served the first one's entry"
    );
}

/// An authorization the server refused is forgotten, so the next exchange asks for a new one, but
/// only while the cache still holds it: a refusal that arrives after the cache holds a newer one
/// leaves the newer one alone.
#[tokio::test]
async fn a_refusal_forgets_only_the_authorization_refused() {
    let stub = StubAuthentication::new(|_, served| if served == 0 { AUTHZ_ONE } else { AUTHZ_TWO });
    let endpoint = AuthEndpoint::new(
        "stub-auth://auth.late-refusal.test.invalid".to_string(),
        stub.clone(),
    );
    let repository: RepositoryId = "aabbccdd00112233aabbccdd00112233"
        .parse()
        .expect("a valid repository id");
    let authorize = || {
        exchange(
            &endpoint,
            "alice",
            repository,
            "example.com".to_string(),
            AUTHN_ONE,
            "",
        )
    };
    let refuse = || {
        forget_refused_authorization(
            &endpoint,
            "alice",
            repository,
            "example.com",
            AUTHN_ONE,
            AUTHZ_ONE,
        )
    };

    assert_eq!(authorize().await.expect("authorized"), AUTHZ_ONE);
    refuse().await;
    assert_eq!(authorize().await.expect("authorized"), AUTHZ_TWO);
    refuse().await;
    assert_eq!(authorize().await.expect("authorized"), AUTHZ_TWO);
    assert_eq!(stub.exchanges(), 2);
}

/// A refusal that arrives after the token store holds a newer authorization leaves the newer one
/// in the store.
#[tokio::test]
async fn a_refusal_forgets_only_the_stored_authorization_refused() {
    const AUTH_URL: &str = "stub-auth://auth.late-stored-refusal.test.invalid";
    token_store::store_user_token(
        AUTH_URL,
        "alice",
        AUTHN_ONE,
        vec!["example.com".to_string()],
    )
    .await
    .expect("store the authentication token");
    let stub = StubAuthentication::new(|_, served| if served == 0 { AUTHZ_ONE } else { AUTHZ_TWO });
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());
    let repository: RepositoryId = "aabbccdd00112233aabbccdd44556677"
        .parse()
        .expect("a valid repository id");
    let authorize = || {
        exchange(
            &endpoint,
            "alice",
            repository,
            "example.com".to_string(),
            "",
            "",
        )
    };
    let refuse = || {
        forget_refused_authorization(&endpoint, "alice", repository, "example.com", "", AUTHZ_ONE)
    };

    assert_eq!(authorize().await.expect("authorized"), AUTHZ_ONE);
    refuse().await;
    assert_eq!(authorize().await.expect("authorized"), AUTHZ_TWO);
    refuse().await;

    let stored = token_store::load_user_token_from_store(
        &authorization_store_key(&endpoint, &repository.to_string()),
        "alice",
        token_store::vulnerable_all_tokens(),
    )
    .await;
    assert_eq!(stored.ok().as_deref(), Some(AUTHZ_TWO));
}

mod refused {
    use std::sync::Arc;
    use std::sync::Weak;

    use lore_base::lore_spawn_net;
    use lore_credential::token_store;
    use lore_transport::auth::AuthEndpoint;
    use lore_transport::auth::exchange::exchange;
    use lore_transport::connection::SuppliedCredentials;
    use lore_transport::error::ProtocolError;
    use lore_transport::quic::storage_service::client::StorageClient;
    use lore_transport::traits::Storage;

    use crate::auth::stub::*;
    use crate::quic::storage_service::test_server::SessionStarts;
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_partition;
    use crate::quic::storage_service::test_server::wait_until;

    /// An auth service at `auth_url` that hands out the first authorization, then the second.
    fn auth_service(auth_url: &str) -> (AuthEndpoint, Arc<StubAuthentication>) {
        let stub =
            StubAuthentication::new(|_, served| if served == 0 { AUTHZ_ONE } else { AUTHZ_TWO });
        (AuthEndpoint::new(auth_url.to_string(), stub.clone()), stub)
    }

    /// A storage client for alice on `server`, through `endpoint`, with `credentials`.
    async fn connect(
        server: &TestStorageServer,
        endpoint: &AuthEndpoint,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Arc<StorageClient> {
        StorageClient::connect(
            Weak::new(),
            &server.url,
            "example.com".to_string(),
            endpoint,
            "alice",
            test_partition(),
            credentials,
            None,
        )
        .await
        .expect("connect to the test server")
    }

    /// A session start for alice through `endpoint` with `credentials`, on a server that refuses
    /// `refused`. Returns the server and what the session start returned.
    async fn session_start(
        endpoint: &AuthEndpoint,
        refused: &[&str],
        credentials: SuppliedCredentials,
    ) -> (TestStorageServer, Result<u32, ProtocolError>) {
        let server = TestStorageServer::start_refusing_authorizations(refused);
        let client = connect(&server, endpoint, &Arc::new(credentials)).await;
        let started = client.session_start(test_partition(), "test").await;
        (server, started)
    }

    /// A session start for alice through `endpoint` with `credentials`, which the server holds
    /// until a later call has supplied `identity_token` in their place, then refuses as not
    /// authorized, as it does every start after. Returns the server and what the session start
    /// returned.
    async fn session_start_across_a_rotation(
        endpoint: &AuthEndpoint,
        credentials: SuppliedCredentials,
        identity_token: &str,
    ) -> (TestStorageServer, Result<u32, ProtocolError>) {
        let (server, starts) = TestStorageServer::start_controlling_session_starts();
        starts
            .send(SessionStarts::Hold)
            .expect("the server holds starts");
        let credentials = Arc::new(credentials);
        let client = connect(&server, endpoint, &credentials).await;
        let starting =
            lore_spawn_net!(async move { client.session_start(test_partition(), "test").await });
        wait_until(
            || server.requested_session_starts_per_connection() == [1],
            "the session start did not reach the server",
        )
        .await;
        credentials.update(identity_token, "");
        starts
            .send(SessionStarts::RefuseAuthorization)
            .expect("the server refuses the held start");
        let started = starting.await.expect("session start task");
        (server, started)
    }

    /// A storage session start the server refuses forgets the authorization it presented and starts
    /// the session with a newly exchanged one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_authorization_is_exchanged_again() {
        let (endpoint, stub) = auth_service("stub-auth://auth.refused-authorization.test.invalid");
        let (server, started) = session_start(
            &endpoint,
            &[AUTHZ_ONE],
            SuppliedCredentials::new(AUTHN_ONE, ""),
        )
        .await;

        assert!(started.is_ok(), "the session must start: {started:?}");
        assert_eq!(server.presented_authorizations(), [AUTHZ_ONE, AUTHZ_TWO]);
        assert_eq!(stub.exchanges(), 2);
    }

    /// A refused authorization from the token store is not read back from it on the cache miss
    /// its eviction leaves, so the session starts with a newly exchanged one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_stored_authorization_is_exchanged_again() {
        const AUTH_URL: &str = "stub-auth://auth.refused-stored-authorization.test.invalid";
        token_store::store_user_token(
            AUTH_URL,
            "alice",
            AUTHN_ONE,
            vec!["example.com".to_string()],
        )
        .await
        .expect("store the authentication token");
        let (endpoint, stub) = auth_service(AUTH_URL);

        let (server, started) =
            session_start(&endpoint, &[AUTHZ_ONE], SuppliedCredentials::default()).await;

        assert!(started.is_ok(), "the session must start: {started:?}");
        assert_eq!(server.presented_authorizations(), [AUTHZ_ONE, AUTHZ_TWO]);
        assert_eq!(stub.exchanges(), 2);
    }

    /// A refusal of the newly exchanged authorization is final.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_new_authorization_is_final() {
        let (endpoint, stub) = auth_service("stub-auth://auth.refused-twice.test.invalid");
        let (server, started) = session_start(
            &endpoint,
            &[AUTHZ_ONE, AUTHZ_TWO],
            SuppliedCredentials::new(AUTHN_ONE, ""),
        )
        .await;

        assert!(started.is_err_and(|err| err.is_not_authorized()));
        assert_eq!(server.presented_authorizations(), [AUTHZ_ONE, AUTHZ_TWO]);
        assert_eq!(stub.exchanges(), 2);
    }

    /// A refusal is final where the exchange after it gives the authorization refused again, which
    /// is not presented twice.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_the_exchange_gives_again_is_final() {
        let stub = StubAuthentication::new(|_, _| AUTHZ_ONE);
        let endpoint = AuthEndpoint::new(
            "stub-auth://auth.refused-unchanged.test.invalid".to_string(),
            stub.clone(),
        );
        let (server, started) = session_start(
            &endpoint,
            &[AUTHZ_ONE],
            SuppliedCredentials::new(AUTHN_ONE, ""),
        )
        .await;

        assert!(started.is_err_and(|err| err.is_not_authorized()));
        assert_eq!(server.presented_authorizations(), [AUTHZ_ONE]);
        assert_eq!(stub.exchanges(), 2);
    }

    /// A refusal of an access token the caller supplied is final, and asks the auth service
    /// nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_supplied_access_token_is_final() {
        let (endpoint, stub) = auth_service("stub-auth://auth.refused-access-token.test.invalid");
        let (server, started) = session_start(
            &endpoint,
            &[AUTHZ_ONE],
            SuppliedCredentials::new("", AUTHZ_ONE),
        )
        .await;

        assert!(started.is_err_and(|err| err.is_not_authorized()));
        assert_eq!(server.presented_authorizations(), [AUTHZ_ONE]);
        assert_eq!(stub.exchanges(), 0);
    }

    /// A refusal of an access token the caller supplied is final, and asks the auth service
    /// nothing, though a later call supplies an identity token while the server considers it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_supplied_access_token_is_final_after_a_rotation() {
        let (endpoint, stub) = auth_service("stub-auth://auth.rotated-access-token.test.invalid");
        let (server, started) = session_start_across_a_rotation(
            &endpoint,
            SuppliedCredentials::new("", AUTHZ_ONE),
            AUTHN_ONE,
        )
        .await;

        assert!(started.is_err_and(|err| err.is_not_authorized()));
        assert_eq!(server.presented_authorizations(), [AUTHZ_ONE]);
        assert_eq!(stub.exchanges(), 0);
    }

    /// A refusal is forgotten under the identity token that exchanged the authorization refused,
    /// though a later call supplies another while the server considers it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_after_a_rotation_forgets_the_authorization_refused() {
        let (endpoint, stub) = auth_service("stub-auth://auth.rotated-identity-token.test.invalid");
        let (server, started) = session_start_across_a_rotation(
            &endpoint,
            SuppliedCredentials::new(AUTHN_ONE, ""),
            AUTHN_TWO,
        )
        .await;

        assert!(started.is_err_and(|err| err.is_not_authorized()));
        assert_eq!(server.presented_authorizations(), [AUTHZ_ONE, AUTHZ_TWO]);
        let authorized = exchange(
            &endpoint,
            "alice",
            test_partition(),
            "example.com".to_string(),
            AUTHN_ONE,
            "",
        )
        .await
        .expect("authorized");
        assert_eq!(authorized, AUTHZ_TWO);
        assert_eq!(stub.exchanges(), 3);
    }
}

/// An implementation that refuses the login token reports `NotAuthenticated`, and the
/// exchange passes it on as such rather than as an internal failure, so the caller can
/// tell that logging in again fixes it. The same holds for a refused partition and
/// `NotAuthorized`.
#[tokio::test]
async fn a_refusal_of_the_login_token_stays_not_authenticated() {
    use lore_base::error::NotAuthenticated;
    use lore_base::error::NotAuthorized;
    use lore_transport::error::ProtocolError;
    use lore_transport::traits::Authentication;
    use lore_transport::types::AuthSession;
    use lore_transport::types::AuthSessionPoll;
    use lore_transport::types::AuthenticationToken;
    use lore_transport::types::AuthorizationToken;

    /// `sub` alice, `aud` example.com, expiring in 2033.
    const AUTHN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UifQ.signature";

    /// Refuses every exchange with the error it holds.
    struct Refusing(fn() -> ProtocolError);

    fn unused() -> ProtocolError {
        ProtocolError::internal("the stub only serves authorization exchanges")
    }

    #[async_trait::async_trait]
    impl Authentication for Refusing {
        async fn exchange_for_repository(
            &self,
            _auth_url: &str,
            _authn_token: &str,
            _repository: RepositoryId,
            _correlation_id: &str,
        ) -> Result<AuthorizationToken, ProtocolError> {
            Err((self.0)())
        }

        async fn exchange_for_custom_resource(
            &self,
            _auth_url: &str,
            _authn_token: &str,
            _resource_id: &str,
            _correlation_id: &str,
        ) -> Result<AuthorizationToken, ProtocolError> {
            Err((self.0)())
        }

        async fn start_auth_session(
            &self,
            _auth_url: &str,
            _client_state: &str,
            _correlation_id: &str,
        ) -> Result<AuthSession, ProtocolError> {
            Err(unused())
        }

        async fn poll_auth_session(
            &self,
            _auth_url: &str,
            _client_state: &str,
            _session_code: &str,
            _correlation_id: &str,
        ) -> Result<AuthSessionPoll, ProtocolError> {
            Err(unused())
        }

        async fn exchange_external_token(
            &self,
            _auth_url: &str,
            _token: &str,
            _token_type: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(unused())
        }

        async fn authenticate_client(
            &self,
            _auth_url: &str,
            _client_id: &str,
            _client_secret: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(unused())
        }

        async fn refresh_authentication(
            &self,
            _auth_url: &str,
            _refresh_token: &str,
            _correlation_id: &str,
        ) -> Result<AuthenticationToken, ProtocolError> {
            Err(unused())
        }

        async fn revoke_refresh_token(
            &self,
            _auth_url: &str,
            _refresh_token: &str,
            _correlation_id: &str,
        ) -> Result<(), ProtocolError> {
            Err(unused())
        }
    }

    async fn assert_refused_as(
        auth_url: &str,
        refusal: fn() -> ProtocolError,
        check: fn(&ExchangeError) -> bool,
    ) {
        let repository: RepositoryId = "ccddeeff00112233ccddeeff00112233"
            .parse()
            .expect("a valid repository id");
        let endpoint =
            AuthEndpoint::new(auth_url.to_string(), std::sync::Arc::new(Refusing(refusal)));

        let refused = exchange(
            &endpoint,
            "alice",
            repository,
            "example.com".to_string(),
            AUTHN,
            "",
        )
        .await
        .expect_err("the stub refuses the exchange");
        assert!(check(&refused), "{auth_url}: {refused}");

        let refused = exchange_custom_resource(
            &endpoint,
            "alice",
            "urc-user-service",
            "example.com".to_string(),
            AUTHN,
            "",
        )
        .await
        .expect_err("the stub refuses the exchange");
        assert!(check(&refused), "{auth_url}: {refused}");
    }

    assert_refused_as(
        "stub-auth://auth.refused-login.test.invalid",
        || NotAuthenticated.into(),
        ExchangeError::is_not_authenticated,
    )
    .await;
    assert_refused_as(
        "stub-auth://auth.refused-partition.test.invalid",
        || NotAuthorized.into(),
        ExchangeError::is_not_authorized,
    )
    .await;
}

/// An unsigned JWT naming `subject` for `audience`, expiring at `exp`.
fn jwt(subject: &str, audience: &str, exp: u64) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = serde_json::json!({"iss": "lore", "sub": subject, "aud": [audience], "exp": exp});
    format!(
        "{}.{}.signature",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

const EXPIRED: u64 = 1_000_000_000;

fn refreshed(token: &str, refresh_token: &str) -> lore_transport::types::AuthenticationToken {
    lore_transport::types::AuthenticationToken {
        token: token.to_owned(),
        user_id: "alice".to_owned(),
        user_name: String::new(),
        expires_ms: 2_000_000_000_000,
        acceptable_root_domains: Vec::new(),
        refresh_token: Some(refresh_token.to_owned()),
        scope: None,
    }
}

/// A stored login whose token has expired is refreshed while an identity is picked, so the
/// identity is found and authenticated without a new login. The new token is stored, so the
/// next derivation presents no refresh token.
#[tokio::test]
async fn an_expired_stored_login_is_refreshed() {
    const AUTH_URL: &str = "oidc+https://refresh.test.invalid";
    token_store::store_user_token_and_refresh_token(
        AUTH_URL,
        "alice",
        &jwt("alice", "example.com", EXPIRED),
        vec!["example.com".to_owned()],
        Some("refresh-1"),
        None,
    )
    .await
    .expect("store the login");
    let stub = StubAuthentication::refreshing(|_| Ok(refreshed(AUTHN_ONE, "refresh-2")));
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());

    for _ in 0..2 {
        let (authentication_token, _, identity) = auth_exchange(
            &endpoint,
            "example.com",
            "",
            RepositoryId::default(),
            "",
            "",
        )
        .await;
        assert_eq!(authentication_token, AUTHN_ONE);
        assert_eq!(identity, "alice");
    }
    assert_eq!(stub.presented(), ["refresh-1"]);
}

/// A refused refresh token skips the identity, as an expired login did before, and is not
/// presented again.
#[tokio::test]
async fn an_expired_stored_login_whose_refresh_is_refused_is_skipped() {
    const AUTH_URL: &str = "oidc+https://refresh-refused.test.invalid";
    token_store::store_user_token_and_refresh_token(
        AUTH_URL,
        "alice",
        &jwt("alice", "example.com", EXPIRED),
        vec!["example.com".to_owned()],
        Some("refresh-1"),
        None,
    )
    .await
    .expect("store the login");
    let stub = StubAuthentication::refreshing(|_| Err(lore_base::error::NotAuthenticated.into()));
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());

    for _ in 0..2 {
        let (authentication_token, _, identity) = auth_exchange(
            &endpoint,
            "example.com",
            "alice",
            RepositoryId::default(),
            "",
            "",
        )
        .await;
        assert!(authentication_token.is_empty());
        assert!(identity.is_empty());
    }
    assert_eq!(stub.presented(), ["refresh-1"]);
}

/// A refreshed token is sent only where the issuer named in it allows: one naming another
/// domain is stored, and not handed out for this one.
#[tokio::test]
async fn a_refreshed_token_for_another_domain_is_not_handed_out() {
    const AUTH_URL: &str = "oidc+https://refresh-other-domain.test.invalid";
    token_store::store_user_token_and_refresh_token(
        AUTH_URL,
        "alice",
        &jwt("alice", "example.com", EXPIRED),
        vec!["example.com".to_owned()],
        Some("refresh-1"),
        None,
    )
    .await
    .expect("store the login");
    let elsewhere = jwt("alice", "elsewhere.test.invalid", 2_000_000_000);
    let refreshed_token = elsewhere.clone();
    let stub =
        StubAuthentication::refreshing(move |_| Ok(refreshed(&refreshed_token, "refresh-2")));
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());

    let (authentication_token, _, _) = auth_exchange(
        &endpoint,
        "example.com",
        "alice",
        RepositoryId::default(),
        "",
        "",
    )
    .await;

    assert!(authentication_token.is_empty());
    let stored = token_store::load_user_token_from_store(
        AUTH_URL,
        "alice",
        token_store::vulnerable_all_tokens(),
    )
    .await;
    assert_eq!(stored.ok(), Some(elsewhere));
}

/// A supplied identity token is handed over expired, as before: the store's refresh token is
/// for the stored login, not for a credential the caller supplied.
#[tokio::test]
async fn an_expired_supplied_identity_token_is_not_refreshed() {
    const AUTH_URL: &str = "oidc+https://refresh-supplied.test.invalid";
    let supplied = jwt("alice", "example.com", EXPIRED);
    token_store::store_user_token_and_refresh_token(
        AUTH_URL,
        "alice",
        &supplied,
        vec!["example.com".to_owned()],
        Some("refresh-1"),
        None,
    )
    .await
    .expect("store the login");
    let stub = StubAuthentication::refreshing(|_| Ok(refreshed(AUTHN_ONE, "refresh-2")));
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());

    let (authentication_token, _, _) = auth_exchange(
        &endpoint,
        "example.com",
        "alice",
        RepositoryId::default(),
        &supplied,
        "",
    )
    .await;

    assert_eq!(authentication_token, supplied);
    assert!(stub.presented().is_empty());
}

/// The partition exchange a connection makes before it authenticates trades a refreshed login
/// token, not the expired one, for the repository and for a custom resource alike.
#[tokio::test]
async fn an_exchange_refreshes_an_expired_stored_login_first() {
    const AUTH_URL: &str = "oidc+https://refresh-exchange.test.invalid";
    token_store::store_user_token_and_refresh_token(
        AUTH_URL,
        "alice",
        &jwt("alice", "example.com", EXPIRED),
        vec!["example.com".to_owned()],
        Some("refresh-1"),
        None,
    )
    .await
    .expect("store the login");
    let stub = StubAuthentication::refreshing(|_| Ok(refreshed(AUTHN_ONE, "refresh-2")));
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());
    let repository: RepositoryId = "aabbccdd00112233aabbccdd00112244"
        .parse()
        .expect("a valid repository id");

    let authorization = exchange(
        &endpoint,
        "alice",
        repository,
        "example.com".to_string(),
        "",
        "",
    )
    .await
    .expect("the exchange succeeds");
    assert_eq!(authorization, AUTHZ_ONE);

    let authorization = exchange_custom_resource(
        &endpoint,
        "alice",
        "urc-user-service",
        "example.com".to_string(),
        "",
        "",
    )
    .await
    .expect("the exchange succeeds");
    assert_eq!(authorization, AUTHZ_ONE);

    assert_eq!(stub.presented(), ["refresh-1"]);
}

/// A refresh that was not sent keeps the refresh token, which the next derivation presents
/// again. One that may have reached the issuer drops it, so it is never presented twice.
#[tokio::test]
async fn only_an_unsent_refresh_is_tried_again() {
    for (auth_url, failure, presented) in [
        (
            "oidc+https://refresh-unsent-again.test.invalid",
            (|| lore_base::error::Disconnected.into()) as fn() -> lore_transport::ProtocolError,
            2,
        ),
        (
            "oidc+https://refresh-maybe-sent.test.invalid",
            || lore_transport::ProtocolError::internal("the answer was lost"),
            1,
        ),
    ] {
        token_store::store_user_token_and_refresh_token(
            auth_url,
            "alice",
            &jwt("alice", "example.com", EXPIRED),
            vec!["example.com".to_owned()],
            Some("refresh-1"),
            None,
        )
        .await
        .expect("store the login");
        let stub = StubAuthentication::refreshing(move |_| Err(failure()));
        let endpoint = AuthEndpoint::new(auth_url.to_string(), stub.clone());

        for _ in 0..2 {
            let (authentication_token, _, _) = auth_exchange(
                &endpoint,
                "example.com",
                "alice",
                RepositoryId::default(),
                "",
                "",
            )
            .await;
            assert!(authentication_token.is_empty());
        }
        assert_eq!(stub.presented().len(), presented, "{auth_url}");
    }
}

/// A caller that gives up during a refresh, as a cancelled connection does, does not stop it:
/// the refresh completes and stores the new token, which the next caller uses without
/// presenting the refresh token again.
#[tokio::test]
async fn a_cancelled_caller_does_not_stop_the_refresh() {
    const AUTH_URL: &str = "oidc+https://refresh-caller-cancelled.test.invalid";
    token_store::store_user_token_and_refresh_token(
        AUTH_URL,
        "alice",
        &jwt("alice", "example.com", EXPIRED),
        vec!["example.com".to_owned()],
        Some("refresh-1"),
        None,
    )
    .await
    .expect("store the login");
    let stub = StubAuthentication::refreshing_after(std::time::Duration::from_millis(300), |_| {
        Ok(refreshed(AUTHN_ONE, "refresh-2"))
    });
    let endpoint = AuthEndpoint::new(AUTH_URL.to_string(), stub.clone());
    let authenticate = || {
        auth_exchange(
            &endpoint,
            "example.com",
            "alice",
            RepositoryId::default(),
            "",
            "",
        )
    };

    let cancelled =
        tokio::time::timeout(std::time::Duration::from_millis(50), authenticate()).await;
    assert!(cancelled.is_err(), "the refresh outlasts the caller");

    let (authentication_token, _, _) = authenticate().await;
    assert_eq!(authentication_token, AUTHN_ONE);
    assert_eq!(stub.presented(), ["refresh-1"]);
}
