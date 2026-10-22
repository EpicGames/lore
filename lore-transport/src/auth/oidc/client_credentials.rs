// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The client credentials grant (RFC 6749 §4.4): a confidential client registered at the
//! provider logs in as itself, authenticating with its own ID and secret.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotSupported;
use lore_base::lore_debug;

use super::discovery::DiscoveryDocument;
use super::token;
use super::token::Outcome;
use crate::error::ProtocolError;
use crate::types::AuthenticationToken;

pub const GRANT_TYPE: &str = "client_credentials";
pub const CLIENT_SECRET_BASIC: &str = "client_secret_basic";
pub const CLIENT_SECRET_POST: &str = "client_secret_post";

/// How the client presents its credentials at the token endpoint (RFC 6749 §2.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// HTTP Basic in the `Authorization` header.
    Basic,
    /// `client_id` and `client_secret` in the form.
    Post,
}

/// The method the provider's `token_endpoint_auth_methods_supported` names:
/// `client_secret_basic` if it is listed or the list is absent, since RFC 8414 §2 makes it
/// the default, else `client_secret_post`.
pub fn auth_method(document: &DiscoveryDocument) -> Result<ClientAuthMethod, ProtocolError> {
    let Some(supported) = &document.token_endpoint_auth_methods_supported else {
        return Ok(ClientAuthMethod::Basic);
    };
    if supported.iter().any(|method| method == CLIENT_SECRET_BASIC) {
        Ok(ClientAuthMethod::Basic)
    } else if supported.iter().any(|method| method == CLIENT_SECRET_POST) {
        Ok(ClientAuthMethod::Post)
    } else {
        Err(NotSupported {
            operation: format!(
                "login with a client secret against OIDC issuer {}: its token endpoint accepts \
                 neither {CLIENT_SECRET_BASIC} nor {CLIENT_SECRET_POST}",
                document.issuer
            ),
        }
        .into())
    }
}

/// The `Authorization` header value for `client_secret_basic`. Both parts are form-encoded
/// before they are joined, as RFC 6749 §2.3.1 requires, so a `:` in the ID survives.
pub fn basic_authorization(client_id: &str, client_secret: &str) -> String {
    let encode =
        |value: &str| url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>();
    let credentials = format!("{}:{}", encode(client_id), encode(client_secret));
    format!("Basic {}", STANDARD.encode(credentials))
}

/// Requests a token for the client `client_id` names. No scope is sent, so the provider
/// grants the client's default scopes. A refusal of the credentials, `invalid_client`, is
/// [`NotAuthenticated`].
///
/// A refresh token in the answer is dropped. Refreshing it needs the client's secret, which
/// is not stored, and RFC 6749 §4.4.3 has the provider issue none.
pub async fn login(
    document: &DiscoveryDocument,
    client_id: &str,
    client_secret: &str,
) -> Result<AuthenticationToken, ProtocolError> {
    let mut form = vec![("grant_type", GRANT_TYPE.to_owned())];
    let authorization = match auth_method(document)? {
        ClientAuthMethod::Basic => Some(basic_authorization(client_id, client_secret)),
        ClientAuthMethod::Post => {
            form.push(("client_id", client_id.to_owned()));
            form.push(("client_secret", client_secret.to_owned()));
            None
        }
    };
    match token::request_authorized(
        "token request",
        &document.token_endpoint,
        form,
        authorization,
    )
    .await?
    {
        Outcome::Token(response) => Ok(AuthenticationToken {
            refresh_token: None,
            ..token::authentication_token(response)?
        }),
        Outcome::Refused(refusal) if refusal.error == "invalid_client" => {
            lore_debug!(
                "OIDC token endpoint at {} refused the credentials of client {client_id}: {}",
                document.issuer,
                refusal.describe()
            );
            Err(NotAuthenticated.into())
        }
        Outcome::Refused(refusal) => Err(ProtocolError::internal(format!(
            "OIDC token endpoint at {} refused the client credentials grant: {}",
            document.issuer,
            refusal.describe()
        ))),
    }
}
