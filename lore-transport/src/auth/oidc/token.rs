// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The token endpoint (RFC 6749 §3.2): the request every grant POSTs to it, and the
//! success and error responses it answers with (§5.1, §5.2).

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::lore_warn;
use serde::Deserialize;

use super::FormResponse;
use super::PostError;
use super::body_excerpt;
use super::on_net_runtime;
use super::post_form;
use crate::error::ProtocolError;
use crate::types::AuthenticationToken;

/// A successful response, RFC 6749 §5.1, with the `id_token` `OpenID` Connect Core
/// §3.1.3.3 adds.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    /// Seconds until the refresh token stops working. No RFC or discovery metadata names
    /// the field: GitHub, Azure AD and Google send `refresh_token_expires_in`, Keycloak
    /// `refresh_expires_in`. Two fields rather than a serde alias, which would refuse a
    /// response carrying both.
    pub refresh_token_expires_in: Option<u64>,
    pub refresh_expires_in: Option<u64>,
    pub scope: Option<String>,
    pub id_token: Option<String>,
}

/// An error response, RFC 6749 §5.2.
#[derive(Debug, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub error_description: Option<String>,
}

impl ErrorResponse {
    /// The error code with its description, both bounded, since both are the provider's
    /// text.
    pub fn describe(&self) -> String {
        match self.error_description.as_deref() {
            Some(description) if !description.is_empty() => {
                format!(
                    "{}: {}",
                    body_excerpt(&self.error),
                    body_excerpt(description)
                )
            }
            _ => body_excerpt(&self.error),
        }
    }
}

/// What an endpoint answered a grant with: a token, or a refusal in the RFC 6749 §5.2
/// shape, which a grant may treat as a state of its own rather than a failure.
#[derive(Debug)]
pub enum Outcome<T> {
    Token(T),
    Refused(ErrorResponse),
}

/// POSTs `form` to `endpoint` and reads a `T` from a success, or the refusal from a 4xx
/// in the RFC 6749 §5.2 shape. Any other answer is an error. The body of a success is
/// never logged, since it carries the credential.
pub(super) async fn request<T>(
    what: &'static str,
    endpoint: &str,
    form: Vec<(&'static str, String)>,
) -> Result<Outcome<T>, ProtocolError>
where
    T: for<'de> Deserialize<'de> + Send + 'static,
{
    request_authorized(what, endpoint, form, None).await
}

/// [`request`] with `authorization` as the `Authorization` header, for a client that
/// authenticates with HTTP Basic (RFC 6749 §2.3.1).
pub(super) async fn request_authorized<T>(
    what: &'static str,
    endpoint: &str,
    form: Vec<(&'static str, String)>,
    authorization: Option<String>,
) -> Result<Outcome<T>, ProtocolError>
where
    T: for<'de> Deserialize<'de> + Send + 'static,
{
    request_once(what, endpoint, form, authorization)
        .await
        .map_err(ProtocolError::from)
}

/// [`request_authorized`], telling a failure that sent nothing from one after which the
/// provider may have acted on the request. Everything after a response arrives, a refusal
/// that cannot be read and an unusable success included, may have been acted on.
pub(super) async fn request_once<T>(
    what: &'static str,
    endpoint: &str,
    form: Vec<(&'static str, String)>,
    authorization: Option<String>,
) -> Result<Outcome<T>, PostError>
where
    T: for<'de> Deserialize<'de> + Send + 'static,
{
    let endpoint = endpoint.to_owned();
    on_net_runtime(async move {
        let form: Vec<(&str, &str)> = form
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        Ok(
            match post_form(what, &endpoint, &form, authorization.as_deref()).await {
                Ok(response) => parse(what, &response).map_err(PostError::MaybeSent),
                Err(error) => Err(error),
            },
        )
    })
    .await
    .map_err(PostError::MaybeSent)?
}

fn parse<T: for<'de> Deserialize<'de>>(
    what: &str,
    response: &FormResponse,
) -> Result<Outcome<T>, ProtocolError> {
    if response.status.is_success() {
        return serde_json::from_slice(&response.body)
            .map(Outcome::Token)
            .map_err(|source| {
                lore_warn!("OIDC {what} response is malformed: {source}");
                ProtocolError::internal_with_context(
                    source,
                    "reading the identity provider's response",
                )
            });
    }
    if response.status.is_client_error()
        && let Ok(refusal) = serde_json::from_slice::<ErrorResponse>(&response.body)
    {
        return Ok(Outcome::Refused(refusal));
    }
    lore_warn!(
        "OIDC {what} answered HTTP {}: {}",
        response.status.as_u16(),
        response.body_text()
    );
    Err(ProtocolError::internal(format!(
        "OIDC {what} answered HTTP {}",
        response.status.as_u16()
    )))
}

/// The authentication token in `response`. The access token is the credential, and the
/// identity is read from its claims. An ID token is read for the display name alone and
/// dropped here, so it is neither stored nor sent anywhere: it is for this client to
/// consume (`OpenID` Connect Core §2), never a credential to present.
pub fn authentication_token(response: TokenResponse) -> Result<AuthenticationToken, ProtocolError> {
    let TokenResponse {
        access_token,
        token_type,
        expires_in: _,
        refresh_token,
        refresh_token_expires_in,
        refresh_expires_in,
        scope,
        id_token,
    } = response;
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(ProtocolError::internal(format!(
            "OIDC token endpoint issued a token of type '{}'; only Bearer is supported",
            body_excerpt(&token_type)
        )));
    }
    let Ok(decoded) = lore_credential::insecure_decode_token(&access_token) else {
        return Err(ProtocolError::internal(
            "OIDC access token is not a JWT carrying the standard claims",
        ));
    };
    let Some(access) = lore_credential::user_info_from_token(access_token) else {
        let (claim, _) = decoded.claims.identity_source();
        return Err(ProtocolError::internal(format!(
            "OIDC access token names no one: its identity claim '{claim}' is empty, null or not a string"
        )));
    };
    let identity = id_token.and_then(lore_credential::user_info_from_token);
    let user_name = [identity.as_ref(), Some(&access)]
        .into_iter()
        .flatten()
        .flat_map(|info| [info.name.as_str(), info.preferred_username.as_str()])
        .find(|name| !name.is_empty())
        .unwrap_or_default()
        .to_owned();
    Ok(AuthenticationToken {
        token: access.token,
        user_id: access.id,
        user_name,
        expires_ms: access.expires,
        // Populated by the orchestration layer from the token's claims.
        acceptable_root_domains: Vec::new(),
        refresh_expires_ms: refresh_token.as_ref().map_or(0, |_| {
            refresh_deadline(refresh_token_expires_in.or(refresh_expires_in))
        }),
        refresh_token,
        scope,
    })
}

/// The deadline `expires_in` seconds from now, in milliseconds since the UNIX epoch, or 0 for
/// none. A `0` is none too: Keycloak sends it for offline tokens. The deadline is capped where
/// the token store's TOML can still hold it.
fn refresh_deadline(expires_in: Option<u64>) -> u64 {
    let Some(expires_in) = expires_in.filter(|&seconds| seconds > 0) else {
        return 0;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    now.saturating_add(expires_in.saturating_mul(1000))
        .min(i64::MAX as u64)
}

/// The `scope` parameter (RFC 6749 §3.3) joining the non-empty entries of `scopes`, or `None`
/// when there are none.
pub(crate) fn join_scopes<'a>(scopes: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let scope = scopes
        .into_iter()
        .filter(|scope| !scope.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!scope.is_empty()).then_some(scope)
}
