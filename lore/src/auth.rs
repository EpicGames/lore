// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::error::Disconnected;
use lore_base::error::Maintenance;
use lore_base::error::NoRemote;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::error::NotSupported;
use lore_base::error::Oversized;
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

use crate::call::repository_call_read;
use crate::call::setup_execution;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreString;

#[error_set]
pub enum AuthStoreError {
    TokenNotFound,
    // The remaining variants mirror `ProtocolError` so a connect failure can be
    // forwarded whole, preserving its kind instead of collapsing to `Internal`.
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
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
            with_token: 0,
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

fn read_repository_config(repository_path: &str) -> Option<String> {
    // If this command is invoked in a repository, load the config
    if let Ok(repository_config) =
        lore_revision::repository::load_repository_config(repository_path)
    {
        repository_config.remote_url
    } else {
        None
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
        read_repository_config(globals.repository_path.as_str()).unwrap_or_default()
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
        read_repository_config(globals.repository_path.as_str()).unwrap_or_default()
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

    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                let auth_url =
                    resolve_auth_endpoint(args.auth_url.as_str(), &repository_path).await?;

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
    /// Emit cached token details for identities with a local token
    pub with_token: u8,
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
/// When `with_token` is set, emits `AuthUserToken` events (including the
/// cached token string) for identities that have a locally stored token,
/// and `AuthUserInfo` events for others.
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
/// | [`LoreEvent::AuthUserToken`](crate::interface::LoreEvent::AuthUserToken) | Emitted instead of `AuthUserInfo` when `with_token` is set and a cached token is available, includes full token details |
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
) -> Result<String, AuthStoreError> {
    if !auth_endpoint.is_empty() {
        return Ok(auth_endpoint.to_string());
    }

    // Forward the connect error instead of discarding it, so the real failure
    // reaches the caller rather than collapsing into the generic error below.
    if let Some(remote_url) = read_repository_config(repository_path) {
        let connection = lore_revision::protocol::connect(
            &remote_url,
            "",
            lore_revision::lore::RepositoryId::default(),
        )
        .await
        .forward::<AuthStoreError>("resolving auth endpoint from remote")?;

        let auth_url = connection.auth_url().to_string();
        if !auth_url.is_empty() {
            return Ok(auth_url);
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
    let repository_path = globals.repository_path.to_string();

    let execution = setup_execution(globals, callback);

    let include_token = args.with_token != 0;

    LORE_CONTEXT
        .scope(execution, async move {
            let result = async move {
                let auth_endpoint =
                    resolve_auth_endpoint(args.auth_endpoint.as_str(), &repository_path).await?;

                let mut user_ids: Vec<String> = args
                    .user_ids
                    .as_slice()
                    .iter()
                    .map(|s| s.as_str().to_string())
                    .collect();

                // When no user IDs are provided, resolve the current user
                if user_ids.is_empty() {
                    let identities = lore_credential::token_store::load_identities(&auth_endpoint)
                        .await
                        .forward::<AuthStoreError>("accessing token store")?;
                    if let Some(first) = identities.into_iter().next() {
                        user_ids.push(first);
                    }
                }

                let resolved = lore_revision::auth::userinfo::resolve_local_user_info(
                    &auth_endpoint,
                    &user_ids,
                )
                .await;

                for entry in &resolved {
                    if include_token && let Some(user_info) = &entry.local_user_info {
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

                Ok::<(), AuthStoreError>(())
            }
            .await;
            execution_context().dispatcher.complete_result(result).await
        })
        .await
}

#[cfg(test)]
mod resolve_auth_endpoint_tests {
    use lore_base::error::NotSupported;
    use lore_error_set::FfiError;

    use super::resolve_auth_endpoint;

    // An explicit endpoint is returned verbatim without touching the
    // repository config or the network.
    #[tokio::test]
    async fn returns_explicit_endpoint_verbatim() {
        let endpoint = resolve_auth_endpoint("ucs-auth://auth.example.com", "/does/not/exist")
            .await
            .expect("an explicit endpoint should resolve");
        assert_eq!(endpoint, "ucs-auth://auth.example.com");
    }

    // With no explicit endpoint and no resolvable repository remote, the
    // operation is `NotSupported` (code 18) — there is no URL to key a token
    // lookup on — rather than an opaque internal error.
    #[tokio::test]
    async fn missing_endpoint_is_not_supported() {
        let err = resolve_auth_endpoint("", "/does/not/exist")
            .await
            .expect_err("a missing endpoint must be an error");

        let not_supported_code = NotSupported {
            operation: String::new(),
        }
        .ffi_code();
        assert_eq!(err.ffi_code(), not_supported_code);
    }
}
