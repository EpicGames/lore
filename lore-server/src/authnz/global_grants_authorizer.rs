// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;

use async_trait::async_trait;
use lore_base::types::RepositoryId;
use tonic::Status;

use super::repository_authorizer::Grants;
use super::repository_authorizer::RepositoryAuthorizer;
use super::repository_authorizer::VerifiedToken;

/// Tier 1 authorizer: actions are granted globally, so the repository
/// parameter is ignored entirely. The caller's action set is read from the
/// dotted claim path named by `[server.auth].permission_claim`, which is an
/// ordinary role or group claim such as `realm_access.roles` or `groups`.
pub struct GlobalGrantsAuthorizer {
    permission_claim: Option<String>,
}

impl GlobalGrantsAuthorizer {
    pub fn new(permission_claim: Option<String>) -> Self {
        Self { permission_claim }
    }

    /// Any authenticated principal reaches every partition on this tier. The
    /// action set is the permission claim's string values, empty when the
    /// claim is unconfigured or absent.
    fn grants_for(&self, token: &VerifiedToken<'_>) -> Grants {
        let Some(claim) = &self.permission_claim else {
            return Grants::Actions(HashSet::new());
        };
        let Some(serde_json::Value::Array(values)) = token.claims.claim_at(claim) else {
            return Grants::Actions(HashSet::new());
        };
        Grants::Actions(
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect(),
        )
    }

    /// The whole verdict is in the token, so the async and sync trait
    /// methods share this body.
    fn check(
        &self,
        token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        let Some(token) = token else {
            return Err(Status::unauthenticated("No token"));
        };
        match action {
            // action == None -> just check that the user has a valid token
            None => Ok(()),
            Some(action) if self.grants_for(token).permits(action) => Ok(()),
            Some(_) => Err(Status::permission_denied("Action not permitted")),
        }
    }
}

#[async_trait]
impl RepositoryAuthorizer for GlobalGrantsAuthorizer {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        self.check(token, repository_id, action)
    }

    fn check_repository_access_sync(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Option<Result<(), Status>> {
        Some(self.check(token, repository_id, action))
    }

    async fn granted_actions(
        &self,
        token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        Ok(Some(match token {
            None => Grants::Denied,
            Some(token) => self.grants_for(token),
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tonic::Code;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;

    fn token_with_extra(extra: serde_json::Value) -> AuthorizationToken {
        let serde_json::Value::Object(extra) = extra else {
            panic!("extra claims must be a JSON object");
        };
        AuthorizationToken {
            user_id: "the u".to_string(),
            extra,
            ..Default::default()
        }
    }

    async fn check(
        authorizer: &GlobalGrantsAuthorizer,
        claims: &AuthorizationToken,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        let token = VerifiedToken { raw: "raw", claims };
        authorizer
            .check_repository_access(Some(&token), repository_id, action)
            .await
    }

    #[tokio::test]
    async fn nested_claim_grants_the_action_on_any_partition() {
        let authorizer = GlobalGrantsAuthorizer::new(Some("realm_access.roles".to_string()));
        let claims = token_with_extra(json!({
            "realm_access": { "roles": ["obliterate"] }
        }));
        for repository in [RepositoryId::default(), RepositoryId::from([7u8; 16])] {
            check(&authorizer, &claims, repository, Some("obliterate"))
                .await
                .unwrap();
            let err = check(&authorizer, &claims, repository, Some("admin"))
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied);
        }
    }

    #[tokio::test]
    async fn flat_claim_grants_the_action_on_any_partition() {
        // Dex emits its grants as a flat `groups` claim.
        let authorizer = GlobalGrantsAuthorizer::new(Some("groups".to_string()));
        let claims = AuthorizationToken {
            groups: Some(vec!["obliterate".to_string()]),
            ..Default::default()
        };
        check(
            &authorizer,
            &claims,
            RepositoryId::default(),
            Some("obliterate"),
        )
        .await
        .unwrap();
        let err = check(&authorizer, &claims, RepositoryId::default(), Some("admin"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test]
    async fn no_permission_claim_permits_plain_access_and_denies_every_action() {
        let authorizer = GlobalGrantsAuthorizer::new(None);
        let claims = token_with_extra(json!({
            "realm_access": { "roles": ["obliterate"] }
        }));
        check(&authorizer, &claims, RepositoryId::default(), None)
            .await
            .unwrap();
        for action in ["obliterate", "admin", "presign"] {
            let err = check(&authorizer, &claims, RepositoryId::default(), Some(action))
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied);
        }
    }

    #[tokio::test]
    async fn named_action_permits_plain_access_too() {
        let authorizer = GlobalGrantsAuthorizer::new(Some("groups".to_string()));
        let claims = token_with_extra(json!({ "groups": ["obliterate"] }));
        check(&authorizer, &claims, RepositoryId::default(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn non_string_values_are_ignored_rather_than_panicking() {
        // `roles` is not a named field, so these shapes reach the reader
        // as-is instead of being shadowed by a typed field.
        let authorizer = GlobalGrantsAuthorizer::new(Some("roles".to_string()));
        // A whole claim of the wrong shape denies.
        for wrong_shape in [
            json!({ "roles": "obliterate" }),
            json!({ "roles": 42 }),
            json!({ "roles": { "obliterate": true } }),
        ] {
            let claims = token_with_extra(wrong_shape);
            let err = check(
                &authorizer,
                &claims,
                RepositoryId::default(),
                Some("obliterate"),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied);
        }
        // Non-string entries inside the array are skipped, not fatal.
        let claims = token_with_extra(json!({ "roles": [1, null, "obliterate"] }));
        check(
            &authorizer,
            &claims,
            RepositoryId::default(),
            Some("obliterate"),
        )
        .await
        .unwrap();
    }

    /// The verdict is in the token, so the sync check always answers and
    /// agrees with the async one.
    #[tokio::test]
    async fn sync_check_answers_and_agrees_with_the_async_path() {
        let authorizer = GlobalGrantsAuthorizer::new(Some("groups".to_string()));
        let claims = token_with_extra(json!({ "groups": ["obliterate"] }));
        let token = VerifiedToken {
            raw: "raw",
            claims: &claims,
        };
        for token in [None, Some(&token)] {
            for action in [None, Some("obliterate"), Some("admin")] {
                let sync = authorizer
                    .check_repository_access_sync(token, RepositoryId::default(), action)
                    .expect("the token holds the verdict");
                let asynchronous = authorizer
                    .check_repository_access(token, RepositoryId::default(), action)
                    .await;
                assert_eq!(sync.is_ok(), asynchronous.is_ok(), "{action:?}");
            }
        }
    }

    #[tokio::test]
    async fn no_token_denies_even_plain_access() {
        let authorizer = GlobalGrantsAuthorizer::new(Some("groups".to_string()));
        for action in [None, Some("obliterate")] {
            let err = authorizer
                .check_repository_access(None, RepositoryId::default(), action)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::Unauthenticated);
        }
    }
}
