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
use lore_base::runtime::LORE_CONTEXT;
use lore_credential::UserInfo;
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
use serde::Deserialize;
use serde::Serialize;

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

/// Arguments for resolving user IDs to display names via the remote auth service.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(resolve_user_info_local)]
pub struct LoreAuthUserInfoArgs {
    /// User IDs to resolve; empty resolves the current user locally
    pub user_ids: LoreArray<LoreString>,
}

/// Resolves user IDs to display names using the remote authentication service.
///
/// Requires an authenticated connection. Queries the authentication service to
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
    lore_revision::auth::userinfo::resolve_user_info(repository, ids).await
}

/// What the repository at a given path says about its remote.
///
/// A repository with no remote URL is distinct from no repository at all: the former is a
/// configured state that network commands answer with `NoRemote`, the latter leaves the
/// auth endpoint genuinely unresolvable.
enum RepositoryRemote {
    /// No repository config could be read at this path.
    NoRepository,
    /// A repository is present, but no remote URL is configured for it.
    NoRemote,
    /// A repository is present with a remote URL configured.
    Remote(String),
}

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
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
}

/// Authenticates against a remote URL using a provided token.
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

    let auth_url: Option<String> = args.auth_url.into();

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                login_with_token_impl(
                    remote_url.as_str(),
                    args.token.as_str(),
                    args.token_type.as_str(),
                    auth_url.as_deref(),
                )
                .await
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
    match lore_revision::auth::login::with_token(remote_url, token, token_type, auth_url).await {
        Ok(user_info) => {
            send_user_info(user_info);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Arguments for authenticating interactively via browser-based login flow.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
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
/// | [`LoreEvent::AuthUrl`](crate::interface::LoreEvent::AuthUrl) | Emitted with the login URL when no_browser mode is requested (instead of opening browser) |
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, LoreArgs)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
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
                let auth_url =
                    resolve_auth_endpoint(args.auth_url.as_str(), &repository_path, &identity)
                        .await?;

                if args.user_id.is_empty() {
                    lore_credential::token_store::remove_all_tokens_for_auth_url(&auth_url)
                        .await
                        .forward::<AuthStoreError>("accessing token store")?;
                } else if args.resource.is_empty() {
                    lore_credential::token_store::remove_user_tokens_for_auth_url(
                        &auth_url,
                        args.user_id.as_str(),
                    )
                    .await
                    .forward::<AuthStoreError>("accessing token store")?;
                } else {
                    let store_key = format!("{}/{}", auth_url, args.resource.as_str());
                    lore_credential::token_store::remove_user_token(
                        &store_key,
                        args.user_id.as_str(),
                    )
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

/// Arguments for clearing all stored authentication identities and tokens.
#[repr(C)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, LoreArgs)]
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
                lore_credential::token_store::reset_tokens()
                    .await
                    .forward::<AuthStoreError>("accessing token store")?;
                Ok::<(), AuthStoreError>(())
            }
            .await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

/// Arguments for resolving user identities from locally stored JWT tokens.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(local_user_info_impl)]
pub struct LoreAuthLocalUserInfoArgs {
    /// Auth service remote URL; empty resolves from the repository's remote environment
    pub auth_endpoint: LoreString,
    /// User identities to resolve; empty resolves the current user
    pub user_ids: LoreArray<LoreString>,
    /// Emit cached identity token details for identities with a local token
    #[serde(alias = "with_token")]
    pub with_identity_token: u8,
    /// Emit the repository's authorization (access) token. Requires running
    /// inside a repository
    #[serde(default)]
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
/// [`resolve_user_info`] which queries the remote authentication service.
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

async fn resolve_auth_endpoint(
    auth_endpoint: &str,
    repository_path: &str,
    identity: &str,
) -> Result<String, AuthStoreError> {
    if !auth_endpoint.is_empty() {
        return Ok(auth_endpoint.to_string());
    }

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

            let auth_url = connection.auth_url().to_string();
            if !auth_url.is_empty() {
                return Ok(auth_url);
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
        lore_revision::auth::userinfo::resolve_local_user_info(&auth_endpoint, &user_ids).await;

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

#[cfg(test)]
mod resolve_auth_endpoint_tests {
    use lore_base::error::NoRemote;
    use lore_base::error::RepositoryNotFound;
    use lore_error_set::FfiError;

    use super::RepositoryRemote;
    use super::read_repository_remote;
    use super::resolve_auth_endpoint;

    /// A repository directory with the given `config.toml` body, or none at all when
    /// `config` is `None`. Returns the repository root.
    fn repository_with_config(label: &str, config: Option<&str>) -> lore_base::test_util::TempDir {
        let root = lore_base::test_util::TempDir::new(&format!("lore-auth-{label}-"));
        let dot_dir = root.join(lore_revision::repository::DOT_LORE);
        std::fs::create_dir_all(&dot_dir).expect("creating the repository directory");
        if let Some(config) = config {
            std::fs::write(dot_dir.join("config.toml"), config).expect("writing the config");
        }
        root
    }

    // An explicit endpoint is returned verbatim without touching the
    // repository config or the network.
    #[tokio::test]
    async fn returns_explicit_endpoint_verbatim() {
        let endpoint = resolve_auth_endpoint("ucs-auth://auth.example.com", "/does/not/exist", "")
            .await
            .expect("an explicit endpoint should resolve");
        assert_eq!(endpoint, "ucs-auth://auth.example.com");
    }

    // Run outside a repository with no explicit endpoint, the answer names the actual
    // problem — there is no repository here to read a remote from — rather than the
    // generic "requires a configured auth endpoint", which reads as though something
    // needs configuring when the fix is to run this from a repository or pass an endpoint.
    #[tokio::test]
    async fn missing_repository_is_repository_not_found() {
        let err = resolve_auth_endpoint("", "/does/not/exist", "")
            .await
            .expect_err("a missing endpoint must be an error");

        let repository_not_found_code = RepositoryNotFound {
            repository: String::new(),
        }
        .ffi_code();
        assert_eq!(err.ffi_code(), repository_not_found_code, "{err:?}");
    }

    // A repository created without a URL has a remote-less config. Asking it for an auth
    // endpoint is `NoRemote`, not `NotSupported` and not a connection failure:
    // there is no remote to reach, as opposed to one that could not be reached.
    #[tokio::test]
    async fn repository_without_a_remote_is_no_remote() {
        let root = repository_with_config("empty-remote", Some("remote_url = \"\"\n"));

        let err = resolve_auth_endpoint("", &root.display().to_string(), "")
            .await
            .expect_err("a repository with no remote must be an error");

        assert_eq!(err.ffi_code(), NoRemote.ffi_code(), "{err:?}");
    }

    // Same answer when the config omits the key outright rather than writing it empty,
    // which is what an older or hand-edited config looks like.
    #[tokio::test]
    async fn repository_with_no_remote_key_is_no_remote() {
        let root = repository_with_config("absent-remote", Some("identity = \"me\"\n"));

        let err = resolve_auth_endpoint("", &root.display().to_string(), "")
            .await
            .expect_err("a repository with no remote must be an error");

        assert_eq!(err.ffi_code(), NoRemote.ffi_code(), "{err:?}");
    }

    // A missing config file parses as the default config, so repository presence has to
    // come from the tracking directory. Without that check every path in the filesystem
    // would report as a remote-less repository and mask the `NotSupported` case above.
    #[test]
    fn a_path_outside_a_repository_is_not_a_remote_less_repository() {
        assert!(matches!(
            read_repository_remote("/does/not/exist"),
            RepositoryRemote::NoRepository
        ));

        let root = repository_with_config("no-config", None);
        assert!(
            matches!(
                read_repository_remote(&root.display().to_string()),
                RepositoryRemote::NoRemote
            ),
            "a repository whose config is absent still has no remote"
        );
    }

    #[test]
    fn a_configured_remote_is_returned() {
        let root = repository_with_config(
            "with-remote",
            Some("remote_url = \"lore://127.0.0.1:41337\"\n"),
        );

        let remote = read_repository_remote(&root.display().to_string());
        assert!(
            matches!(remote, RepositoryRemote::Remote(ref url) if url == "lore://127.0.0.1:41337"),
            "a configured remote should be reported verbatim"
        );
    }
}
