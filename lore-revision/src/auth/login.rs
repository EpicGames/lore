// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Weak;
use std::time::Duration;

use lore_credential::UserInfo;
use lore_credential::domain_from_url_or_url;
use lore_credential::insecure_decode_token;
use lore_credential::token_store;
use lore_credential::token_store::vulnerable_all_tokens;
use lore_credential::verify_jwt_usage_for_remote;
use lore_error_set::prelude::*;
use lore_transport::AuthSession;
use lore_transport::AuthSessionPoll;
use lore_transport::Authentication;
use lore_transport::AuthenticationToken;
use lore_transport::auth::authentication;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout_at;
use url::Url;
use uuid::Uuid;

use crate::auth::LoreAuthPendingEventData;
use crate::auth::LoreAuthUrlEventData;
use crate::errors::AddressNotFound;
use crate::errors::Disconnected;
use crate::errors::Maintenance;
use crate::errors::NoRemote;
use crate::errors::NotAuthenticated;
use crate::errors::NotAuthorized;
use crate::errors::NotFound;
use crate::errors::NotSupported;
use crate::errors::Oversized;
use crate::errors::SlowDown;
use crate::errors::TokenNotFound;
use crate::event;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::lore_debug;

#[error_set]
pub enum LoginError {
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    AddressNotFound,
    NoRemote,
    NotSupported,
    Oversized,
    TokenNotFound,
}

impl EventError for LoginError {
    fn translated(&self) -> LoreError {
        match self {
            LoginError::Disconnected(_) => LoreError::Connection,
            LoginError::SlowDown(_) => LoreError::SlowDown,
            LoginError::Oversized(_) => LoreError::Oversized,
            LoginError::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[error_set]
pub enum InteractiveLoginError {
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    AddressNotFound,
    NoRemote,
    NotSupported,
    Oversized,
    TokenNotFound,
}

impl EventError for InteractiveLoginError {
    fn translated(&self) -> LoreError {
        match self {
            InteractiveLoginError::Disconnected(_) => LoreError::Connection,
            InteractiveLoginError::SlowDown(_) => LoreError::SlowDown,
            InteractiveLoginError::Oversized(_) => LoreError::Oversized,
            InteractiveLoginError::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// How much a `slow_down` answer widens the polling interval, per RFC 8628
/// §3.5.
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

/// The shortest gap between two polls, whatever the session says. A
/// backend answering `interval: 0` would otherwise be polled in a tight
/// loop.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Exchanges an external token for a URC authentication token via the
/// registered `Authentication` implementation.
async fn exchange_token(
    auth_url: String,
    token: &str,
    token_type: &str,
    recipient_url: &Url,
) -> Result<UserInfo, LoginError> {
    let auth_impl =
        authentication::find(&auth_url).forward::<LoginError>("finding authentication handler")?;
    let correlation_id = crate::lore::execution_context()
        .globals()
        .correlation_id
        .to_string();

    lore_debug!("Start auth exchange request");
    let authn = auth_impl
        .exchange_external_token(&auth_url, token, token_type, &correlation_id)
        .await
        .forward::<LoginError>("exchanging external token")?;

    if let Some(user_info) = lore_credential::user_info_from_token(authn.token.clone()) {
        lore_debug!(
            "Auth with {token_type} successful, identity {}",
            user_info.id
        );

        let decoded_token = insecure_decode_token(&authn.token).internal("decoding token")?;
        verify_jwt_usage_for_remote(
            &decoded_token.claims,
            &domain_from_url_or_url(recipient_url),
        )
        .forward::<LoginError>("verifying JWT usage for remote")?;

        token_store::store_user_token(
            auth_url.as_str(),
            user_info.id.as_str(),
            &authn.token,
            decoded_token.claims.acceptable_root_domains(),
        )
        .await
        .forward::<LoginError>("storing user token")?;

        // Store refresh token if issued
        if let Some(ref refresh) = authn.refresh_token
            && let Err(e) =
                token_store::store_refresh_token(&auth_url, &user_info.id, refresh).await
        {
            lore_debug!("Failed to store refresh token for {}: {e}", user_info.id);
        }

        Ok(user_info)
    } else {
        Err(LoginError::internal("Invalid token"))
    }
}

pub(crate) async fn with_token(
    remote_url: &str,
    token: &str,
    token_type: &str,
    explicit_auth_url: Option<&str>,
) -> Result<UserInfo, LoginError> {
    lore_debug!("Authenticating using remote {remote_url}");

    let (auth_url, remote_url) = if let Some(url) = explicit_auth_url {
        // Auth URL provided directly (e.g. via --auth-url), skip environment resolution.
        // Use the auth URL's domain for JWT validation when no remote URL is available.
        lore_debug!("Using explicit auth URL: {url}");
        let parsed = url::Url::parse(url).internal("parsing explicit auth URL")?;
        (url.to_string(), parsed)
    } else {
        let (parsed_remote, protocol) =
            lore_transport::parse(remote_url).forward::<LoginError>("parsing remote URL")?;

        let environment = protocol
            .environment(Weak::default(), parsed_remote.as_str())
            .await
            .forward::<LoginError>("fetching environment")?;
        let environment = environment
            .get()
            .await
            .forward::<LoginError>("getting environment config")?;
        lore_debug!("Server environment config: {:?}", environment);

        let auth_url = environment
            .endpoint
            .and_then(|endpoint| endpoint.auth_url)
            .unwrap_or_default();

        if auth_url.is_empty() {
            return Err(NotSupported {
                operation: "No authentication configured on server".to_string(),
            }
            .into());
        }

        (auth_url, parsed_remote)
    };

    let user_info = if token_type == "lore" {
        // Direct lore token — just validate and store, no exchange needed
        let decoded_token = insecure_decode_token(token).internal("decoding token")?;
        verify_jwt_usage_for_remote(&decoded_token.claims, &domain_from_url_or_url(&remote_url))
            .forward::<LoginError>("verifying JWT usage for remote")?;

        if let Some(user_info) = lore_credential::user_info_from_token(token.to_string()) {
            token_store::store_user_token(
                auth_url.as_str(),
                user_info.id.as_str(),
                token,
                decoded_token.claims.acceptable_root_domains(),
            )
            .await
            .forward::<LoginError>("storing user token")?;

            user_info
        } else {
            return Err(LoginError::internal("Invalid token"));
        }
    } else {
        exchange_token(auth_url, token, token_type, &remote_url).await?
    };

    Ok(user_info)
}

/// Boxed version of [`with_token`] for cross-crate use.
pub fn with_token_boxed<'a>(
    remote_url: &'a str,
    token: &'a str,
    token_type: &'a str,
    explicit_auth_url: Option<&'a str>,
) -> crate::BoxFuture<'a, Result<UserInfo, LoginError>> {
    Box::pin(with_token(remote_url, token, token_type, explicit_auth_url))
}

/// Authenticates interactively via a browser-based login flow.
///
/// Connects to the remote URL's auth endpoint, starts an auth session, and
/// either opens the login URL in a browser or emits it as an
/// [`LoreEvent::AuthUrl`] event when `no_browser` is set. Polls the auth
/// service at the cadence the session sets until the user approves the
/// login, reporting each wait as a [`LoreEvent::AuthPending`] event, and
/// fails when the session ends without one.
///
/// The received token is validated against the remote's domain before being
/// stored in the encrypted token store.
pub async fn interactive(
    remote_url: &str,
    no_browser: bool,
) -> Result<UserInfo, InteractiveLoginError> {
    lore_debug!("Interactive login with remote {remote_url}");

    let (remote_url, protocol) =
        lore_transport::parse(remote_url).forward::<InteractiveLoginError>("parsing remote URL")?;

    // Get the server config from environment endpoint
    let environment = protocol
        .environment(Weak::default(), remote_url.as_str())
        .await
        .forward::<InteractiveLoginError>("fetching environment")?;
    let environment = environment
        .get()
        .await
        .forward::<InteractiveLoginError>("getting environment config")?;
    lore_debug!("Server environment config: {:?}", environment);

    let auth_url = environment
        .endpoint
        .and_then(|endpoint| endpoint.auth_url)
        .unwrap_or_default();

    if auth_url.is_empty() {
        return Err(NotSupported {
            operation: "No authentication configured on server".to_string(),
        }
        .into());
    }

    let auth_impl = authentication::find(&auth_url)
        .forward::<InteractiveLoginError>("finding authentication handler")?;
    let correlation_id = crate::lore::execution_context()
        .globals()
        .correlation_id
        .to_string();

    lore_debug!("Login on web with auth {auth_url} no_browser {no_browser}");

    // 1. Generate a `clientState` (uuid-like)
    let client_state = Uuid::new_v4().to_string();
    lore_debug!("ClientState {}", client_state);

    // 2. Start auth session via the Authentication implementation. Its
    // lifetime is counted from before the request that creates it, so the
    // request's own latency, opening the browser, and handing the URL to
    // the caller all count against it. The client may give up a little
    // before the backend does, but never polls a session the backend has
    // already expired.
    lore_debug!("Authenticating using {auth_url}");
    let started = Instant::now();
    let session = auth_impl
        .start_auth_session(&auth_url, &client_state, &correlation_id)
        .await
        .forward::<InteractiveLoginError>("starting auth session")?;

    lore_debug!(
        "Got: '{} / {}' from service",
        session.login_url,
        session.session_code
    );

    if !no_browser {
        open::that(session.login_url.as_str()).internal("opening authentication URL")?;
    } else {
        event::LoreEvent::AuthUrl(LoreAuthUrlEventData {
            url: (&session.login_url).into(),
        })
        .send();
    }

    // 3. Poll at the session's cadence until approved, denied or expired
    let authn = poll_interactive_session(
        &*auth_impl,
        &auth_url,
        &client_state,
        &session,
        started,
        &correlation_id,
    )
    .await?;

    // 4. Verify the given remote can be trusted with this JWT.
    let decoded_token = insecure_decode_token(&authn.token).internal("decoding token")?;
    verify_jwt_usage_for_remote(&decoded_token.claims, &domain_from_url_or_url(&remote_url))
        .forward::<InteractiveLoginError>("verifying JWT usage for remote")?;

    lore_debug!("Auth successful");
    token_store::store_user_token(
        auth_url.as_str(),
        authn.user_id.as_str(),
        authn.token.as_str(),
        decoded_token.claims.acceptable_root_domains(),
    )
    .await
    .forward::<InteractiveLoginError>("storing user token")?;

    // Store refresh token if the backend issued one
    if let Some(ref refresh) = authn.refresh_token
        && let Err(e) = token_store::store_refresh_token(&auth_url, &authn.user_id, refresh).await
    {
        lore_debug!("Failed to store refresh token for {}: {e}", authn.user_id);
    }

    let Some(user_info) = lore_credential::user_info(
        auth_url.as_str(),
        authn.user_id.as_str(),
        vulnerable_all_tokens(),
        authn.token.as_str(),
        "",
    )
    .await
    else {
        return Err(InteractiveLoginError::internal("Unable to load user info"));
    };

    Ok(user_info)
}

/// Polls `session` until the user approves it, at the cadence the session
/// sets: every `interval`, widened by [`SLOW_DOWN_INCREMENT`] each time the
/// backend answers `SlowDown`, and never past `expires_in` from `started`,
/// the moment the session was requested. A session whose lifetime puts
/// that deadline out of the clock's range is refused before it is polled,
/// rather than waited on without a bound. A poll still in flight at that
/// point is abandoned, so the loop ends on the client's clock even when the
/// backend stops answering. Each wait is reported as a
/// [`LoreEvent::AuthPending`] event.
#[lore_macro::test_pub]
async fn poll_interactive_session(
    auth: &dyn Authentication,
    auth_url: &str,
    client_state: &str,
    session: &AuthSession,
    started: Instant,
    correlation_id: &str,
) -> Result<AuthenticationToken, InteractiveLoginError> {
    const EXPIRED: &str = "Login session expired before it was approved";
    // The advertised lifetime is honoured in full. One the clock cannot
    // represent is refused rather than waited on unbounded.
    let deadline = started
        .checked_add(session.expires_in)
        .ok_or_else(|| InteractiveLoginError::internal("Login session lifetime is out of range"))?;
    let mut interval = session.interval.max(MIN_POLL_INTERVAL);

    loop {
        if Instant::now() >= deadline {
            return Err(InteractiveLoginError::internal(EXPIRED));
        }

        let poll = auth.poll_auth_session(
            auth_url,
            client_state,
            &session.session_code,
            correlation_id,
        );
        let poll = timeout_at(deadline, poll)
            .await
            .map_err(|_elapsed| InteractiveLoginError::internal(EXPIRED))?
            .forward::<InteractiveLoginError>("polling auth session")?;

        match poll {
            AuthSessionPoll::Complete(token) => return Ok(token),
            AuthSessionPoll::Pending => {}
            AuthSessionPoll::SlowDown => {
                interval = interval.saturating_add(SLOW_DOWN_INCREMENT);
                lore_debug!("Auth service asked to slow down. Polling every {interval:?}");
            }
        }

        // Scoped so that nothing read off the clock is held across the sleep.
        {
            let now = Instant::now();
            let expired = match now.checked_add(interval) {
                Some(next_poll) => next_poll >= deadline,
                None => true,
            };
            if expired {
                return Err(InteractiveLoginError::internal(EXPIRED));
            }
            let elapsed = now.duration_since(started);
            event::LoreEvent::AuthPending(LoreAuthPendingEventData {
                elapsed_secs: elapsed.as_secs(),
                interval_secs: interval.as_secs(),
                remaining_secs: deadline.duration_since(now).as_secs(),
            })
            .send();
        }
        sleep(interval).await;
    }
}
