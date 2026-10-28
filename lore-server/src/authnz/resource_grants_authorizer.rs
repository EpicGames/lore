// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use lore_base::types::RepositoryId;
use tonic::Status;

use super::repository_authorizer::Action;
use super::repository_authorizer::Grants;
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
/// A token with no matching resource entry is denied everything.
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

impl ResourceGrantsAuthorizer {
    /// The token's access to one partition: unreachable when no entry
    /// matches, otherwise the permissions merged across every matching entry.
    fn grants_on(&self, token: &VerifiedToken<'_>, repository_id: RepositoryId) -> Grants {
        let entries = self.grants(token);
        if !self.matcher.any_match(&entries, repository_id) {
            return Grants::Denied;
        }
        Grants::Actions(
            self.matcher
                .merged_permissions(&entries, repository_id)
                .into_iter()
                .collect(),
        )
    }

    /// The whole verdict is in the token, so the async and sync trait
    /// methods share this body. Answers in place rather than through
    /// [`grants_on`](Self::grants_on): the link-read closure asks this per
    /// link, and the merged permission set is only worth building for an
    /// enumeration.
    fn check(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        let Some(token) = token else {
            return Err(Status::unauthenticated("No token"));
        };
        let entries = self.grants(token);
        if !self.matcher.any_match(&entries, repository_id) {
            return Err(Status::permission_denied("No grant for repository"));
        }
        if self.matcher.permits(&entries, repository_id, action) {
            Ok(())
        } else {
            Err(Status::permission_denied("Action not permitted"))
        }
    }
}

#[async_trait]
impl RepositoryAuthorizer for ResourceGrantsAuthorizer {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        self.check(token, repository_id, action)
    }

    /// Any verified token: the client looks a repository up before it can exchange for a
    /// token naming it, and a record discloses no more than `RepositoryCreate` refusing a
    /// taken name already does.
    async fn check_repository_lookup(
        &self,
        token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
    ) -> Result<(), Status> {
        match token {
            Some(_) => Ok(()),
            None => Err(Status::unauthenticated("No token")),
        }
    }

    fn check_repository_access_sync(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Action,
    ) -> Option<Result<(), Status>> {
        Some(self.check(token, repository_id, action))
    }

    async fn granted_actions(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        Ok(Some(match token {
            None => Grants::Denied,
            Some(token) => self.grants_on(token, repository_id),
        }))
    }
}
