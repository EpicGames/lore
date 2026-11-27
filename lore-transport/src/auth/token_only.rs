// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use lore_base::types::RepositoryId;
use lore_credential::JWTUserInfo;
use lore_credential::insecure_decode_token;

use crate::error::ProtocolError;
use crate::traits::UserService;
use crate::types::ResolvedUser;

/// The [`UserService`] for a deployment with no remote user directory.
///
/// Resolves the calling user's name from the token, if one is
/// available. Falls back to returning the user ID for tokenless users
/// and all the other users that do not match the token.
#[derive(Default)]
pub struct TokenOnlyUserService;

fn bearer(authz_token: &str) -> Option<JWTUserInfo> {
    insecure_decode_token(authz_token)
        .ok()
        .map(|decoded| decoded.claims)
}

fn bearer_display_name(bearer: &JWTUserInfo) -> String {
    [&bearer.name, &bearer.preferred_username]
        .into_iter()
        .flatten()
        .find(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| bearer.user_id.clone())
}

#[async_trait]
impl UserService for TokenOnlyUserService {
    async fn get_user_info(
        &self,
        _user_url: &str,
        authz_token: &str,
        _repository: RepositoryId,
        user_ids: &[String],
        _correlation_id: &str,
    ) -> Result<Vec<ResolvedUser>, ProtocolError> {
        let bearer = bearer(authz_token);
        let bearer_name = bearer.as_ref().map(bearer_display_name);
        Ok(user_ids
            .iter()
            .map(|id| {
                let user_name = match (&bearer, &bearer_name) {
                    (Some(bearer), Some(name)) if bearer.user_id == *id => name.clone(),
                    _ => id.clone(),
                };
                ResolvedUser {
                    user_id: id.clone(),
                    user_name,
                }
            })
            .collect())
    }

    async fn get_user_id(
        &self,
        _user_url: &str,
        authz_token: &str,
        _repository: RepositoryId,
        display_name: &str,
        _correlation_id: &str,
    ) -> Result<Option<ResolvedUser>, ProtocolError> {
        let Some(bearer) = bearer(authz_token) else {
            return Ok(None);
        };
        let is_bearer = [&bearer.preferred_username, &bearer.name]
            .into_iter()
            .flatten()
            .any(|name| name == display_name);
        Ok(is_bearer.then(|| ResolvedUser {
            user_id: bearer.user_id,
            user_name: display_name.to_string(),
        }))
    }
}
