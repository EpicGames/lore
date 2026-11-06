// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_transport::Oidc;
use tracing::info;

use crate::auth::jwk::JWKServiceError;
use crate::auth::jwk::fetch_discovery_document;
use crate::settings::AuthSettings;

/// Resolves the OIDC provider info advertised for the clients.
pub async fn advertised_oidc(
    auth: &AuthSettings,
    auth_url: Option<&str>,
) -> Result<Option<Oidc>, JWKServiceError> {
    let Some(oidc) = auth.oidc.as_ref() else {
        return Ok(None);
    };
    let issuer = auth
        .jwt_issuer
        .first()
        .ok_or(JWKServiceError::EndpointUnresolvable)?;
    let document = fetch_discovery_document(issuer).await?;
    info!(issuer = %document.issuer, "Advertising the OIDC provider to clients");
    // An empty string counts as unset, as it does in the settings validation.
    let non_empty = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    let resource_template = non_empty(&oidc.resource_template);
    let scope_template = non_empty(&oidc.scope_template);
    let is_tier_2 = resource_template.is_some() || scope_template.is_some();
    let token_exchange_issuer = is_tier_2
        .then(|| non_empty(&oidc.token_exchange_issuer).unwrap_or_else(|| document.issuer.clone()));
    Ok(Some(Oidc {
        issuer: document.issuer,
        client_id: oidc.client_id.clone(),
        scopes: oidc.scopes.clone(),
        preferred: oidc.preferred || auth_url.is_none(),
        resource_template,
        scope_template,
        token_exchange_issuer,
        client_credentials_scopes: oidc.client_credentials_scopes.clone(),
    }))
}
