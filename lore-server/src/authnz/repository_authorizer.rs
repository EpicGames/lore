// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt;
use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use lore_base::types::RepositoryId;
use lore_proto::auth::CheckUserPermissionRequest;
use lore_proto::auth::CheckUserPermissionResponse;
use tonic::Code;
use tonic::Status;
use tracing::info;

use super::auth::grpc_get_auth_client;
use super::common::create_request_with_authorization;
use super::global_grants_authorizer::GlobalGrantsAuthorizer;
use super::resource_grants_authorizer::ResourceGrantsAuthorizer;
use crate::auth::jwt::AuthorizationToken;
use crate::grpc::ServerResultExt;
use crate::settings::AuthSettings;

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

/// Always allows access. Selected when no `[server.auth]` is configured:
/// nothing verifies tokens, so a local server keeps working unchecked.
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

/// Which implementation [`repository_authorizer`] selects for a
/// configuration. Selection is separate from construction so tests can
/// assert it and the startup log can name the tier a deployment landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorizerSelection {
    /// No `[server.auth]`: all requests are allowed.
    AllowAll,
    /// Legacy `UrcAuthApi` deployment: an online `CheckUserPermission` call
    /// answers each check.
    AuthClient,
    /// OIDC Tier 1: verify global actions from `permission_claim`.
    GlobalGrants,
    /// OIDC Tier 2: verify per-repository grants from the resource claim.
    ResourceGrants,
}

impl fmt::Display for AuthorizerSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AllowAll => "AllowAllRepositoryAuthorizer",
            Self::AuthClient => "AuthClientAuthorizer",
            Self::GlobalGrants => "GlobalGrantsAuthorizer",
            Self::ResourceGrants => "ResourceGrantsAuthorizer",
        })
    }
}

/// The four-way selection:
/// - neither `[server.auth]` nor `auth_url` → allow-all
/// - `auth_url` set → the gRPC online auth check
/// - `resource_claim` set → `ResourceGrants`
/// - otherwise → `GlobalGrants`
pub fn select_repository_authorizer(
    auth: Option<&AuthSettings>,
    auth_url: Option<&str>,
) -> anyhow::Result<AuthorizerSelection> {
    let Some(auth) = auth else {
        return match auth_url {
            None => Ok(AuthorizerSelection::AllowAll),
            Some(_) => bail!(
                "[environment.endpoint] auth_url is set but [server.auth] is not: without \
                 [server.auth] tokens are not verified. Add [server.auth] (jwt_issuer, jwt_audience) \
                 to enable verification, or remove auth_url."
            ),
        };
    };
    match (auth_url, auth.resource_claim.as_deref()) {
        (Some(_), Some(_)) => bail!(
            "[environment.endpoint] auth_url and [server.auth] resource_claim are both set: \
             with auth_url configured, every check calls the auth service and resource_claim \
             does nothing. Remove auth_url to authorize from the token's resource claim, or \
             remove resource_claim to stay on the gRPC auth service."
        ),
        (Some(_), None) => Ok(AuthorizerSelection::AuthClient),
        (None, Some(_)) => Ok(AuthorizerSelection::ResourceGrants),
        (None, None) => Ok(AuthorizerSelection::GlobalGrants),
    }
}

/// Creates the authorizer [`select_repository_authorizer`] picks for this
/// configuration. Built once at startup and shared by every server.
pub fn repository_authorizer(
    auth: Option<&AuthSettings>,
    auth_url: Option<String>,
) -> anyhow::Result<Arc<dyn RepositoryAuthorizer>> {
    let selection = select_repository_authorizer(auth, auth_url.as_deref())?;
    info!("Repository authorizer: {selection}");
    Ok(match selection {
        AuthorizerSelection::AllowAll => Arc::new(AllowAllRepositoryAuthorizer),
        AuthorizerSelection::AuthClient => Arc::new(AuthClientAuthorizer::new(
            auth_url.expect("AuthClient is only selected when auth_url is set"),
        )),
        AuthorizerSelection::GlobalGrants => {
            let auth = auth.expect("GlobalGrants is only selected under [server.auth]");
            Arc::new(GlobalGrantsAuthorizer::new(auth.permission_claim.clone()))
        }
        AuthorizerSelection::ResourceGrants => {
            let auth = auth.expect("ResourceGrants is only selected under [server.auth]");
            Arc::new(ResourceGrantsAuthorizer::new(
                auth.resource_claim
                    .clone()
                    .expect("ResourceGrants is only selected with resource_claim set"),
                auth.resource_id_claim.clone(),
                auth.permission_claim.clone(),
                auth.resource_id_template.clone(),
                auth.resource_wildcard.clone(),
            ))
        }
    })
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

    /// `[server.auth]` settings with the mandatory pair present and `extra`
    /// TOML appended, mirroring how a config file builds them.
    fn auth_settings(extra: &str) -> AuthSettings {
        toml::from_str(&format!(
            "jwt_issuer = \"https://auth.example.com\"\njwt_audience = [\"lore\"]\n{extra}"
        ))
        .unwrap()
    }

    const AUTH_URL: Option<&str> = Some("https://legacy-auth.example.com");

    #[test]
    fn selection_follows_the_flowchart() {
        // (auth settings, legacy auth_url) → selected implementation.
        let table = [
            (None, None, AuthorizerSelection::AllowAll),
            (
                Some(auth_settings("")),
                AUTH_URL,
                AuthorizerSelection::AuthClient,
            ),
            (
                Some(auth_settings("resource_claim = \"resources\"")),
                None,
                AuthorizerSelection::ResourceGrants,
            ),
            (
                Some(auth_settings("")),
                None,
                AuthorizerSelection::GlobalGrants,
            ),
            (
                Some(auth_settings("permission_claim = \"realm_access.roles\"")),
                None,
                AuthorizerSelection::GlobalGrants,
            ),
        ];
        for (auth, auth_url, expected) in table {
            assert_eq!(
                select_repository_authorizer(auth.as_ref(), auth_url).unwrap(),
                expected,
                "auth: {auth:?}, auth_url: {auth_url:?}"
            );
            // Construction agrees with selection for every valid row.
            repository_authorizer(auth.as_ref(), auth_url.map(ToString::to_string)).unwrap();
        }
    }

    /// The check that makes a `UrcAuthApi` deployment's move onto the token
    /// claims a deliberate step: setting `resource_claim` while `auth_url`
    /// is still configured refuses to start instead of silently staying on
    /// the auth service.
    #[test]
    fn auth_url_with_resource_claim_refuses_startup_naming_both() {
        let auth = auth_settings("resource_claim = \"resources\"");
        let message = select_repository_authorizer(Some(&auth), AUTH_URL)
            .unwrap_err()
            .to_string();
        assert!(message.contains("auth_url"), "{message}");
        assert!(message.contains("resource_claim"), "{message}");
    }

    /// `auth_url` names an authorization service, but without `[server.auth]`
    /// nothing verifies tokens: the server would run open while the operator
    /// believed they had configured authorization. Refused, naming both.
    #[test]
    fn auth_url_without_server_auth_refuses_startup_naming_both() {
        let message = select_repository_authorizer(None, AUTH_URL)
            .unwrap_err()
            .to_string();
        assert!(message.contains("auth_url"), "{message}");
        assert!(message.contains("[server.auth]"), "{message}");
    }

    /// The startup log prints the `Display` form, so an operator can tell
    /// which implementation they got.
    #[test]
    fn selection_display_names_the_implementation() {
        assert_eq!(
            AuthorizerSelection::AllowAll.to_string(),
            "AllowAllRepositoryAuthorizer"
        );
        assert_eq!(
            AuthorizerSelection::AuthClient.to_string(),
            "AuthClientAuthorizer"
        );
        assert_eq!(
            AuthorizerSelection::GlobalGrants.to_string(),
            "GlobalGrantsAuthorizer"
        );
        assert_eq!(
            AuthorizerSelection::ResourceGrants.to_string(),
            "ResourceGrantsAuthorizer"
        );
    }
}
