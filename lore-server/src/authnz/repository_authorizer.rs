// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
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
use crate::auth::jwt::ResourceMatcher;
use crate::grpc::ServerResultExt;
use crate::settings::AuthSettings;

/// The bearer token exactly as it arrived, without the `Bearer ` prefix.
/// The interceptors insert it into request extensions beside the decoded
/// [`AuthorizationToken`] so handlers can rebuild a [`VerifiedToken`].
#[derive(Clone)]
pub struct RawToken(pub String);

/// A token the interceptor has already verified. Claim-reading authorizers
/// use `claims`. [`AuthClientAuthorizer`] forwards `raw` upstream for
/// identity tokens and answers access tokens from their `resources` claim.
pub struct VerifiedToken<'a> {
    pub raw: &'a str,
    pub claims: &'a AuthorizationToken,
}

impl VerifiedToken<'_> {
    pub fn owned(&self) -> VerifiedTokenOwned {
        VerifiedTokenOwned {
            raw: self.raw.to_string(),
            claims: self.claims.clone(),
        }
    }
}

/// Owned form of [`VerifiedToken`], for state that outlives the request or
/// frame that carried the token: a QUIC session, a per-item stream task.
#[derive(Clone)]
pub struct VerifiedTokenOwned {
    pub raw: String,
    pub claims: AuthorizationToken,
}

impl VerifiedTokenOwned {
    pub fn as_token(&self) -> VerifiedToken<'_> {
        VerifiedToken {
            raw: &self.raw,
            claims: &self.claims,
        }
    }
}

/// A caller's enumerated access to one partition.
#[derive(Clone, Debug, PartialEq)]
pub enum Grants {
    /// The partition is not reachable: every action denied.
    Denied,
    /// Reachable, permitted exactly these actions.
    Actions(HashSet<String>),
    /// Reachable, permitted every action.
    All,
}

impl Grants {
    pub fn reachable(&self) -> bool {
        !matches!(self, Grants::Denied)
    }

    pub fn permits(&self, action: &str) -> bool {
        match self {
            Grants::Denied => false,
            Grants::All => true,
            Grants::Actions(actions) => actions.contains(action),
        }
    }
}

/// The partition-access layer's enumerated answer for the partition the
/// request named in its metadata, inserted as a request extension so a
/// handler can make action checks without asking the authorizer again.
/// Carries the partition it answers for, so a handler acting on a different
/// one — a cross-partition source, say — cannot consume it by mistake.
#[derive(Clone)]
pub struct PartitionGrants {
    pub repository_id: RepositoryId,
    pub grants: Grants,
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

    /// The caller's enumerated access to `repository_id`, when this
    /// authorizer can enumerate it. `Ok(None)` means enumeration is
    /// not possible: an authorizer backed by a policy engine can answer
    /// "may X do A?" without being able to list everything X may do.
    /// Callers fall back to
    /// [`check_repository_access`](Self::check_repository_access) per
    /// question on `Ok(None)`.
    async fn granted_actions(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        Ok(None)
    }

    /// [`check_repository_access`](Self::check_repository_access) for call
    /// sites that cannot await: the cross-partition link-read closure runs
    /// inside revision-graph traversal, potentially many times per request.
    /// `None` means the answer needs I/O this authorizer cannot do here.
    /// Such callers must deny, and the online paths keep today's behaviour
    /// because they never granted a link read without an in-token claim.
    ///
    /// The token-based authorizers always answer: the verdict is in a token
    /// the interceptor already verified, so no cache, preload or staleness
    /// bound is needed on those paths.
    fn check_repository_access_sync(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        _action: Option<&str>,
    ) -> Option<Result<(), Status>> {
        None
    }
}

impl dyn RepositoryAuthorizer {
    /// Reachability of `repository_id`, with the caller's enumerated grants
    /// when this authorizer can enumerate them: `Ok(Some(grants))` also
    /// answers later action checks in memory, `Ok(None)` means reachable but
    /// per-action checks must ask the authorizer. Denials are flattened to
    /// [`no_repository_access_status`] so an unauthorized caller learns
    /// nothing from the reason.
    ///
    /// The one call every partition-scoped entry point makes — the gRPC
    /// partition-access layer, the QUIC session start / connect, and the
    /// HTTP middleware — each exposing the grants to its handlers in its own
    /// carrier ([`PartitionGrants`] extension, session entry, connection
    /// context).
    pub async fn granted_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        match self.granted_actions(token, repository_id).await {
            Ok(Some(grants)) if grants.reachable() => Ok(Some(grants)),
            Ok(None) => self
                .check_repository_access(token, repository_id, None)
                .await
                .map(|()| None)
                .map_err(|_denied| crate::grpc::no_repository_access_status()),
            _ => Err(crate::grpc::no_repository_access_status()),
        }
    }

    /// Whether the caller may perform `action` on `repository_id` — the one
    /// call a handler makes for a fine-grained permission check.
    ///
    /// Answered from the [`PartitionGrants`] the partition-access layer
    /// enumerated, when the request carries them for this partition. Asked
    /// of the authorizer otherwise, which is the fallback for authorizers
    /// that can only answer per-action policy questions (and for call sites
    /// that are not behind the Tower middleware).
    pub async fn permits(
        &self,
        extensions: &tonic::Extensions,
        repository_id: RepositoryId,
        action: &str,
    ) -> bool {
        if let Some(grants) = extensions
            .get::<PartitionGrants>()
            .filter(|grants| grants.repository_id == repository_id)
        {
            return grants.grants.permits(action);
        }
        let Some(token) = crate::grpc::get_verified_token(extensions) else {
            return false;
        };
        self.check_repository_access(Some(&token), repository_id, Some(action))
            .await
            .is_ok()
    }
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

    async fn granted_actions(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        Ok(Some(Grants::All))
    }

    fn check_repository_access_sync(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        _action: Option<&str>,
    ) -> Option<Result<(), Status>> {
        Some(Ok(()))
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

    /// Takes the `authorization` header value verbatim.
    async fn check_access_with_header(
        &self,
        authorization: Option<String>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Result<(), Status> {
        let resource_id = format!("urc-{repository_id}");
        let permissions = self.fetch_permissions(authorization, &resource_id).await?;
        evaluate_check_user_permission(&permissions, &resource_id, action)
    }

    /// One `CheckUserPermission` round trip for `resource_id`.
    async fn fetch_permissions(
        &self,
        authorization: Option<String>,
        resource_id: &str,
    ) -> Result<CheckUserPermissionResponse, Status> {
        let mut client = grpc_get_auth_client(self.auth_url.clone()).await?;
        let request = check_user_permission_request(resource_id.to_string(), authorization)?;

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

        Ok(permissions.into_inner())
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

pub(super) fn bearer_header(token: Option<&VerifiedToken<'_>>) -> Option<String> {
    token.map(|token| format!("Bearer {}", token.raw))
}

/// The grants an exchanged access token's `resources` claim holds on
/// `repository_id`, in the legacy `urc-{id}` / `urc-*` shape the auth
/// service mints: unreachable when no entry names the partition, otherwise
/// the permissions merged across every matching entry.
fn grants_from_resources_claim(
    resources: &[crate::auth::jwt::ResourcePermission],
    repository_id: RepositoryId,
) -> Grants {
    let matcher = ResourceMatcher::default();
    if !matcher.any_match(resources, repository_id) {
        return Grants::Denied;
    }
    Grants::Actions(
        matcher
            .merged_permissions(resources, repository_id)
            .into_iter()
            .collect(),
    )
}

/// Grants from a `CheckUserPermission` response: unreachable when no
/// allowed entry names the resource, otherwise the permissions merged across
/// every entry for the resource.
fn grants_from_response(response: &CheckUserPermissionResponse, resource_id: &str) -> Grants {
    let matching: Vec<_> = response
        .allowed_resource_permission
        .iter()
        .filter(|entry| entry.resource_id == resource_id)
        .collect();
    if matching.is_empty() {
        return Grants::Denied;
    }
    Grants::Actions(
        matching
            .iter()
            .flat_map(|entry| entry.permission.iter().cloned())
            .collect(),
    )
}

/// Answer an access question from the `resources` claim of an exchanged
/// access token. `action: None` asks whether any entry names the partition.
/// `action: Some` asks whether a matching entry grants the action. Answered
/// in place rather than through [`grants_from_resources_claim`]: the
/// link-read closure asks this per link, and the merged permission set is
/// only worth building for an enumeration.
fn evaluate_resources_claim(
    resources: &[crate::auth::jwt::ResourcePermission],
    repository_id: RepositoryId,
    action: Option<&str>,
) -> Result<(), Status> {
    let matcher = ResourceMatcher::default();
    let permitted = match action {
        None => matcher.any_match(resources, repository_id),
        Some(action) => matcher.permits(resources, repository_id, action),
    };
    if permitted {
        Ok(())
    } else {
        Err(Status::permission_denied("Not permitted for resource"))
    }
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
        // An exchanged access token carries the auth service's own signed
        // answer for the partition in its `resources` claim, and the auth
        // service refuses access tokens as `CheckUserPermission` credentials
        // (`Unauthenticated: INVALID_FORMAT, field=authorization`), so the
        // claim is evaluated in place. Identity tokens carry no `resources`
        // claim and are checked online — the identity-token paths are where
        // revocation is observable. An access token's grants hold for its
        // lifetime, on this path as on QUIC.
        if let Some(verdict) = self.check_repository_access_sync(token, repository_id, action) {
            return verdict;
        }
        self.check_access_with_header(bearer_header(token), repository_id, action)
            .await
    }

    /// An access token is answered from its `resources` claim, exactly as
    /// the async path does. An identity token needs `CheckUserPermission`,
    /// so `None`.
    fn check_repository_access_sync(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Option<&str>,
    ) -> Option<Result<(), Status>> {
        let resources = token.and_then(|token| token.claims.resources.as_deref())?;
        Some(evaluate_resources_claim(resources, repository_id, action))
    }

    async fn granted_actions(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        let Some(token) = token else {
            // Nothing to enumerate for. The per-question path answers this
            // the same way it always has.
            return Ok(None);
        };
        if let Some(resources) = token.claims.resources.as_deref() {
            return Ok(Some(grants_from_resources_claim(resources, repository_id)));
        }
        // An identity token: the one CheckUserPermission response carries the
        // full permission list.
        let resource_id = format!("urc-{repository_id}");
        let permissions = self
            .fetch_permissions(bearer_header(Some(token)), &resource_id)
            .await?;
        Ok(Some(grants_from_response(&permissions, &resource_id)))
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

    mod resources_claim {
        use std::str::FromStr;

        use lore_base::types::Context;

        use super::*;

        fn repository() -> RepositoryId {
            Context::from_str("0194b726b34e72b0b45550b88a967076")
                .unwrap()
                .into()
        }

        fn access_token_claims(
            resource_id: &str,
            permissions: &[&str],
        ) -> crate::auth::jwt::AuthorizationToken {
            AuthorizationToken {
                resources: Some(vec![crate::auth::jwt::ResourcePermission {
                    resource_id: resource_id.to_string(),
                    permission: permissions.iter().map(ToString::to_string).collect(),
                }]),
                ..Default::default()
            }
        }

        async fn check(claims: &AuthorizationToken, action: Option<&str>) -> Result<(), Status> {
            // A URL nothing listens on: reaching for the network here would
            // hang or error, so a verdict proves the claim answered in place.
            AuthClientAuthorizer::new("https://auth.invalid".to_string())
                .check_repository_access(
                    Some(&VerifiedToken {
                        raw: "raw.jwt",
                        claims,
                    }),
                    repository(),
                    action,
                )
                .await
        }

        /// The auth service refuses exchanged access tokens as
        /// `CheckUserPermission` credentials, so a token carrying a
        /// `resources` claim must be answered from the claim, never sent
        /// upstream.
        #[tokio::test]
        async fn an_access_token_is_answered_from_its_claim() {
            let granted = access_token_claims(
                &format!("urc-{}", repository()),
                &["read", "write", "migrate"],
            );
            check(&granted, None).await.unwrap();
            check(&granted, Some("migrate")).await.unwrap();
            check(&granted, Some("obliterate")).await.unwrap_err();

            let other_partition = access_token_claims("urc-somewhere-else", &["read"]);
            check(&other_partition, None).await.unwrap_err();

            // A wildcard grant reaches every partition, as everywhere else.
            let wildcard = access_token_claims("urc-*", &["read"]);
            check(&wildcard, None).await.unwrap();
        }

        /// A `resources` claim scoped to no partition denies everything —
        /// present-but-empty is a verdict, not a fallback to the network.
        #[tokio::test]
        async fn an_empty_resources_claim_denies() {
            let empty = AuthorizationToken {
                resources: Some(vec![]),
                ..Default::default()
            };
            check(&empty, None).await.unwrap_err();
            check(&empty, Some("read")).await.unwrap_err();
        }

        /// The enumeration and the per-question path answer from the same
        /// derivation, so their verdicts must agree for every access token.
        #[tokio::test]
        async fn enumeration_agrees_with_the_per_question_path() {
            let authorizer = AuthClientAuthorizer::new("https://auth.invalid".to_string());
            let tokens = [
                access_token_claims(&format!("urc-{}", repository()), &["read", "migrate"]),
                access_token_claims("urc-somewhere-else", &["read"]),
                access_token_claims("urc-*", &[]),
                AuthorizationToken {
                    resources: Some(vec![]),
                    ..Default::default()
                },
            ];
            for claims in &tokens {
                let token = VerifiedToken {
                    raw: "raw.jwt",
                    claims,
                };
                let grants = authorizer
                    .granted_actions(Some(&token), repository())
                    .await
                    .unwrap()
                    .expect("access tokens are enumerable");
                assert_eq!(
                    grants.reachable(),
                    check(claims, None).await.is_ok(),
                    "reachability must agree for {claims:?}"
                );
                for action in ["read", "migrate", "obliterate"] {
                    assert_eq!(
                        grants.permits(action),
                        check(claims, Some(action)).await.is_ok(),
                        "verdict for {action} must agree for {claims:?}"
                    );
                }
            }
        }
    }

    mod sync_check {
        use std::str::FromStr;

        use lore_base::types::Context;

        use super::*;

        fn repository() -> RepositoryId {
            Context::from_str("0194b726b34e72b0b45550b88a967076")
                .unwrap()
                .into()
        }

        /// Implements only the required method, so the sync check is the
        /// trait's default.
        struct PolicyOnly;

        #[async_trait]
        impl RepositoryAuthorizer for PolicyOnly {
            async fn check_repository_access(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
                _action: Option<&str>,
            ) -> Result<(), Status> {
                Ok(())
            }
        }

        #[test]
        fn default_cannot_answer_without_io() {
            assert!(
                PolicyOnly
                    .check_repository_access_sync(None, repository(), None)
                    .is_none()
            );
        }

        #[test]
        fn allow_all_answers_everything() {
            let claims = AuthorizationToken::default();
            let token = VerifiedToken {
                raw: "raw",
                claims: &claims,
            };
            for token in [None, Some(&token)] {
                for action in [None, Some("obliterate")] {
                    AllowAllRepositoryAuthorizer
                        .check_repository_access_sync(token, repository(), action)
                        .expect("allow-all needs no I/O")
                        .unwrap();
                }
            }
        }

        /// An access token's `resources` claim is the auth service's own
        /// signed answer, so the legacy authorizer answers it in place and
        /// agrees with its async path. An identity token needs the network.
        #[tokio::test]
        async fn auth_client_answers_access_tokens_and_declines_identity_tokens() {
            let authorizer = AuthClientAuthorizer::new("https://auth.invalid".to_string());
            let granted = AuthorizationToken {
                resources: Some(vec![crate::auth::jwt::ResourcePermission {
                    resource_id: format!("urc-{}", repository()),
                    permission: vec!["migrate".to_string()],
                }]),
                ..Default::default()
            };
            let elsewhere = AuthorizationToken {
                resources: Some(vec![crate::auth::jwt::ResourcePermission {
                    resource_id: "urc-somewhere-else".to_string(),
                    permission: vec![],
                }]),
                ..Default::default()
            };
            for claims in [&granted, &elsewhere] {
                let token = VerifiedToken {
                    raw: "raw.jwt",
                    claims,
                };
                for action in [None, Some("migrate"), Some("obliterate")] {
                    let sync = authorizer
                        .check_repository_access_sync(Some(&token), repository(), action)
                        .expect("access tokens are answered in place");
                    let asynchronous = authorizer
                        .check_repository_access(Some(&token), repository(), action)
                        .await;
                    assert_eq!(
                        sync.is_ok(),
                        asynchronous.is_ok(),
                        "{action:?} on {claims:?}"
                    );
                }
            }

            let identity = AuthorizationToken::default();
            let token = VerifiedToken {
                raw: "raw.jwt",
                claims: &identity,
            };
            assert!(
                authorizer
                    .check_repository_access_sync(Some(&token), repository(), None)
                    .is_none()
            );
            assert!(
                authorizer
                    .check_repository_access_sync(None, repository(), None)
                    .is_none()
            );
        }
    }

    mod granted_access {
        use std::str::FromStr;

        use lore_base::types::Context;

        use super::*;
        use crate::grpc::no_repository_access_status;

        fn repository() -> RepositoryId {
            Context::from_str("0194b726b34e72b0b45550b88a967076")
                .unwrap()
                .into()
        }

        /// Non-enumerable: `granted_actions` keeps its `Ok(None)` default and
        /// only the per-question path answers.
        struct PolicyOnly(bool);

        #[async_trait]
        impl RepositoryAuthorizer for PolicyOnly {
            async fn check_repository_access(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
                action: Option<&str>,
            ) -> Result<(), Status> {
                assert_eq!(action, None, "granted_access asks reachability only");
                if self.0 {
                    Ok(())
                } else {
                    Err(Status::permission_denied("the policy's own reason"))
                }
            }
        }

        #[tokio::test]
        async fn enumerable_authorizer_yields_its_grants() {
            let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
            let grants = authorizer.granted_access(None, repository()).await.unwrap();
            assert_eq!(grants, Some(Grants::All));
        }

        #[tokio::test]
        async fn non_enumerable_authorizer_answers_reachability_with_no_grants() {
            let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(PolicyOnly(true));
            let grants = authorizer.granted_access(None, repository()).await.unwrap();
            assert_eq!(grants, None);
        }

        /// Denials flatten to the uniform status whichever path produced
        /// them, so an unauthorized caller learns nothing from the reason.
        #[tokio::test]
        async fn denials_flatten_to_the_uniform_status() {
            let non_enumerable: Arc<dyn RepositoryAuthorizer> = Arc::new(PolicyOnly(false));
            let err = non_enumerable
                .granted_access(None, repository())
                .await
                .unwrap_err();
            assert_eq!(err.code(), no_repository_access_status().code());
            assert_eq!(err.message(), no_repository_access_status().message());

            let claims = AuthorizationToken {
                resources: Some(vec![crate::auth::jwt::ResourcePermission {
                    resource_id: "urc-somewhere-else".to_string(),
                    permission: vec![],
                }]),
                ..Default::default()
            };
            let token = VerifiedToken {
                raw: "raw.jwt",
                claims: &claims,
            };
            let enumerable: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
                "https://auth.invalid".to_string(),
            ));
            let err = enumerable
                .granted_access(Some(&token), repository())
                .await
                .unwrap_err();
            assert_eq!(err.message(), no_repository_access_status().message());
        }
    }

    mod permits_helper {
        use std::str::FromStr;

        use lore_base::types::Context;

        use super::*;
        use crate::authnz::repository_authorizer::RawToken;

        fn repository() -> RepositoryId {
            Context::from_str("0194b726b34e72b0b45550b88a967076")
                .unwrap()
                .into()
        }

        fn extensions_with(grants: Option<PartitionGrants>, token: bool) -> tonic::Extensions {
            let mut extensions = tonic::Extensions::new();
            if let Some(grants) = grants {
                extensions.insert(grants);
            }
            if token {
                extensions.insert(AuthorizationToken::default());
                extensions.insert(RawToken("raw.jwt".into()));
            }
            extensions
        }

        /// The layer's enumerated grants answer first — proven by pairing an
        /// allow-all authorizer with grants that deny: the denial wins.
        #[tokio::test]
        async fn enumerated_grants_answer_before_the_authorizer() {
            let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
            let extensions = extensions_with(
                Some(PartitionGrants {
                    repository_id: repository(),
                    grants: Grants::Actions(HashSet::new()),
                }),
                true,
            );
            assert!(
                !authorizer
                    .permits(&extensions, repository(), "migrate")
                    .await
            );
        }

        /// Grants naming another partition are not consumed; the check falls
        /// back to the authorizer with the verified token.
        #[tokio::test]
        async fn grants_for_another_partition_fall_back_to_the_authorizer() {
            let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
            let other: RepositoryId = Context::from_str("f6ca55437aa34198ba0f0fdc33154d51")
                .unwrap()
                .into();
            let extensions = extensions_with(
                Some(PartitionGrants {
                    repository_id: other,
                    grants: Grants::Denied,
                }),
                true,
            );
            assert!(
                authorizer
                    .permits(&extensions, repository(), "migrate")
                    .await
            );
        }

        /// Without a verified token nothing is granted, whatever the
        /// authorizer would say.
        #[tokio::test]
        async fn no_token_and_no_grants_deny_even_under_allow_all() {
            let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
            let extensions = extensions_with(None, false);
            assert!(
                !authorizer
                    .permits(&extensions, repository(), "migrate")
                    .await
            );
        }
    }

    mod grants {
        use super::*;

        #[test]
        fn permits_and_reachable_follow_the_variant() {
            assert!(Grants::All.reachable());
            assert!(Grants::All.permits("anything"));

            assert!(!Grants::Denied.reachable());
            assert!(!Grants::Denied.permits("anything"));

            let actions = Grants::Actions(["migrate".to_string()].into());
            assert!(actions.reachable());
            assert!(actions.permits("migrate"));
            assert!(!actions.permits("obliterate"));

            // Reachable with nothing granted: a matched entry with an empty
            // permission list, or Tier 1 without a permission claim.
            let none = Grants::Actions(HashSet::new());
            assert!(none.reachable());
            assert!(!none.permits("read"));
        }

        /// The response-derived grants merge every entry naming the
        /// resource and ignore the rest.
        #[test]
        fn response_grants_merge_matching_entries() {
            let response = response(vec![
                entry("urc-abc", &["read"]),
                entry("urc-other", &["obliterate"]),
                entry("urc-abc", &["migrate"]),
            ]);
            let grants = grants_from_response(&response, "urc-abc");
            assert!(grants.reachable());
            assert!(grants.permits("read"));
            assert!(grants.permits("migrate"));
            assert!(!grants.permits("obliterate"));

            assert_eq!(
                grants_from_response(&response, "urc-absent"),
                Grants::Denied
            );
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
