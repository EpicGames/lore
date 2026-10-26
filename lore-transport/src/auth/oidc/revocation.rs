// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Token revocation (RFC 7009): the provider invalidates a refresh token the client no longer
//! needs.

use lore_base::error::NotSupported;
use lore_base::lore_debug;

use super::FormResponse;
use super::discovery::DiscoveryDocument;
use super::on_net_runtime;
use super::post_form;
use super::token::ErrorResponse;
use crate::error::ProtocolError;
use crate::types::Oidc;

/// Asks the provider to revoke `refresh_token`, as the public client `oidc` names (§2.1).
///
/// [`NotSupported`] when the discovery document advertises no revocation endpoint.
pub async fn revoke(
    oidc: &Oidc,
    document: &DiscoveryDocument,
    refresh_token: &str,
) -> Result<(), ProtocolError> {
    let Some(endpoint) = document.revocation_endpoint.clone() else {
        return Err(NotSupported {
            operation: format!(
                "token revocation at OIDC issuer {}: it advertises no revocation endpoint",
                oidc.issuer
            ),
        }
        .into());
    };
    let form = [
        ("token", refresh_token.to_owned()),
        ("token_type_hint", "refresh_token".to_owned()),
        ("client_id", oidc.client_id.clone()),
    ];
    let response = on_net_runtime(async move {
        let form: Vec<(&str, &str)> = form
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        post_form("revocation", &endpoint, &form, None)
            .await
            .map_err(ProtocolError::from)
    })
    .await?;
    answer(oidc, &response)
}

/// §2.2: any success revokes the token, or finds it invalid already, which a client treats
/// alike. The body of a success carries nothing.
fn answer(oidc: &Oidc, response: &FormResponse) -> Result<(), ProtocolError> {
    if response.status.is_success() {
        return Ok(());
    }
    lore_debug!(
        "OIDC revocation request answered HTTP {}: {}",
        response.status.as_u16(),
        response.body_text()
    );
    Err(ProtocolError::internal(
        match serde_json::from_slice::<ErrorResponse>(&response.body) {
            Ok(refusal) => format!(
                "OIDC revocation endpoint at {} refused the request: {}",
                oidc.issuer,
                refusal.describe()
            ),
            Err(_) => format!(
                "OIDC revocation request answered HTTP {}",
                response.status.as_u16()
            ),
        },
    ))
}
