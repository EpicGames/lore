// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use lore_base::types::RepositoryId;
use tonic::Status;

use super::repository_authorizer::RepositoryAuthorizer;
use super::repository_authorizer::VerifiedToken;
use crate::auth::jwt::ResourceMatcher;
use crate::auth::jwt::ResourcePermission;

/// Tier 2 authorizer: per-repository grants read from the token's resource
/// claim, with no network call.
///
/// The dotted claim path named by `[server.auth].resource_claim` holds the
/// resource entries. An entry matches a repository when its resource name
/// equals what `resource_id_template` renders for it, or equals
/// `resource_wildcard`. Permissions are merged across all matching entries.
/// A token with no matching resource entry is denied everything,
/// `action: None` included.
///
/// The two fields inside a resource entry follow the same rule: the resource
/// name is read from the field named by `[server.auth].resource_id_claim`
/// (default `resource_id`) and the actions from the field named by
/// `[server.auth].permission_claim` (default `permission`). The defaults
/// match legacy `UrcAuthApi` tokens; both are configurable for providers
/// whose entry shape cannot be changed, such as Keycloak's UMA `permissions`
/// entries carrying `rsname` and `scopes`.
pub struct ResourceGrantsAuthorizer {
    resource_claim: String,
    resource_id_claim: String,
    permission_claim: String,
    matcher: ResourceMatcher,
}

impl ResourceGrantsAuthorizer {
    pub fn new(
        resource_claim: String,
        resource_id_claim: String,
        permission_claim: Option<String>,
        resource_id_template: String,
        resource_wildcard: String,
    ) -> Self {
        Self {
            resource_claim,
            resource_id_claim,
            permission_claim: permission_claim.unwrap_or_else(|| "permission".to_string()),
            matcher: ResourceMatcher::new(resource_id_template, resource_wildcard),
        }
    }

    /// The token's grant entries.
    fn grants(&self, token: &VerifiedToken<'_>) -> Vec<ResourcePermission> {
        let Some(serde_json::Value::Array(entries)) = token.claims.claim_at(&self.resource_claim)
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| self.grant_entry(entry))
            .collect()
    }

    fn grant_entry(&self, entry: &serde_json::Value) -> Option<ResourcePermission> {
        let resource_id = value_at(entry, &self.resource_id_claim)?
            .as_str()?
            .to_string();
        let permission = match value_at(entry, &self.permission_claim) {
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect(),
            _ => Vec::new(),
        };
        Some(ResourcePermission {
            resource_id,
            permission,
        })
    }
}

/// Resolve a dotted path inside one grant entry.
fn value_at<'a>(value: &'a serde_json::Value, dotted_path: &str) -> Option<&'a serde_json::Value> {
    dotted_path
        .split('.')
        .try_fold(value, |value, segment| value.get(segment))
}

#[async_trait]
impl RepositoryAuthorizer for ResourceGrantsAuthorizer {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        let Some(token) = token else {
            return Err(Status::unauthenticated("No token"));
        };
        let grants = self.grants(token);
        if !self.matcher.any_match(&grants, repository_id) {
            return Err(Status::permission_denied("No grant for repository"));
        }
        match action {
            None => Ok(()),
            Some(action)
                if self
                    .matcher
                    .merged_permissions(&grants, repository_id)
                    .iter()
                    .any(|granted| granted == action) =>
            {
                Ok(())
            }
            Some(_) => Err(Status::permission_denied("Action not permitted")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use lore_base::types::Context;
    use serde_json::json;
    use tonic::Code;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;

    const REPOSITORY_HEX: &str = "0194b726b34e72b0b45550b88a967076";
    const UNRELATED_HEX: &str = "0192ae48ccf17060bc1ba9d04f6acb2f";

    fn repository(id: &str) -> RepositoryId {
        Context::from_str(id).unwrap().into()
    }

    fn default_authorizer() -> ResourceGrantsAuthorizer {
        ResourceGrantsAuthorizer::new(
            "resources".to_string(),
            "resource_id".to_string(),
            None,
            "urc-{id}".to_string(),
            "urc-*".to_string(),
        )
    }

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
        authorizer: &ResourceGrantsAuthorizer,
        claims: &AuthorizationToken,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        let token = VerifiedToken { raw: "raw", claims };
        authorizer
            .check_repository_access(Some(&token), repository_id, action)
            .await
    }

    /// `resource_claim = "resources"` with the default template reads the
    /// legacy claim exactly as today's matching does: `UrcAuthApi` tokens
    /// need no migration.
    #[tokio::test]
    async fn default_template_reproduces_the_legacy_matching() {
        let authorizer = default_authorizer();
        let claims = AuthorizationToken {
            resources: Some(vec![ResourcePermission {
                resource_id: format!("urc-{REPOSITORY_HEX}"),
                permission: vec!["obliterate".to_string()],
            }]),
            ..Default::default()
        };

        check(&authorizer, &claims, repository(REPOSITORY_HEX), None)
            .await
            .unwrap();
        check(
            &authorizer,
            &claims,
            repository(REPOSITORY_HEX),
            Some("obliterate"),
        )
        .await
        .unwrap();
        for action in [None, Some("obliterate")] {
            let err = check(&authorizer, &claims, repository(UNRELATED_HEX), action)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied);
        }
    }

    fn claims_with_resources(resources: Vec<ResourcePermission>) -> AuthorizationToken {
        // `resources` is a named field, and `claim_at` resolves named fields
        // before `extra` — exactly as decoding a real token would populate it.
        AuthorizationToken {
            resources: Some(resources),
            ..Default::default()
        }
    }

    fn entry(resource_id: &str, permissions: &[&str]) -> ResourcePermission {
        ResourcePermission {
            resource_id: resource_id.to_string(),
            permission: permissions.iter().map(ToString::to_string).collect(),
        }
    }

    #[tokio::test]
    async fn wildcard_entry_matches_every_repository() {
        let authorizer = default_authorizer();
        let claims = claims_with_resources(vec![entry("urc-*", &["migrate"])]);

        for repository in [repository(REPOSITORY_HEX), repository(UNRELATED_HEX)] {
            check(&authorizer, &claims, repository, None).await.unwrap();
            check(&authorizer, &claims, repository, Some("migrate"))
                .await
                .unwrap();
            let err = check(&authorizer, &claims, repository, Some("obliterate"))
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied);
        }
    }

    #[tokio::test]
    async fn permissions_merge_across_matching_entries() {
        let authorizer = default_authorizer();
        let claims = claims_with_resources(vec![
            entry(&format!("urc-{REPOSITORY_HEX}"), &["push"]),
            entry("urc-*", &["migrate"]),
            entry(&format!("urc-{REPOSITORY_HEX}"), &["obliterate"]),
        ]);

        for action in ["push", "migrate", "obliterate"] {
            check(
                &authorizer,
                &claims,
                repository(REPOSITORY_HEX),
                Some(action),
            )
            .await
            .unwrap();
        }
        // The unrelated repository sees only the wildcard entry's actions.
        check(
            &authorizer,
            &claims,
            repository(UNRELATED_HEX),
            Some("migrate"),
        )
        .await
        .unwrap();
        let err = check(
            &authorizer,
            &claims,
            repository(UNRELATED_HEX),
            Some("push"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test]
    async fn no_resource_claim_denies_everything_including_plain_access() {
        let authorizer = default_authorizer();
        let claims = token_with_extra(json!({}));

        for action in [None, Some("obliterate")] {
            let err = check(&authorizer, &claims, repository(REPOSITORY_HEX), action)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied);
        }
    }

    #[tokio::test]
    async fn permission_claim_names_the_actions_field() {
        let authorizer = ResourceGrantsAuthorizer::new(
            "authorization.grants".to_string(),
            "resource_id".to_string(),
            Some("actions".to_string()),
            "repo:{id}".to_string(),
            "repo:all".to_string(),
        );
        let claims = token_with_extra(json!({
            "authorization": {
                "grants": [
                    { "resource_id": format!("repo:{REPOSITORY_HEX}"), "actions": ["obliterate"] },
                ]
            }
        }));

        check(
            &authorizer,
            &claims,
            repository(REPOSITORY_HEX),
            Some("obliterate"),
        )
        .await
        .unwrap();
        let err = check(&authorizer, &claims, repository(UNRELATED_HEX), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    /// Keycloak's UMA `authorization.permissions` shape, which the provider
    /// cannot be configured out of: `rsname` names the resource and `scopes`
    /// the actions. Both entry field names are configuration, so this shape
    /// needs no mapper on the provider side.
    #[tokio::test]
    async fn resource_id_claim_names_the_resource_field() {
        let authorizer = ResourceGrantsAuthorizer::new(
            "authorization.permissions".to_string(),
            "rsname".to_string(),
            Some("scopes".to_string()),
            "urc-{id}".to_string(),
            "urc-*".to_string(),
        );
        let claims = token_with_extra(json!({
            "authorization": {
                "permissions": [
                    { "rsname": format!("urc-{REPOSITORY_HEX}"), "scopes": ["obliterate"] },
                    // An entry carrying `resource_id` under this config does
                    // not name a resource at all, so it is skipped.
                    { "resource_id": format!("urc-{UNRELATED_HEX}"), "scopes": ["obliterate"] },
                ]
            }
        }));

        check(
            &authorizer,
            &claims,
            repository(REPOSITORY_HEX),
            Some("obliterate"),
        )
        .await
        .unwrap();
        let err = check(&authorizer, &claims, repository(UNRELATED_HEX), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test]
    async fn malformed_entries_are_skipped_rather_than_fatal() {
        // A provider emitting loose shapes emits them under a custom claim:
        // the typed `resources` field could not even decode these. A custom
        // claim lands in `extra`, where every shape reaches the reader.
        let authorizer = ResourceGrantsAuthorizer::new(
            "grants".to_string(),
            "resource_id".to_string(),
            None,
            "urc-{id}".to_string(),
            "urc-*".to_string(),
        );
        let claims = token_with_extra(json!({
            "grants": [
                "not an object",
                { "no_resource_id": true },
                { "resource_id": 42 },
                { "resource_id": format!("urc-{REPOSITORY_HEX}"), "permission": "not an array" },
                { "resource_id": format!("urc-{REPOSITORY_HEX}"), "permission": [1, null, "push"] },
            ]
        }));

        // The malformed-actions entries still name the resource, so
        // reachability holds while their unreadable actions grant nothing.
        check(&authorizer, &claims, repository(REPOSITORY_HEX), None)
            .await
            .unwrap();
        check(
            &authorizer,
            &claims,
            repository(REPOSITORY_HEX),
            Some("push"),
        )
        .await
        .unwrap();
        let err = check(
            &authorizer,
            &claims,
            repository(REPOSITORY_HEX),
            Some("obliterate"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);

        // A claim that is not an array at all denies everything.
        let claims = token_with_extra(json!({ "grants": "not an array" }));
        let err = check(&authorizer, &claims, repository(REPOSITORY_HEX), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test]
    async fn no_token_denies_even_plain_access() {
        let authorizer = default_authorizer();
        for action in [None, Some("obliterate")] {
            let err = authorizer
                .check_repository_access(None, repository(REPOSITORY_HEX), action)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::Unauthenticated);
        }
    }
}
