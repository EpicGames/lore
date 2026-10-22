// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The refresh token grant (RFC 6749 §6): a refresh token traded at the token endpoint for a
//! new access token, without the user.

use lore_base::error::Disconnected;
use lore_base::error::NotAuthenticated;
use lore_base::lore_debug;

use super::PostError;
use super::discovery::DiscoveryDocument;
use super::token;
use super::token::Outcome;
use crate::error::ProtocolError;
use crate::types::AuthenticationToken;
use crate::types::Oidc;

pub const GRANT_TYPE: &str = "refresh_token";

/// Trades `refresh_token` for a new authentication token, as the public client `oidc` names.
/// No scope is sent, so the provider grants the scope of the login (§6). The answer carries
/// the rotated refresh token when the provider rotates them.
///
/// A refusal of the refresh token, `invalid_grant`, is [`NotAuthenticated`]: it is spent,
/// expired or revoked, and only a new login helps. [`Disconnected`] means the request was not
/// sent, so the refresh token is unspent. After any other failure the provider may have
/// consumed it.
pub async fn refresh(
    oidc: &Oidc,
    document: &DiscoveryDocument,
    refresh_token: &str,
) -> Result<AuthenticationToken, ProtocolError> {
    let form = vec![
        ("grant_type", GRANT_TYPE.to_owned()),
        ("refresh_token", refresh_token.to_owned()),
        ("client_id", oidc.client_id.clone()),
    ];
    let outcome =
        match token::request_once("token request", &document.token_endpoint, form, None).await {
            Ok(outcome) => outcome,
            Err(PostError::NotSent(_)) => return Err(Disconnected.into()),
            Err(PostError::MaybeSent(error)) if error.is_disconnected() => {
                return Err(ProtocolError::internal(
                    "the refresh token grant was interrupted after it was sent",
                ));
            }
            Err(PostError::MaybeSent(error)) => return Err(error),
        };
    match outcome {
        // An unusable answer is not `Disconnected`: the provider has consumed the token.
        Outcome::Token(response) => token::authentication_token(response),
        Outcome::Refused(refusal) if refusal.error == "invalid_grant" => {
            lore_debug!(
                "OIDC token endpoint at {} refused the refresh token: {}",
                oidc.issuer,
                refusal.describe()
            );
            Err(NotAuthenticated.into())
        }
        Outcome::Refused(refusal) => Err(ProtocolError::internal(format!(
            "OIDC token endpoint at {} refused the refresh token grant: {}",
            oidc.issuer,
            refusal.describe()
        ))),
    }
}
