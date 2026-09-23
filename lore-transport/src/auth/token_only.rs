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
    [&bearer.preferred_username, &bearer.name]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `{"iss":"lore","sub":"alice","name":"Alice Example","preferred_username":"alice.e","exp":2000000000,"aud":["example.com"]}`
    const FULL: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UgRXhhbXBsZSIsInByZWZlcnJlZF91c2VybmFtZSI6ImFsaWNlLmUiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";
    /// `{"iss":"lore","sub":"alice","name":"Alice Example","exp":2000000000,"aud":["example.com"]}`
    const NAME_ONLY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UgRXhhbXBsZSIsImV4cCI6MjAwMDAwMDAwMCwiYXVkIjpbImV4YW1wbGUuY29tIl19.signature";
    /// `{"iss":"lore","sub":"alice","exp":2000000000,"aud":["example.com"]}`
    const NAMELESS: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

    async fn info(token: &str, ids: &[&str]) -> Vec<(String, String)> {
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        TokenOnlyUserService
            .get_user_info("", token, RepositoryId::default(), &ids, "")
            .await
            .expect("the default never fails")
            .into_iter()
            .map(|user| (user.user_id, user.user_name))
            .collect()
    }

    async fn id(token: &str, display_name: &str) -> Option<String> {
        TokenOnlyUserService
            .get_user_id("", token, RepositoryId::default(), display_name, "")
            .await
            .expect("the default never fails")
            .map(|user| user.user_id)
    }

    fn pair(id: &str, name: &str) -> (String, String) {
        (id.to_string(), name.to_string())
    }

    #[tokio::test]
    async fn bearer_is_named_from_the_token_and_others_are_echoed() {
        assert_eq!(
            info(FULL, &["bob", "alice", "carol"]).await,
            vec![
                pair("bob", "bob"),
                pair("alice", "alice.e"),
                pair("carol", "carol"),
            ],
            "one answer per requested id, in request order"
        );
    }

    #[tokio::test]
    async fn bearer_name_falls_back_from_preferred_username_to_name_to_id() {
        assert_eq!(info(FULL, &["alice"]).await, vec![pair("alice", "alice.e")]);
        assert_eq!(
            info(NAME_ONLY, &["alice"]).await,
            vec![pair("alice", "Alice Example")]
        );
        assert_eq!(
            info(NAMELESS, &["alice"]).await,
            vec![pair("alice", "alice")]
        );
    }

    #[tokio::test]
    async fn an_unreadable_token_names_nobody() {
        assert_eq!(
            info("not-a-jwt", &["alice", "bob"]).await,
            vec![pair("alice", "alice"), pair("bob", "bob")]
        );
        assert_eq!(id("not-a-jwt", "alice.e").await, None);
    }

    #[tokio::test]
    async fn only_the_bearers_names_resolve_to_an_id() {
        assert_eq!(id(FULL, "alice.e").await, Some("alice".to_string()));
        assert_eq!(id(FULL, "Alice Example").await, Some("alice".to_string()));
        assert_eq!(id(FULL, "bob").await, None);
        // The bearer's ID is not one of their names.
        assert_eq!(id(FULL, "alice").await, None);
        assert_eq!(id(NAMELESS, "alice").await, None);
    }
}
