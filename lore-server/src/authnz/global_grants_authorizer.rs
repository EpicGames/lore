// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;

use async_trait::async_trait;
use lore_base::types::RepositoryId;
use tonic::Status;

use super::repository_authorizer::Action;
use super::repository_authorizer::Grants;
use super::repository_authorizer::RepositoryAuthorizer;
use super::repository_authorizer::VerifiedToken;

/// Tier 1 authorizer: actions are granted globally, so the repository
/// parameter is ignored entirely. The caller's action set is read from the
/// dotted claim path named by `[server.auth].permission_claim`, which is an
/// ordinary role or group claim such as `realm_access.roles` or `groups`,
/// plus `[server.auth].implicit_actions`, which every token holder gets.
pub struct GlobalGrantsAuthorizer {
    permission_claim: Option<String>,
    implicit_actions: HashSet<Action>,
}

impl GlobalGrantsAuthorizer {
    pub fn new(permission_claim: Option<String>, implicit_actions: Vec<Action>) -> Self {
        Self {
            permission_claim,
            implicit_actions: implicit_actions.into_iter().collect(),
        }
    }

    /// [`grants_for`](Self::grants_for)`(token).permits(action)`, answered
    /// without allocating.
    fn permits(&self, token: &VerifiedToken<'_>, action: Action) -> bool {
        action == Action::Read
            || self.implicit_actions.contains(&action)
            || self.permission_claim.as_ref().is_some_and(|claim| {
                matches!(
                    token.claims.claim_at(claim),
                    Some(serde_json::Value::Array(values))
                        if values.iter().any(|value| value.as_str() == Some(action.as_str()))
                )
            })
    }

    /// Any authenticated principal reaches every partition on this tier.
    fn grants_for(&self, token: &VerifiedToken<'_>) -> Grants {
        let mut actions: HashSet<String> = self
            .implicit_actions
            .iter()
            .map(|action| action.as_str().to_string())
            .collect();
        if let Some(claim) = &self.permission_claim
            && let Some(serde_json::Value::Array(values)) = token.claims.claim_at(claim)
        {
            actions.extend(
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToString::to_string),
            );
        }
        Grants::Actions(actions)
    }

    /// The whole verdict is in the token, so the async and sync trait
    /// methods share this body.
    fn check(
        &self,
        token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        let Some(token) = token else {
            return Err(Status::unauthenticated("No token"));
        };
        if self.permits(token, action) {
            Ok(())
        } else {
            Err(Status::permission_denied("Action not permitted"))
        }
    }
}

#[async_trait]
impl RepositoryAuthorizer for GlobalGrantsAuthorizer {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        self.check(token, repository_id, action)
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
        _repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        Ok(Some(match token {
            None => Grants::Denied,
            Some(token) => self.grants_for(token),
        }))
    }
}
