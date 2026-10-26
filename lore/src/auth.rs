// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::error::AddressNotFound;
use lore_base::error::Disconnected;
use lore_base::error::Maintenance;
use lore_base::error::NoRemote;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::error::NotSupported;
use lore_base::error::Oversized;
use lore_base::error::RepositoryNotFound;
use lore_base::error::SlowDown;
use lore_base::error::TokenNotFound;
use lore_base::lore_debug;
use lore_base::lore_warn;
use lore_base::runtime::LORE_CONTEXT;
use lore_credential::UserInfo;
use lore_credential::token_store::RemovedRefreshToken;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::auth;
use lore_revision::auth::login::LoginError;
use lore_revision::auth::userinfo::LoreAuthIdentityEventData;
use lore_revision::auth::userinfo::LoreAuthUserInfoEventData;
use lore_revision::auth::userinfo::LoreAuthUserTokenEventData;
use lore_revision::auth::userinfo::UserInfoError;
use lore_revision::event::EventError;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreEvent;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::execution_context;
use lore_revision::repository::RepositoryContext;
use lore_transport::auth::AuthEndpoint;

use crate::call::repository_call_no_store;
use crate::call::repository_call_read;
use crate::call::setup_execution;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreString;

#[error_set]
pub enum AuthStoreError {
    TokenNotFound,
    // Raised when the command needs a repository to resolve an auth endpoint from and
    // was not run in one, which is a different fix for the caller than any of the below.
    RepositoryNotFound,
    // The remaining variants mirror `ProtocolError` so a connect failure can be
    // forwarded whole, preserving its kind instead of collapsing to `Internal`.
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    AddressNotFound,
    NoRemote,
    NotSupported,
    Oversized,
}

impl EventError for AuthStoreError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Arguments for resolving user IDs to display names via the remote user service.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(resolve_user_info_local)]
pub struct LoreAuthUserInfoArgs {
    /// User IDs to resolve; empty resolves the current user locally
    pub user_ids: LoreArray<LoreString>,
}

/// Resolves user IDs to display names using the remote user service.
///
/// Requires an authenticated connection. Queries the remote user service to
/// resolve the provided user IDs to their display names.
///
/// When `user_ids` is empty, falls back to [`local_user_info`] to return the
/// current user's identity without contacting the remote service.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Auth Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::AuthUserInfo`](crate::interface::LoreEvent::AuthUserInfo) | Emitted with user id and display name for each resolved user |
pub async fn resolve_user_info(
    globals: LoreGlobalArgs,
    args: LoreAuthUserInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, resolve_user_info_local).await
}

async fn resolve_user_info_local(
    globals: LoreGlobalArgs,
    args: LoreAuthUserInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    if args.user_ids.is_empty() {
        // No user IDs provided — resolve the current user locally
        let local_args = LoreAuthLocalUserInfoArgs {
            auth_endpoint: LoreString::default(),
            user_ids: LoreArray::default(),
            with_identity_token: 0,
            with_access_token: 0,
        };
        return local_user_info_impl(globals, local_args, callback).await;
    }

    repository_call_read(
        globals,
        callback,
        args,
        resolve_user_info,
        move |repository, args| resolve_user_info_impl(repository, args.user_ids),
    )
    .await
}

async fn resolve_user_info_impl(
    repository: Arc<RepositoryContext>,
    ids: LoreArray<LoreString>,
) -> Result<(), UserInfoError> {
    lore_revision::auth::userinfo::resolve_user_info_boxed(repository, ids).await
}

/// What the repository at a given path says about its remote.
///
/// A repository with no remote URL is distinct from no repository at all: the former is a
/// configured state that network commands answer with `NoRemote`, the latter leaves the
/// auth endpoint genuinely unresolvable.
#[lore_macro::test_pub]
enum RepositoryRemote {
    /// No repository config could be read at this path.
    NoRepository,
    /// A repository is present, but no remote URL is configured for it.
    NoRemote,
    /// A repository is present with a remote URL configured.
    Remote(String),
}

#[lore_macro::test_pub]
fn read_repository_remote(repository_path: &str) -> RepositoryRemote {
    // Presence of the tracking directory is what says a repository is here. A missing
    // config file reads as the default config, so asking the config alone would report
    // every path in the filesystem as a repository that merely has no remote.
    //
    // Resolve that directory the way the config loader does rather than joining `.lore`
    // onto the path: a repository under a VFS keeps its tracking directory outside the
    // working copy, and looking only for a physical one would report it as no repository
    // at all. Loading from the directory already resolved keeps the two from diverging.
    let Ok(dot_dir) =
        lore_revision::repository::get_dot_lore_path(std::path::Path::new(repository_path))
    else {
        return RepositoryRemote::NoRepository;
    };
    if !dot_dir.is_dir() {
        return RepositoryRemote::NoRepository;
    }

    // If this command is invoked in a repository, load the config
    let Ok(repository_config) =
        lore_revision::repository::load_repository_config_from_dot_dir(&dot_dir)
    else {
        return RepositoryRemote::NoRepository;
    };

    match repository_config.remote_url {
        Some(remote_url) if !remote_url.is_empty() => RepositoryRemote::Remote(remote_url),
        _ => RepositoryRemote::NoRemote,
    }
}

/// The remote URL configured for the repository at `repository_path`, if it has one.
///
/// Collapses "no repository" and "repository with no remote" into `None` for the callers
/// that only need a URL to hand to the transport, which reports the absence itself.
fn configured_remote_url(repository_path: &str) -> Option<String> {
    match read_repository_remote(repository_path) {
        RepositoryRemote::Remote(remote_url) => Some(remote_url),
        RepositoryRemote::NoRepository | RepositoryRemote::NoRemote => None,
    }
}

fn send_user_info(user_info: UserInfo) {
    let id = user_info.id;
    let name = if !user_info.preferred_username.is_empty() {
        user_info.preferred_username
    } else if !user_info.name.is_empty() {
        user_info.name
    } else {
        id.clone()
    };

    LoreEvent::AuthUserInfo(LoreAuthUserInfoEventData {
        id: id.into(),
        name: name.into(),
    })
    .send();
}

/// Arguments for authenticating against a remote URL using a provided token.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(login_with_token_local)]
pub struct LoreAuthLoginWithTokenArgs {
    /// Remote URL; empty resolves from the repository config
    pub remote_url: LoreString,
    /// Authentication token
    pub token: LoreString,
    /// Token type
    pub token_type: LoreString,
    /// Auth service URL with scheme (e.g. `ucs-auth://auth.example.com`); used
    /// directly when non-empty, required when no remote URL is available
    pub auth_url: LoreString,
    /// Client ID of a confidential client registered with the auth service. When
    /// non-empty, logs in as that client with `client_secret`, and `token` and
    /// `token_type` must be empty
    pub client_id: LoreString,
    /// Secret of the client `client_id` names
    pub client_secret: LoreString,
}

/// Authenticates against a remote URL using a provided token, or as a registered client
/// with its ID and secret.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Auth Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::AuthUserInfo`](crate::interface::LoreEvent::AuthUserInfo) | Emitted with user id and display name after successful token authentication |
pub async fn login_with_token(
    globals: LoreGlobalArgs,
    args: LoreAuthLoginWithTokenArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, login_with_token_local).await
}

async fn login_with_token_local(
    globals: LoreGlobalArgs,
    args: LoreAuthLoginWithTokenArgs,
    callback: LoreEventCallback,
) -> i32 {
    let remote_url = if !args.remote_url.is_empty() {
        args.remote_url.to_string()
    } else {
        configured_remote_url(globals.repository_path.as_str()).unwrap_or_default()
    };

    let execution = setup_execution(globals, callback);

    let auth_url: Option<String> = args.auth_url.clone().into();

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                if args.client_id.is_empty() {
                    login_with_token_impl(
                        remote_url.as_str(),
                        args.token.as_str(),
                        args.token_type.as_str(),
                        auth_url.as_deref(),
                    )
                    .await
                } else {
                    login_with_client_credentials_impl(
                        remote_url.as_str(),
                        &args,
                        auth_url.as_deref(),
                    )
                    .await
                }
            }
            .await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

async fn login_with_token_impl(
    remote_url: &str,
    token: &str,
    token_type: &str,
    auth_url: Option<&str>,
) -> Result<(), LoginError> {
    match lore_revision::auth::login::with_token_boxed(remote_url, token, token_type, auth_url)
        .await
    {
        Ok(user_info) => {
            send_user_info(user_info);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

async fn login_with_client_credentials_impl(
    remote_url: &str,
    args: &LoreAuthLoginWithTokenArgs,
    auth_url: Option<&str>,
) -> Result<(), LoginError> {
    if !args.token.is_empty() || !args.token_type.is_empty() {
        return Err(LoginError::internal(
            "Login takes either a token or client credentials, not both",
        ));
    }
    let user_info = lore_revision::auth::login::with_client_credentials_boxed(
        remote_url,
        args.client_id.as_str(),
        args.client_secret.as_str(),
        auth_url,
    )
    .await?;
    send_user_info(user_info);
    Ok(())
}

/// Arguments for authenticating interactively via browser-based login flow.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(login_interactive_local)]
pub struct LoreAuthLoginInteractiveArgs {
    /// Remote URL; empty resolves from the repository config
    pub remote_url: LoreString,
    /// Emit the login URL instead of opening a browser
    pub no_browser: u8,
}

/// Authenticates interactively via browser-based login flow for a remote URL.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Auth Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::AuthUrl`](crate::interface::LoreEvent::AuthUrl) | Emitted with the login URL when no_browser mode is requested (instead of opening browser), and whenever the login has a code for the user to enter or confirm at that URL |
/// | [`LoreEvent::AuthPending`](crate::interface::LoreEvent::AuthPending) | Emitted before each wait while the login awaits the user's approval, with the seconds elapsed, the seconds until the next poll, and the seconds left before the session expires |
/// | [`LoreEvent::AuthUserInfo`](crate::interface::LoreEvent::AuthUserInfo) | Emitted with user id and display name after successful interactive authentication |
pub async fn login_interactive(
    globals: LoreGlobalArgs,
    args: LoreAuthLoginInteractiveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, login_interactive_local).await
}

async fn login_interactive_local(
    globals: LoreGlobalArgs,
    args: LoreAuthLoginInteractiveArgs,
    callback: LoreEventCallback,
) -> i32 {
    let remote_url = if !args.remote_url.is_empty() {
        args.remote_url.to_string()
    } else {
        configured_remote_url(globals.repository_path.as_str()).unwrap_or_default()
    };

    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                match auth::login::interactive(remote_url.as_str(), args.no_browser != 0).await {
                    Ok(user_info) => {
                        send_user_info(user_info);
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            }
            .await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for listing all stored authentication identities across endpoints.
#[repr(C)]
#[derive(Debug, Clone, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(list_local)]
pub struct LoreAuthListArgs {
    /// Include the decrypted cached token in each identity
    pub with_token: u8,
}

/// Lists all stored authentication identities across all auth endpoints.
///
/// Each emitted `AuthIdentity` event represents a stored token entry. Entries
/// with an empty `resource` field are authentication tokens (used to prove the
/// user's identity to the auth service). Entries with a `resource` field
/// (e.g. `urc-{repository_id}`) are authorization tokens (granting access to
/// a specific resource).
///
/// When `with_token` is set, the `token` field in each `AuthIdentity` event
/// is populated with the decrypted cached token.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Auth Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::AuthIdentity`](crate::interface::LoreEvent::AuthIdentity) | Emitted once per stored identity with remote, resource, user id, authorized domains, expiry, and optionally the cached token |
pub async fn list(
    globals: LoreGlobalArgs,
    args: LoreAuthListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, list_local).await
}

async fn list_local(
    globals: LoreGlobalArgs,
    args: LoreAuthListArgs,
    callback: LoreEventCallback,
) -> i32 {
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                let identities =
                    lore_credential::token_store::load_all_identities(args.with_token != 0)
                        .await
                        .forward::<AuthStoreError>("accessing token store")?;

                for identity in identities {
                    LoreEvent::AuthIdentity(LoreAuthIdentityEventData {
                        auth_url: identity.auth_url.into(),
                        resource: identity.resource.into(),
                        user_id: identity.user_id.into(),
                        authorized_domains: identity.acceptable_root_domains.join(", ").into(),
                        expires: identity.expires_ms,
                        token: identity.token.into(),
                    })
                    .send();
                }

                Ok::<(), AuthStoreError>(())
            }
            .await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for removing stored authentication and authorization tokens.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(logout_local)]
pub struct LoreAuthLogoutArgs {
    /// Auth service URL; empty resolves from the repository
    pub auth_url: LoreString,
    /// Resource ID (e.g. `urc-{id}`); empty removes all tokens for the auth URL
    pub resource: LoreString,
    /// User identity to remove; empty removes all identities
    pub user_id: LoreString,
}

/// Removes stored authentication and authorization tokens.
///
/// Behavior depends on which arguments are provided:
///
/// - `auth_url` empty: resolved from the current repository's remote environment.
/// - `user_id` empty: removes all identities for the auth URL.
/// - `user_id` set, `resource` empty: removes the user's authentication token
///   and all authorization tokens for the auth URL.
/// - `user_id` set, `resource` set: removes only the specific authorization
///   token for that resource.
///
/// Each login removed with its refresh token is then revoked if the auth service offers
/// revocation. A failed revocation fails the call after the tokens are removed.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn logout(
    globals: LoreGlobalArgs,
    args: LoreAuthLogoutArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, logout_local).await
}

async fn logout_local(
    globals: LoreGlobalArgs,
    args: LoreAuthLogoutArgs,
    callback: LoreEventCallback,
) -> i32 {
    let repository_path = globals.repository_path.to_string();
    let identity = globals.identity().unwrap_or_default().to_string();

    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                let (auth_url, endpoint) =
                    logout_endpoint(args.auth_url.as_str(), &repository_path, &identity).await?;
                let auth_url = auth_url.as_str();

                if execution_context().globals().dry_run() {
                    return Ok(());
                }

                let removed = if args.user_id.is_empty() {
                    lore_credential::token_store::remove_all_tokens_for_auth_url(auth_url)
                        .await
                        .forward::<AuthStoreError>("accessing token store")?
                } else if args.resource.is_empty() {
                    lore_credential::token_store::remove_user_tokens_for_auth_url(
                        auth_url,
                        args.user_id.as_str(),
                    )
                    .await
                    .forward::<AuthStoreError>("accessing token store")?
                } else {
                    let store_key = format!("{}/{}", auth_url, args.resource.as_str());
                    lore_credential::token_store::remove_user_token(
                        &store_key,
                        args.user_id.as_str(),
                    )
                    .await
                    .forward::<AuthStoreError>("accessing token store")?;
                    Vec::new()
                };
                let correlation_id = execution_context().globals().correlation_id.to_string();
                revoke_refresh_tokens(auth_url, endpoint.as_ref(), removed, &correlation_id).await
            }
            .await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for clearing all stored authentication identities and tokens.
#[repr(C)]
#[derive(Debug, Clone, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(clear_local)]
pub struct LoreAuthClearArgs {
    _unused: u8,
}

/// Clears all stored authentication identities and tokens.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
pub async fn clear(
    globals: LoreGlobalArgs,
    args: LoreAuthClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, clear_local).await
}

async fn clear_local(
    globals: LoreGlobalArgs,
    _args: LoreAuthClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                if !execution_context().globals().dry_run() {
                    lore_credential::token_store::reset_tokens()
                        .await
                        .forward::<AuthStoreError>("accessing token store")?;
                }
                Ok::<(), AuthStoreError>(())
            }
            .await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for resolving user identities from locally stored JWT tokens.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(local_user_info_impl)]
pub struct LoreAuthLocalUserInfoArgs {
    /// Auth service remote URL; empty resolves from the repository's remote environment
    pub auth_endpoint: LoreString,
    /// User identities to resolve; empty resolves the current user
    pub user_ids: LoreArray<LoreString>,
    /// Emit cached identity token details for identities with a local token
    pub with_identity_token: u8,
    /// Emit the repository's authorization (access) token. Requires running
    /// inside a repository
    pub with_access_token: u8,
}

/// Resolves user identities to user information using locally stored JWT tokens.
///
/// Does not require a repository context or network access. Decodes locally
/// cached JWT tokens to extract display names. For user IDs without a local
/// token, returns the raw user ID as the display name.
///
/// When `user_ids` is empty, returns the current user's identity. When
/// `auth_endpoint` is empty, resolves it from the repository's remote
/// environment configuration.
///
/// When `with_identity_token` is set, emits `AuthUserToken` events (including
/// the cached identity token string) for identities that have a locally stored
/// token, and `AuthUserInfo` events for others.
///
/// When `with_access_token` is set, the call requires a repository and
/// additionally emits an `AuthIdentity` event carrying the repository-scoped
/// authorization (access) token for the current user, performing a token
/// exchange when no valid cached token exists.
///
/// For remote resolution of user IDs with proper authorization, use
/// [`resolve_user_info`] which queries the remote user service.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Auth Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::AuthUserInfo`](crate::interface::LoreEvent::AuthUserInfo) | Emitted once per resolved identity with user id and display name |
/// | [`LoreEvent::AuthUserToken`](crate::interface::LoreEvent::AuthUserToken) | Emitted instead of `AuthUserInfo` when `with_identity_token` is set and a cached token is available, includes full token details |
/// | [`LoreEvent::AuthIdentity`](crate::interface::LoreEvent::AuthIdentity) | Emitted when `with_access_token` is set, carries the repository-scoped authorization token for the current user |
pub async fn local_user_info(
    globals: LoreGlobalArgs,
    args: LoreAuthLocalUserInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, local_user_info_impl).await
}

#[lore_macro::test_pub]
async fn resolve_auth_endpoint(
    auth_endpoint: &str,
    repository_path: &str,
    identity: &str,
) -> Result<String, AuthStoreError> {
    if !auth_endpoint.is_empty() {
        return Ok(auth_endpoint.to_string());
    }
    Ok(remote_auth_endpoint(repository_path, identity).await?.url)
}

/// The auth URL whose tokens a logout removes, `auth_url` when given, and the endpoint to revoke
/// them at. An explicit OIDC endpoint has none: the scheme registry does not serve it, since its
/// grants need the client the server advertises.
async fn logout_endpoint(
    auth_url: &str,
    repository_path: &str,
    identity: &str,
) -> Result<(String, Option<AuthEndpoint>), AuthStoreError> {
    if auth_url.is_empty() {
        let endpoint = remote_auth_endpoint(repository_path, identity).await?;
        return Ok((endpoint.url.clone(), Some(endpoint)));
    }
    Ok((auth_url.to_string(), AuthEndpoint::resolve(auth_url).ok()))
}

async fn remote_auth_endpoint(
    repository_path: &str,
    identity: &str,
) -> Result<AuthEndpoint, AuthStoreError> {
    // Forward the connect error instead of discarding it, so the real failure
    // reaches the caller rather than collapsing into the generic error below.
    match read_repository_remote(repository_path) {
        RepositoryRemote::Remote(remote_url) => {
            let connection = lore_revision::protocol::connect(
                &remote_url,
                identity,
                lore_revision::lore::RepositoryId::default(),
            )
            .await
            .forward::<AuthStoreError>("resolving auth endpoint from remote")?;

            let endpoint = connection.auth_endpoint();
            if !endpoint.url.is_empty() {
                return Ok(endpoint.clone());
            }
        }
        // The repository exists and simply has no remote. Nothing is unreachable and
        // nothing is unsupported — there is just no remote to ask for an auth endpoint.
        RepositoryRemote::NoRemote => return Err(NoRemote.into()),
        // Without a repository there is nowhere to read a remote from, and the fix is to
        // run this from one (or pass an endpoint) rather than to configure anything.
        RepositoryRemote::NoRepository => {
            return Err(RepositoryNotFound {
                repository: repository_path.to_string(),
            }
            .into());
        }
    }

    Err(NotSupported {
        operation: "authentication requires a configured auth endpoint".to_string(),
    }
    .into())
}

/// Revokes each of `removed` at `endpoint`, which serves `auth_url`. They are removed from the
/// store already, so every one is tried, and the first failure is returned afterwards.
///
/// Only refresh tokens are revoked. They come from the public client's own grants, so the
/// client revoking them is the one they were issued to (RFC 7009 §2.1). An access token may
/// belong to a confidential client whose secret is not stored, and it expires on its own.
#[lore_macro::test_pub]
async fn revoke_refresh_tokens(
    auth_url: &str,
    endpoint: Option<&AuthEndpoint>,
    removed: Vec<RemovedRefreshToken>,
    correlation_id: &str,
) -> Result<(), AuthStoreError> {
    let Some(endpoint) = endpoint else {
        for removed in removed {
            lore_warn!(
                "Removed the login of {} at {auth_url} without revoking it: revoking needs \
                 the OIDC client ID, which only the repository's remote provides",
                removed.user_id
            );
        }
        return Ok(());
    };
    let mut failure = None;
    for removed in removed {
        let revoked = endpoint
            .auth
            .revoke_refresh_token(&endpoint.url, &removed.refresh_token, correlation_id)
            .await;
        match revoked {
            Ok(()) => lore_debug!("Revoked the login of {} at {endpoint}", removed.user_id),
            Err(err) if err.is_not_supported() => lore_debug!(
                "The login of {} is removed from this machine only: {err}",
                removed.user_id
            ),
            Err(err) => {
                lore_warn!(
                    "The login of {} is removed from this machine, but was not revoked at \
                     {endpoint}: {err}",
                    removed.user_id
                );
                failure.get_or_insert(err);
            }
        }
    }
    match failure {
        Some(err) => Err(err).forward("revoking the login at the identity provider"),
        None => Ok(()),
    }
}

async fn local_user_info_impl(
    globals: LoreGlobalArgs,
    args: LoreAuthLocalUserInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    // The access token is scoped to a repository, so that variant runs as a
    // repository call. Plain identity resolution stays repository-free.
    if args.with_access_token != 0 {
        return repository_call_no_store(
            globals,
            callback,
            args,
            local_user_info,
            |repository, args| async move {
                emit_local_user_info(&args).await?;
                auth::userinfo::repository_access_token(repository)
                    .await
                    .forward::<AuthStoreError>("resolving the repository access token")
            },
        )
        .await;
    }

    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let result = emit_local_user_info(&args).await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Resolves the requested identities from locally stored tokens and emits one
/// `AuthUserInfo` or `AuthUserToken` event per identity. Runs inside an
/// execution scope. The caller dispatches completion.
async fn emit_local_user_info(args: &LoreAuthLocalUserInfoArgs) -> Result<(), AuthStoreError> {
    let execution = execution_context();
    let globals = execution.globals();
    let repository_path = globals.repository_path.to_string();
    let identity = globals.identity().unwrap_or_default().to_string();
    let include_identity_token = args.with_identity_token != 0;

    let auth_endpoint =
        resolve_auth_endpoint(args.auth_endpoint.as_str(), &repository_path, &identity).await?;

    let mut user_ids: Vec<String> = args
        .user_ids
        .as_slice()
        .iter()
        .map(|s| s.as_str().to_string())
        .collect();

    // When no user IDs are provided, resolve the current user: the
    // identity this call acts as, which is what a supplied token
    // names. Only without one does the store decide, since a caller
    // working from supplied tokens may have no store at all -- and
    // whichever identity it holds first need not be this caller.
    if user_ids.is_empty() {
        if !identity.is_empty() {
            user_ids.push(identity.clone());
        } else {
            let identities = lore_credential::token_store::load_identities(&auth_endpoint)
                .await
                .forward::<AuthStoreError>("accessing token store")?;
            if let Some(first) = identities.into_iter().next() {
                user_ids.push(first);
            }
        }
    }

    let resolved =
        lore_revision::auth::userinfo::resolve_local_user_info_boxed(&auth_endpoint, &user_ids)
            .await;

    for entry in &resolved {
        if include_identity_token && let Some(user_info) = &entry.local_user_info {
            LoreEvent::AuthUserToken(LoreAuthUserTokenEventData {
                id: user_info.id.clone().into(),
                name: user_info.name.clone().into(),
                token: user_info.token.clone().into(),
                preferred_username: user_info.preferred_username.clone().into(),
                flag_service_account: user_info.is_service_account.into(),
                expires: user_info.expires,
            })
            .send();
            continue;
        }

        LoreEvent::AuthUserInfo(LoreAuthUserInfoEventData {
            id: entry.id.clone().into(),
            name: entry.name.clone().into(),
        })
        .send();
    }

    Ok(())
}
