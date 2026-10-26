// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::auth::revoke_refresh_tokens;
use lore_base::error::Disconnected;
use lore_base::error::NotSupported;
use lore_base::types::RepositoryId;
use lore_credential::token_store::RemovedRefreshToken;
use lore_error_set::FfiError;
use lore_transport::AuthSession;
use lore_transport::AuthSessionPoll;
use lore_transport::Authentication;
use lore_transport::AuthenticationToken;
use lore_transport::AuthorizationToken;
use lore_transport::ProtocolError;
use lore_transport::auth::AuthEndpoint;
use parking_lot::Mutex;

const AUTH_URL: &str = "oidc+https://issuer.test.invalid";

/// Answers each revocation with what `revoke` returns for its refresh token, and records the
/// refresh tokens presented.
struct Revoking {
    revoke: fn(&str) -> Result<(), ProtocolError>,
    presented: Mutex<Vec<String>>,
}

fn unused() -> ProtocolError {
    ProtocolError::internal("the stub only serves revocations")
}

#[async_trait::async_trait]
impl Authentication for Revoking {
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
        refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<(), ProtocolError> {
        self.presented.lock().push(refresh_token.to_owned());
        (self.revoke)(refresh_token)
    }

    async fn exchange_for_repository(
        &self,
        _auth_url: &str,
        _authn_token: &str,
        _repository: RepositoryId,
        _correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        Err(unused())
    }

    async fn exchange_for_custom_resource(
        &self,
        _auth_url: &str,
        _authn_token: &str,
        _resource_id: &str,
        _correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        Err(unused())
    }
}

fn endpoint(revoke: fn(&str) -> Result<(), ProtocolError>) -> (AuthEndpoint, Arc<Revoking>) {
    let auth = Arc::new(Revoking {
        revoke,
        presented: Mutex::default(),
    });
    (AuthEndpoint::new(AUTH_URL.to_owned(), auth.clone()), auth)
}

fn removed(users: &[&str]) -> Vec<RemovedRefreshToken> {
    users
        .iter()
        .map(|user| RemovedRefreshToken {
            user_id: (*user).to_owned(),
            refresh_token: format!("{user}-refresh"),
        })
        .collect()
}

/// A failure does not stop the others from being revoked, and the first one is returned with
/// its kind, so the exit code tells an unreachable provider from a refusal.
#[tokio::test]
async fn every_refresh_token_is_tried_and_the_first_failure_returned() {
    let (endpoint, auth) = endpoint(|refresh_token| match refresh_token {
        "bob-refresh" => Err(Disconnected.into()),
        "carol-refresh" => Err(ProtocolError::internal("refused")),
        _ => Ok(()),
    });

    let failed = revoke_refresh_tokens(
        AUTH_URL,
        Some(&endpoint),
        removed(&["alice", "bob", "carol", "dave"]),
        "corr",
    )
    .await
    .expect_err("two revocations fail");

    assert_eq!(failed.ffi_code(), Disconnected.ffi_code(), "{failed:?}");
    assert_eq!(
        *auth.presented.lock(),
        [
            "alice-refresh",
            "bob-refresh",
            "carol-refresh",
            "dave-refresh"
        ]
    );
}

#[tokio::test]
async fn a_backend_without_revocation_ends_the_logout_locally() {
    let (endpoint, auth) = endpoint(|_| {
        Err(NotSupported {
            operation: "token revocation".to_owned(),
        }
        .into())
    });

    revoke_refresh_tokens(AUTH_URL, Some(&endpoint), removed(&["alice"]), "corr")
        .await
        .expect("the logout succeeds");

    assert_eq!(*auth.presented.lock(), ["alice-refresh"]);
}

#[tokio::test]
async fn without_an_endpoint_nothing_is_revoked() {
    revoke_refresh_tokens(AUTH_URL, None, removed(&["alice"]), "corr")
        .await
        .expect("the logout succeeds");
}
