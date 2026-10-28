// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Fixtures for the `push-protected` action on the branch push handlers: one
//! authorizer per tier, and a request carrying a verified token that holds the
//! action or does not.
use std::sync::Arc;

use lore_base::types::RepositoryId;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::global_grants_authorizer::GlobalGrantsAuthorizer;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::AuthClientAuthorizer;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use tonic::Request;

pub const PUSH_PROTECTED: &str = "push-protected";

#[derive(Clone, Copy, Debug)]
pub enum Tier {
    /// Actions in the `groups` claim.
    Global,
    /// Actions in the `resources` claim, per repository.
    Resource,
    /// The legacy authorizer reading an access token's `resources` claim. The
    /// auth URL is one nothing listens on, so a verdict proves the claim
    /// answered in place.
    AuthClient,
}

pub const TIERS: [Tier; 3] = [Tier::Global, Tier::Resource, Tier::AuthClient];

pub fn allow_all() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(AllowAllRepositoryAuthorizer)
}

impl Tier {
    pub fn authorizer(self) -> Arc<dyn RepositoryAuthorizer> {
        match self {
            Self::Global => Arc::new(GlobalGrantsAuthorizer::new(
                Some("groups".to_string()),
                vec![Action::Read, Action::Write],
            )),
            Self::Resource => Arc::new(resource_grants_authorizer()),
            Self::AuthClient => Arc::new(AuthClientAuthorizer::new(
                "https://auth.invalid".to_string(),
            )),
        }
    }

    /// Attaches a verified token to `request` that holds `actions` on `repository`.
    pub fn with_token<T>(
        self,
        mut request: Request<T>,
        repository: RepositoryId,
        actions: &[&str],
    ) -> Request<T> {
        let actions: Vec<String> = actions.iter().map(ToString::to_string).collect();
        let resources = vec![ResourcePermission {
            resource_id: format!("urc-{repository}"),
            permission: actions.clone(),
        }];
        let claims = match self {
            Self::Global => AuthorizationToken {
                user_id: "pusher".into(),
                groups: Some(actions),
                ..Default::default()
            },
            Self::Resource | Self::AuthClient => AuthorizationToken {
                user_id: "pusher".into(),
                resources: Some(resources),
                ..Default::default()
            },
        };
        request.extensions_mut().insert(claims);
        request
            .extensions_mut()
            .insert(RawToken("raw.jwt".to_string()));
        request
    }
}

fn resource_grants_authorizer() -> ResourceGrantsAuthorizer {
    ResourceGrantsAuthorizer::new(
        "resources".to_string(),
        "resource_id".to_string(),
        None,
        "urc-{id}".to_string(),
        "urc-*".to_string(),
    )
}
