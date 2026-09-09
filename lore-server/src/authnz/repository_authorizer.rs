// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::types::RepositoryId;
use lore_proto::auth::CheckUserPermissionRequest;
use lore_proto::auth::CheckUserPermissionResponse;
use tonic::Code;
use tonic::Status;

use super::auth::grpc_get_auth_client;
use super::common::create_request_with_authorization;
use crate::auth::jwt::AuthorizationToken;
use crate::grpc::ServerResultExt;

/// The bearer token exactly as it arrived, without the `Bearer ` prefix.
/// The interceptors insert it into request extensions beside the decoded
/// [`AuthorizationToken`] so handlers can rebuild a [`VerifiedToken`].
#[derive(Clone)]
pub struct RawToken(pub String);

/// A token the interceptor has already verified. Claim-reading authorizers
/// use `claims`. [`AuthClientAuthorizer`] forwards `raw` upstream.
pub struct VerifiedToken<'a> {
    pub raw: &'a str,
    pub claims: &'a AuthorizationToken,
}

#[async_trait]
pub trait RepositoryAuthorizer: Send + Sync {
    /// Whether `token` may reach `repository_id` at all (`action: None`), or
    /// may perform the named privileged action on it (`action: Some`).
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status>;
}

/// Always allows access. Used when no auth URL is configured.
pub struct AllowAllRepositoryAuthorizer;

#[async_trait]
impl RepositoryAuthorizer for AllowAllRepositoryAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        _action: Option<&str>,
    ) -> Result<(), Status> {
        Ok(())
    }
}

/// Checks repository access against the Lore auth service.
pub struct AuthClientAuthorizer {
    auth_url: String,
}

impl AuthClientAuthorizer {
    pub fn new(auth_url: String) -> Self {
        Self { auth_url }
    }

    /// The pre-`VerifiedToken` entry point: takes the `authorization` header
    /// value verbatim.
    pub(crate) async fn check_access_with_header(
        &self,
        authorization: Option<String>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        let mut client = grpc_get_auth_client(self.auth_url.clone()).await?;
        let resource_id = format!("urc-{repository_id}");
        let request = check_user_permission_request(resource_id.clone(), authorization)?;

        let permissions = client
            .check_user_permission(request)
            .await
            .warn_map_err(|err| {
                if err.code() == Code::PermissionDenied {
                    return Status::permission_denied("Query resource denied");
                } else if err.code() == Code::Unauthenticated {
                    return Status::unauthenticated("Query resource failed - unauthenticated");
                }
                Status::internal(format!("Failed to call auth check_user_permission: {err}"))
            })?;

        evaluate_check_user_permission(&permissions.into_inner(), &resource_id, action)
    }
}

fn check_user_permission_request(
    resource_id: String,
    authorization: Option<String>,
) -> Result<tonic::Request<CheckUserPermissionRequest>, Status> {
    create_request_with_authorization(
        CheckUserPermissionRequest {
            resource_id: vec![resource_id],
            target_user: None,
        },
        authorization,
    )
}

fn bearer_header(token: Option<&VerifiedToken<'_>>) -> Option<String> {
    token.map(|token| format!("Bearer {}", token.raw))
}

/// Answer an access question from a `CheckUserPermission` response.
///
/// `action: None` checks whether the token contains the resource at all.
/// `action: Some(str)` checks whether the token contains a given resource
/// with the named action.
fn evaluate_check_user_permission(
    response: &CheckUserPermissionResponse,
    resource_id: &str,
    action: Option<&str>,
) -> Result<(), Status> {
    match action {
        None => {
            if response
                .allowed_resource_permission
                .first()
                .ok_or(Status::internal("No permissions for resource"))?
                .resource_id
                == resource_id
            {
                Ok(())
            } else {
                Err(Status::internal("Unexpected resource_id"))
            }
        }
        Some(action) => {
            let permitted = response
                .allowed_resource_permission
                .iter()
                .filter(|entry| entry.resource_id == resource_id)
                .any(|entry| entry.permission.iter().any(|granted| granted == action));
            if permitted {
                Ok(())
            } else {
                Err(Status::permission_denied("Action not permitted"))
            }
        }
    }
}

#[async_trait]
impl RepositoryAuthorizer for AuthClientAuthorizer {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        self.check_access_with_header(bearer_header(token), repository_id, action)
            .await
    }
}

/// Creates the appropriate authorizer from an optional auth URL.
/// Returns `AllowAllRepositoryAuthorizer` when no URL is configured.
pub fn repository_authorizer(auth_url: Option<String>) -> Arc<dyn RepositoryAuthorizer> {
    match auth_url {
        Some(url) => Arc::new(AuthClientAuthorizer::new(url)),
        None => Arc::new(AllowAllRepositoryAuthorizer),
    }
}

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_proto::auth::ResourcePermission;

    use super::*;

    fn response(entries: Vec<ResourcePermission>) -> CheckUserPermissionResponse {
        CheckUserPermissionResponse {
            allowed_resource_permission: entries,
            denied_resource_permission: vec![],
        }
    }

    fn entry(resource_id: &str, permissions: &[&str]) -> ResourcePermission {
        ResourcePermission {
            resource_id: resource_id.to_string(),
            permission: permissions.iter().map(ToString::to_string).collect(),
        }
    }

    #[tokio::test]
    async fn allow_all_permits_every_token_action_combination() {
        let claims = AuthorizationToken::default();
        let token = VerifiedToken {
            raw: "raw",
            claims: &claims,
        };
        let repository: RepositoryId = Context::default().into();
        for token in [None, Some(&token)] {
            for action in [None, Some("obliterate")] {
                AllowAllRepositoryAuthorizer
                    .check_repository_access(token, repository, action)
                    .await
                    .unwrap();
            }
        }
    }

    #[test]
    fn bearer_header_rebuilds_the_forwarded_header() {
        let claims = AuthorizationToken::default();
        let token = VerifiedToken {
            raw: "abc.def.ghi",
            claims: &claims,
        };
        assert_eq!(
            bearer_header(Some(&token)),
            Some("Bearer abc.def.ghi".to_string())
        );
        assert_eq!(bearer_header(None), None);
    }

    #[test]
    fn upstream_request_carries_resource_and_authorization() {
        let request =
            check_user_permission_request("urc-abc".into(), Some("Bearer tok".into())).unwrap();
        assert_eq!(request.get_ref().resource_id, vec!["urc-abc".to_string()]);
        assert_eq!(request.get_ref().target_user, None);
        assert_eq!(
            request
                .metadata()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok"
        );
    }

    #[test]
    fn named_action_requires_membership_in_the_permission_list() {
        let response = response(vec![entry("urc-abc", &["obliterate"])]);
        evaluate_check_user_permission(&response, "urc-abc", Some("obliterate")).unwrap();
        evaluate_check_user_permission(&response, "urc-abc", None).unwrap();
        // The fail-open case: an authorizer that ignores the action would
        // permit this.
        let err =
            evaluate_check_user_permission(&response, "urc-abc", Some("presign")).unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[test]
    fn empty_permission_list_denies_every_named_action() {
        let response = response(vec![entry("urc-abc", &[])]);
        let err =
            evaluate_check_user_permission(&response, "urc-abc", Some("obliterate")).unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
        // The resource still appears, which is all `None` asks.
        evaluate_check_user_permission(&response, "urc-abc", None).unwrap();
    }

    #[test]
    fn absent_resource_denies_named_actions_and_plain_access() {
        let response = response(vec![]);
        let err =
            evaluate_check_user_permission(&response, "urc-abc", Some("obliterate")).unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
        evaluate_check_user_permission(&response, "urc-abc", None).unwrap_err();
    }

    #[test]
    fn mismatched_resource_denies() {
        let response = response(vec![entry("urc-other", &["obliterate"])]);
        let err =
            evaluate_check_user_permission(&response, "urc-abc", Some("obliterate")).unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
        evaluate_check_user_permission(&response, "urc-abc", None).unwrap_err();
    }
}
