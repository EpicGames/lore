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

/// An action a caller may be asked to hold on a partition. Grants name
/// actions by [`Action::as_str`], the form tokens and the auth service use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Implied by any grant on a partition: a caller who reaches it may read it.
    Read,
    Write,
    Owner,
    Admin,
    Migrate,
    PushProtected,
    Obliterate,
    Presign,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Migrate => "migrate",
            Self::PushProtected => "push-protected",
            Self::Obliterate => "obliterate",
            Self::Presign => "presign",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub const ALLOW_ALL_ACTIONS: [Action; 4] = [
    Action::Read,
    Action::Write,
    Action::Obliterate,
    Action::Presign,
];

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
}

impl Grants {
    pub fn reachable(&self) -> bool {
        !matches!(self, Grants::Denied)
    }

    pub fn permits(&self, action: Action) -> bool {
        match self {
            Grants::Denied => false,
            Grants::Actions(actions) => action == Action::Read || actions.contains(action.as_str()),
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
    /// Whether `token` may perform `action` on `repository_id`: [`Action::Read`],
    /// [`Action::Write`], or a privileged action. `token` is `None` when no verifier
    /// is configured. An implementation may answer different actions
    /// differently, for example from the token for reads and with an online
    /// call for [`Action::Obliterate`].
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status>;

    /// Whether `token` may look `repository_id` up and read its record: the id, the name
    /// and the metadata hash, not the metadata or the contents. [`Action::Read`] by default.
    async fn check_repository_lookup(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
    ) -> Result<(), Status> {
        self.check_repository_access(token, repository_id, Action::Read)
            .await
    }

    /// The caller's enumerated access to `repository_id`, when this
    /// authorizer can enumerate it. `Ok(None)` means enumeration is
    /// not possible: an authorizer backed by a policy engine can answer
    /// "may X do A?" without being able to list everything X may do.
    /// Callers fall back to
    /// [`check_repository_access`](Self::check_repository_access) per
    /// question on `Ok(None)`. Those questions come from handlers, some of
    /// which run under no timeout, so an implementation that returns
    /// `Ok(None)` must bound its own calls.
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
        _action: Action,
    ) -> Option<Result<(), Status>> {
        None
    }
}

impl dyn RepositoryAuthorizer {
    /// Reachability of `repository_id`, with the caller's enumerated grants
    /// when this authorizer can enumerate them: `Ok(Some(grants))` also
    /// answers later action checks in memory. `Ok(None)` means reachable,
    /// answered by asking [`Action::Read`] of an authorizer that cannot enumerate;
    /// later action checks then ask it too. Denials are flattened to
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
                .check_repository_access(token, repository_id, Action::Read)
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
    /// that are not behind the Tower middleware). A request with no verified
    /// token is asked too: the authorizer decides what such a caller holds.
    pub async fn permits(
        &self,
        extensions: &tonic::Extensions,
        repository_id: RepositoryId,
        action: Action,
    ) -> bool {
        let grants = extensions
            .get::<PartitionGrants>()
            .filter(|grants| grants.repository_id == repository_id);
        let token = crate::grpc::get_verified_token(extensions);
        permits_with(
            self,
            grants.map(|grants| &grants.grants),
            token.as_ref(),
            repository_id,
            action,
        )
        .await
    }
}

/// Whether `action` on `repository_id` is permitted. `grants`, the caller's
/// enumerated grants on it, answer when present; otherwise `authorizer` is
/// asked with `token`, which is `None` when no verifier is configured.
pub async fn permits_with(
    authorizer: &dyn RepositoryAuthorizer,
    grants: Option<&Grants>,
    token: Option<&VerifiedToken<'_>>,
    repository_id: RepositoryId,
    action: Action,
) -> bool {
    if let Some(grants) = grants {
        return grants.permits(action);
    }
    authorizer
        .check_repository_access(token, repository_id, action)
        .await
        .is_ok()
}

/// Selected when no `[server.auth]` is configured: nothing verifies tokens,
/// so every caller, with or without a token, holds [`ALLOW_ALL_ACTIONS`] on
/// every repository.
pub struct AllowAllRepositoryAuthorizer;

impl AllowAllRepositoryAuthorizer {
    fn check(action: Action) -> Result<(), Status> {
        if ALLOW_ALL_ACTIONS.contains(&action) {
            Ok(())
        } else {
            Err(Status::permission_denied("Action not permitted"))
        }
    }
}

#[async_trait]
impl RepositoryAuthorizer for AllowAllRepositoryAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        Self::check(action)
    }

    async fn granted_actions(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        Ok(Some(Grants::Actions(
            ALLOW_ALL_ACTIONS.iter().map(ToString::to_string).collect(),
        )))
    }

    fn check_repository_access_sync(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        _repository_id: RepositoryId,
        action: Action,
    ) -> Option<Result<(), Status>> {
        Some(Self::check(action))
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
        action: Action,
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

#[lore_macro::test_pub]
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

#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
/// access token: whether a matching entry grants `action`. Answered in
/// place rather than through [`grants_from_resources_claim`]: the link-read
/// closure asks this per link, and the merged permission set is only worth
/// building for an enumeration.
fn evaluate_resources_claim(
    resources: &[crate::auth::jwt::ResourcePermission],
    repository_id: RepositoryId,
    action: Action,
) -> Result<(), Status> {
    if ResourceMatcher::default().permits(resources, repository_id, action) {
        Ok(())
    } else {
        Err(Status::permission_denied("Not permitted for resource"))
    }
}

/// Answer an access question from a `CheckUserPermission` response: whether
/// an allowed entry for the resource grants `action`.
#[lore_macro::test_pub]
fn evaluate_check_user_permission(
    response: &CheckUserPermissionResponse,
    resource_id: &str,
    action: Action,
) -> Result<(), Status> {
    let permitted = response
        .allowed_resource_permission
        .iter()
        .filter(|entry| entry.resource_id == resource_id)
        .any(|entry| {
            action == Action::Read
                || entry
                    .permission
                    .iter()
                    .any(|granted| granted == action.as_str())
        });
    if permitted {
        Ok(())
    } else {
        Err(Status::permission_denied("Not permitted for resource"))
    }
}

#[async_trait]
impl RepositoryAuthorizer for AuthClientAuthorizer {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        if token.is_none() {
            return Err(Status::unauthenticated("No token"));
        }
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
        action: Action,
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
    if auth.implicit_actions.is_some() && (auth_url.is_some() || auth.resource_claim.is_some()) {
        bail!(
            "[server.auth] implicit_actions is set together with [environment.endpoint] auth_url \
             or [server.auth] resource_claim: implicit_actions applies to Tier 1 only, where \
             neither is set, and does nothing otherwise. Remove implicit_actions."
        );
    }
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
            Arc::new(GlobalGrantsAuthorizer::new(
                auth.permission_claim.clone(),
                auth.implicit_actions(),
            ))
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
