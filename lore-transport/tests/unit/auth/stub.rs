// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! An `Authentication` that serves exchanges from a closure, so a test drives the
//! authorization exchange without a network, and tokens for it to trade.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::types::RepositoryId;
use lore_transport::error::ProtocolError;
use lore_transport::traits::Authentication;
use lore_transport::types::AuthSession;
use lore_transport::types::AuthSessionPoll;
use lore_transport::types::AuthenticationToken;
use lore_transport::types::AuthorizationToken;

/// `sub` alice, `aud` example.com, expiring in 2033.
pub(crate) const AUTHN_ONE: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UifQ.signature";
/// The same user, a different credential -- note the `scope` claim.
pub(crate) const AUTHN_TWO: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UiLCJzY29wZSI6InJlYWQifQ.signature";
/// Authorizations for alice on example.com, expiring in 2033, told apart by an `authz` claim.
pub(crate) const AUTHZ_ONE: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UiLCJhdXRoeiI6Im9uZSJ9.signature";
pub(crate) const AUTHZ_TWO: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdLCJuYW1lIjoiQWxpY2UiLCJhdXRoeiI6InR3byJ9.signature";

/// Names the authorization for the authentication token traded in and the number of exchanges
/// served before it.
type Authorize = Box<dyn Fn(&str, usize) -> &'static str + Send + Sync>;

/// Answers a refresh grant for the refresh token presented.
type Refresh = Box<dyn Fn(&str) -> Result<AuthenticationToken, ProtocolError> + Send + Sync>;

/// Hands out the authorization `authorize` names, and counts the exchanges. Refreshes are
/// answered by `refresh`, and refused without one.
pub(crate) struct StubAuthentication {
    exchanges: AtomicUsize,
    authorize: Authorize,
    refresh: Option<Refresh>,
    refresh_delay: std::time::Duration,
    presented: parking_lot::Mutex<Vec<String>>,
}

impl StubAuthentication {
    pub(crate) fn new(
        authorize: impl Fn(&str, usize) -> &'static str + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            exchanges: AtomicUsize::new(0),
            authorize: Box::new(authorize),
            refresh: None,
            refresh_delay: std::time::Duration::ZERO,
            presented: parking_lot::Mutex::default(),
        })
    }

    /// A stub that answers refresh grants with `refresh`, and trades [`AUTHN_ONE`] for
    /// [`AUTHZ_ONE`] and any other token for [`AUTHZ_TWO`].
    pub(crate) fn refreshing(
        refresh: impl Fn(&str) -> Result<AuthenticationToken, ProtocolError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::refreshing_after(std::time::Duration::ZERO, refresh)
    }

    /// [`StubAuthentication::refreshing`], answering each refresh after `delay`.
    pub(crate) fn refreshing_after(
        delay: std::time::Duration,
        refresh: impl Fn(&str) -> Result<AuthenticationToken, ProtocolError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            exchanges: AtomicUsize::new(0),
            authorize: Box::new(|authn_token, _| {
                if authn_token == AUTHN_ONE {
                    AUTHZ_ONE
                } else {
                    AUTHZ_TWO
                }
            }),
            refresh: Some(Box::new(refresh)),
            refresh_delay: delay,
            presented: parking_lot::Mutex::default(),
        })
    }

    /// Repository exchanges served so far.
    pub(crate) fn exchanges(&self) -> usize {
        self.exchanges.load(Ordering::Acquire)
    }

    /// The refresh tokens presented so far, in order.
    pub(crate) fn presented(&self) -> Vec<String> {
        self.presented.lock().clone()
    }
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
        let served = self.exchanges.fetch_add(1, Ordering::AcqRel);
        Ok(AuthorizationToken {
            token: (self.authorize)(authn_token, served).to_string(),
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

    async fn authenticate_client(
        &self,
        _auth_url: &str,
        _client_id: &str,
        _client_secret: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        Err(ProtocolError::internal(
            "the stub only serves authorization exchanges",
        ))
    }

    async fn refresh_authentication(
        &self,
        _auth_url: &str,
        refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        self.presented.lock().push(refresh_token.to_owned());
        tokio::time::sleep(self.refresh_delay).await;
        match &self.refresh {
            Some(refresh) => refresh(refresh_token),
            None => Err(ProtocolError::internal(
                "the stub only serves authorization exchanges",
            )),
        }
    }

    async fn revoke_refresh_token(
        &self,
        _auth_url: &str,
        _refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<(), ProtocolError> {
        Err(ProtocolError::internal(
            "the stub only serves authorization exchanges",
        ))
    }

    async fn exchange_for_custom_resource(
        &self,
        auth_url: &str,
        authn_token: &str,
        _resource_id: &str,
        correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        self.exchange_for_repository(
            auth_url,
            authn_token,
            RepositoryId::default(),
            correlation_id,
        )
        .await
    }
}
