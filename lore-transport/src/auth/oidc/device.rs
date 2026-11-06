// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The device authorization grant (RFC 8628): the authorization request (§3.1) that opens
//! a login session, and the token request (§3.4) that polls it.

use std::time::Duration;

use lore_base::error::NotAuthenticated;
use lore_base::error::NotSupported;
use lore_base::lore_warn;
use serde::Deserialize;

use super::discovery::DiscoveryDocument;
use super::token;
use super::token::Outcome;
use crate::error::ProtocolError;
use crate::types::AuthSession;
use crate::types::AuthSessionPoll;
use crate::types::Oidc;

pub const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Requested on every login (`OpenID` Connect Core §11). Providers differ on whether a
/// refresh token outlives the browser session without it.
pub const OFFLINE_ACCESS_SCOPE: &str = "offline_access";

/// The polling interval when the provider sets none (RFC 8628 §3.2).
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// A device authorization response, RFC 8628 §3.2.
#[derive(Debug, Deserialize)]
struct AuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

/// The scope a login asks for: the server's default scopes, and [`OFFLINE_ACCESS_SCOPE`].
pub fn scope(oidc: &Oidc) -> String {
    let mut scopes: Vec<&str> = oidc.scopes.iter().map(String::as_str).collect();
    if !scopes.contains(&OFFLINE_ACCESS_SCOPE) {
        scopes.push(OFFLINE_ACCESS_SCOPE);
    }
    token::join_scopes(scopes).unwrap_or_default()
}

/// Opens a login session at the provider's device authorization endpoint. The session's
/// `login_url` is `verification_uri_complete` when the provider gives one, else
/// `verification_uri`, and `user_code` is always the code, which the user may be asked to
/// confirm at either (§3.3.1).
pub async fn authorize(
    oidc: &Oidc,
    document: &DiscoveryDocument,
) -> Result<AuthSession, ProtocolError> {
    let Some(endpoint) = document.device_authorization_endpoint.as_deref() else {
        return Err(NotSupported {
            operation: format!(
                "login against OIDC issuer {}: the provider advertises no device \
                 authorization endpoint",
                oidc.issuer
            ),
        }
        .into());
    };
    let form = vec![
        ("client_id", oidc.client_id.clone()),
        ("scope", scope(oidc)),
    ];
    let response: AuthorizationResponse =
        match token::request("device authorization", endpoint, form).await? {
            Outcome::Token(response) => response,
            Outcome::Refused(refusal) => {
                return Err(ProtocolError::internal(format!(
                    "OIDC device authorization refused: {}",
                    refusal.describe()
                )));
            }
        };
    // The URL is handed to the browser, so a provider may not point it at
    // anything but a web page: the rule discovery applies to its endpoints,
    // loopback `http` included only when the issuer is one.
    let login_url = response
        .verification_uri_complete
        .unwrap_or(response.verification_uri);
    let allow_loopback_http = oidc
        .issuer
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http:"));
    let permitted = url::Url::parse(&login_url)
        .is_ok_and(|url| super::is_permitted_url(&url, allow_loopback_http));
    if !permitted {
        lore_warn!(
            "OIDC device authorization for {} answered with a verification URL that is not a web page",
            oidc.issuer
        );
        return Err(ProtocolError::internal(
            "OIDC device authorization answered with a verification URL that is not https",
        ));
    }
    Ok(AuthSession {
        session_code: response.device_code,
        login_url,
        user_code: response.user_code,
        interval: response
            .interval
            .map_or(DEFAULT_INTERVAL, Duration::from_secs),
        expires_in: Duration::from_secs(response.expires_in),
    })
}

/// Polls the session `device_code` names once, at the token endpoint. The refusals RFC
/// 8628 §3.5 defines map to the poll's states: `authorization_pending` and `slow_down` are
/// states, `access_denied` is [`NotAuthenticated`], and `expired_token` an error naming
/// the expiry.
pub async fn poll(
    oidc: &Oidc,
    document: &DiscoveryDocument,
    device_code: &str,
) -> Result<AuthSessionPoll, ProtocolError> {
    let form = vec![
        ("grant_type", GRANT_TYPE.to_owned()),
        ("device_code", device_code.to_owned()),
        ("client_id", oidc.client_id.clone()),
    ];
    match token::request("token request", &document.token_endpoint, form).await? {
        Outcome::Token(response) => Ok(AuthSessionPoll::Complete(token::authentication_token(
            response,
        )?)),
        Outcome::Refused(refusal) => match refusal.error.as_str() {
            "authorization_pending" => Ok(AuthSessionPoll::Pending),
            "slow_down" => Ok(AuthSessionPoll::SlowDown),
            "access_denied" => Err(NotAuthenticated.into()),
            "expired_token" => Err(ProtocolError::internal(
                "Login session expired before it was approved",
            )),
            _ => Err(ProtocolError::internal(format!(
                "OIDC token endpoint refused the device code grant: {}",
                refusal.describe()
            ))),
        },
    }
}
