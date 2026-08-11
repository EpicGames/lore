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
use lore_transport::Authentication;
use lore_transport::AuthenticationToken;
use lore_transport::auth::authentication;
use tokio::time::sleep;
use url::Url;
use uuid::Uuid;

use crate::auth::LoreAuthUrlEventData;
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

// To be read from config somehow
const POLLING_MAX_RETRIES: u64 = 30;
const POLLING_INTERVAL_SECS: u64 = 5;

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

pub async fn with_token(
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

/// Authenticates interactively via a browser-based login flow.
///
/// Connects to the remote URL's auth endpoint, starts an auth session, and
/// either opens the login URL in a browser or emits it as an
/// [`LoreEvent::AuthUrl`] event when `no_browser` is set. Polls the auth
/// service until a token is received or a timeout occurs.
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

    // 2. Start auth session via the Authentication implementation
    lore_debug!("Authenticating using {auth_url}");
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
            url: session.login_url.into(),
        })
        .send();
    }

    // 3. Poll until complete or timeout
    let authn = poll_interactive_session(
        &*auth_impl,
        &auth_url,
        &client_state,
        &session.session_code,
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
    )
    .await
    else {
        return Err(InteractiveLoginError::internal("Unable to load user info"));
    };

    Ok(user_info)
}

async fn poll_interactive_session(
    auth: &dyn Authentication,
    auth_url: &str,
    client_state: &str,
    session_code: &str,
    correlation_id: &str,
) -> Result<AuthenticationToken, InteractiveLoginError> {
    for _ in 0..POLLING_MAX_RETRIES {
        let result = auth
            .poll_auth_session(auth_url, client_state, session_code, correlation_id)
            .await
            .forward::<InteractiveLoginError>("polling auth session")?;

        if let Some(token) = result {
            return Ok(token);
        }
        lore_debug!("Got: None from poll_auth_session");
        sleep(Duration::from_secs(POLLING_INTERVAL_SECS)).await;
    }
    Err(InteractiveLoginError::internal("Timeout"))
}
