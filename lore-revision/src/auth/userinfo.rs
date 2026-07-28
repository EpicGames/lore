// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_credential::get_domain_or_empty;
use lore_credential::token_store::vulnerable_all_tokens;
use lore_error_set::prelude::*;
use lore_transport::auth::authentication;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event::EventError;
use crate::event::LoreEvent;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::repository::RepositoryContext;
use crate::util::serde::u8_as_bool;

#[error_set]
pub enum UserInfoError {
    Disconnected,
    NotAuthenticated,
    NotAuthorized,
    NoRemote,
    Maintenance,
    NotFound,
    NotSupported,
    Oversized,
    SlowDown,
}

impl EventError for UserInfoError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Event data resolving a user identity to a display name.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreAuthUserInfoEventData {
    /// User identity
    pub id: LoreString,
    /// Display name for the user
    pub name: LoreString,
}

/// Event data describing a stored authentication identity.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreAuthIdentityEventData {
    /// Auth service URL
    pub auth_url: LoreString,
    /// Resource ID (empty for authentication tokens)
    pub resource: LoreString,
    /// User identity
    pub user_id: LoreString,
    /// Comma-separated list of authorized root domains
    pub authorized_domains: LoreString,
    /// Expiry time in milliseconds since UNIX epoch, or 0 if unavailable
    pub expires: u64,
    /// Cached token (only populated when requested)
    pub token: LoreString,
}

/// Resolves user IDs to display names.
///
/// Requires a repository context. If the current user's id is in the input
/// list and a local JWT token is cached for that identity, emits that single
/// event from the decoded local token (no network call). All other ids — and
/// the current user id if no local token is present — are resolved via the
/// auth service `GetUserInfo` gRPC endpoint, which performs a
/// repository-scoped authorization token exchange with domain validation
/// before sending the authorization token as a Bearer header.
///
/// Emits an [`LoreEvent::AuthUserInfo`] event per resolved user. The event's
/// `id` field always echoes the requested id; the `name` field comes from
/// the decoded token's `preferred_username` (falling back to `name`, then
/// `id`) for the local fast path, or from the auth service response for the
/// remote path. Because the local path reads a cached token captured at
/// login, a name change made server-side since then is not reflected until
/// the token is refreshed.
pub async fn resolve_user_info(
    repository: Arc<RepositoryContext>,
    ids: LoreArray<LoreString>,
) -> Result<(), UserInfoError> {
    let remote = repository
        .remote()
        .await
        .forward::<UserInfoError>("Not connected")?;
    let auth_url = remote.auth_url().to_string();

    let user_ids_vec: Vec<String> = ids
        .as_slice()
        .iter()
        .map(LoreString::as_str)
        .map(ToString::to_string)
        .collect();

    let execution = execution_context();
    let current_user_id = execution.user_id().await;

    // Fast path: if the current user id is in the list and a local JWT token
    // is cached for that identity, decode the name locally instead of calling
    // the auth service. Safe to use vulnerable_all_tokens here — the token is
    // only decoded for its `name` field and is never sent over the network.
    //
    // All occurrences of the current user id are stripped from the list so
    // the remote call never emits a duplicate event for the id already
    // resolved locally.
    let (has_current_user, mut remaining_ids) =
        strip_current_user(user_ids_vec, current_user_id.as_str());
    if has_current_user {
        if let Some(user_info) =
            lore_credential::user_info(&auth_url, current_user_id.as_str(), vulnerable_all_tokens())
                .await
        {
            lore_debug!("User info fast path: current user resolved from local token");
            LoreEvent::AuthUserInfo(LoreAuthUserInfoEventData {
                // Echo the input id rather than the token's `sub` claim —
                // downstream consumers (e.g. the CLI formatter) index by
                // the id they asked for, and OIDC providers can return
                // sub claims in a slightly different form than what the
                // caller supplied.
                id: current_user_id.clone().into(),
                name: display_name(&user_info).into(),
            })
            .send();
        } else {
            // No local token for the current user — let it ride with the rest
            // so the auth service can resolve it.
            remaining_ids.push(current_user_id.clone());
        }
    }

    if remaining_ids.is_empty() {
        return Ok(());
    }

    // Obtain an authorization token from the authentication token
    lore_debug!("Get authorization token for identity {current_user_id} using auth url {auth_url}");
    let token_for_auth_service = lore_transport::auth::exchange::exchange(
        auth_url.as_str(),
        current_user_id.as_str(),
        repository.id,
        get_domain_or_empty(&auth_url),
    )
    .await
    .forward::<UserInfoError>("Failed authorization token exchange")?;

    let auth_impl = authentication::find(&auth_url)
        .forward::<UserInfoError>("Unable to connect to auth info endpoint")?;
    let correlation_id = execution_context().globals().correlation_id.to_string();

    lore_debug!(
        "Start user info request on repository {} for {} unresolved id(s)",
        repository.id,
        remaining_ids.len()
    );
    let resolved = auth_impl
        .get_user_info(
            &auth_url,
            &token_for_auth_service,
            repository.id,
            &remaining_ids,
            &correlation_id,
        )
        .await
        .forward::<UserInfoError>("Failed auth service request")?;

    for user in resolved {
        LoreEvent::AuthUserInfo(LoreAuthUserInfoEventData {
            id: user.user_id.into(),
            name: user.user_name.into(),
        })
        .send();
    }

    lore_debug!("User info query successful");

    Ok(())
}

/// Strip every occurrence of the current-user id from the input list.
/// Returns whether the current user was present (so the fast path should
/// run once) and the remaining ids (preserving input order).
fn strip_current_user(ids: Vec<String>, current_user_id: &str) -> (bool, Vec<String>) {
    let mut has_current = false;
    let mut remaining = Vec::with_capacity(ids.len());
    for id in ids {
        if id == current_user_id {
            has_current = true;
        } else {
            remaining.push(id);
        }
    }
    (has_current, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_pulls_current_user_out_and_preserves_other_order() {
        let ids = vec![
            "other-a".to_string(),
            "self-id".to_string(),
            "other-b".to_string(),
        ];
        let (has_current, remaining) = strip_current_user(ids, "self-id");
        assert!(has_current);
        assert_eq!(
            remaining,
            vec!["other-a".to_string(), "other-b".to_string()]
        );
    }

    #[test]
    fn strip_reports_absent_current_user() {
        let ids = vec!["other-a".to_string(), "other-b".to_string()];
        let (has_current, remaining) = strip_current_user(ids, "self-id");
        assert!(!has_current);
        assert_eq!(
            remaining,
            vec!["other-a".to_string(), "other-b".to_string()]
        );
    }

    #[test]
    fn strip_with_only_current_user_leaves_remaining_empty() {
        let ids = vec!["self-id".to_string()];
        let (has_current, remaining) = strip_current_user(ids, "self-id");
        assert!(has_current);
        assert!(
            remaining.is_empty(),
            "current-only list yields empty remaining; got {remaining:?}"
        );
    }

    #[test]
    fn strip_removes_every_occurrence_of_current_user() {
        // The fast path emits one event for the current user; the remote
        // call must not see any self-id so it cannot emit a duplicate.
        let ids = vec![
            "self-id".to_string(),
            "other".to_string(),
            "self-id".to_string(),
        ];
        let (has_current, remaining) = strip_current_user(ids, "self-id");
        assert!(has_current);
        assert_eq!(
            remaining,
            vec!["other".to_string()],
            "every occurrence of the current user must be stripped"
        );
    }
}

/// Event data carrying a user token along with the identity it belongs to.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreAuthUserTokenEventData {
    /// User identity
    pub id: LoreString,
    /// Display name for the user
    pub name: LoreString,
    /// The token string
    pub token: LoreString,
    /// Preferred username from the token
    pub preferred_username: LoreString,
    /// Non-zero if the identity is a service account
    #[serde(with = "u8_as_bool")]
    pub flag_service_account: u8,
    /// Expiry time in milliseconds since UNIX epoch, or 0 if unavailable
    pub expires: u64,
}

/// Result of resolving a user identity from locally cached JWT tokens.
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub id: String,
    pub name: String,
    /// Full user info from locally cached token, if available.
    ///
    /// The contained [`UserInfo::token`] was loaded without domain filtering
    /// and must not be sent to remote endpoints without first validating the
    /// recipient domain against the token's acceptable root domains via
    /// [`auth::verify_jwt_usage_for_remote`]. Use this field only for local
    /// display purposes (names, expiry, service account flag).
    pub local_user_info: Option<lore_credential::UserInfo>,
}

fn display_name(info: &lore_credential::UserInfo) -> String {
    if !info.preferred_username.is_empty() {
        info.preferred_username.clone()
    } else if !info.name.is_empty() {
        info.name.clone()
    } else {
        info.id.clone()
    }
}

/// Resolves user IDs to identity information using locally stored JWT tokens.
///
/// Does not require a repository context or network access. Decodes locally
/// cached JWT tokens to extract display names. For user IDs without a local
/// token, returns the raw user ID as the display name.
///
/// When resolved from local tokens, `local_user_info` is populated with the
/// full decoded token data (including the token string itself).
///
/// For remote resolution of user IDs (e.g. other users), use
/// [`resolve_user_info`] or [`user_display_name`] which perform a proper
/// authorization exchange scoped to a repository.
pub async fn resolve_local_user_info(auth_url: &str, user_ids: &[String]) -> Vec<ResolvedIdentity> {
    let mut results = vec![];

    for user_id in user_ids {
        if let Some(info) =
            lore_credential::user_info(auth_url, user_id, vulnerable_all_tokens()).await
        {
            let name = display_name(&info);
            results.push(ResolvedIdentity {
                id: info.id.clone(),
                name,
                local_user_info: Some(info),
            });
        } else {
            results.push(ResolvedIdentity {
                id: user_id.clone(),
                name: user_id.clone(),
                local_user_info: None,
            });
        }
    }

    results
}

/// Resolves a single user ID to a display name.
///
/// Requires a repository context. If the requested ID matches the current
/// user, returns the name from the locally cached JWT token (no network
/// call). Otherwise performs a repository-scoped authorization exchange and
/// queries `GetUserInfo` via gRPC with proper domain validation.
///
/// Falls back to returning the raw user ID string if the auth service cannot
/// resolve the name.
pub async fn user_display_name(
    repository: Arc<RepositoryContext>,
    id: &str,
) -> Result<String, UserInfoError> {
    let remote = repository
        .remote()
        .await
        .forward::<UserInfoError>("Not connected")?;
    let auth_url = remote.auth_url().to_string();

    let execution = execution_context();
    let user_id = execution.user_id().await;

    // Safe to use vulnerable_all_tokens here: this is a local JWT decode for
    // the current user's own identity — the token is not sent over the network.
    if user_id == id
        && let Some(user_info) =
            lore_credential::user_info(auth_url.as_str(), user_id.as_str(), vulnerable_all_tokens())
                .await
    {
        return Ok(user_info.name);
    }

    // Obtain an authorization token from the authentication token
    lore_debug!("Get authorization token for identity {user_id} using auth url {auth_url}",);
    let token_for_auth_service = lore_transport::auth::exchange::exchange(
        auth_url.as_str(),
        user_id.as_str(),
        repository.id,
        get_domain_or_empty(&auth_url),
    )
    .await
    .forward::<UserInfoError>("Failed authorization token exchange")?;

    let auth_impl = authentication::find(&auth_url)
        .forward::<UserInfoError>("Unable to connect to auth info endpoint")?;
    let correlation_id = execution_context().globals().correlation_id.to_string();

    lore_debug!(
        "Start user info request for {id} on repository {}",
        repository.id
    );
    let Ok(resolved) = auth_impl
        .get_user_info(
            &auth_url,
            &token_for_auth_service,
            repository.id,
            &[id.to_string()],
            &correlation_id,
        )
        .await
        .inspect_err(|err| {
            lore_debug!("User info request returned error {err}, resolving to user ID");
        })
    else {
        return Ok(id.to_string());
    };

    if let Some(user) = resolved.first() {
        lore_debug!(
            "User info query successful, {id} resolved to user name {}",
            user.user_name
        );
        return Ok(user.user_name.clone());
    }

    lore_debug!("User info request for {id} returned nothing, resolving to user id");
    Ok(id.to_string())
}

/// Resolves a display name to a user ID via the auth service.
///
/// Requires a repository context. Performs a repository-scoped authorization
/// token exchange with domain validation, then queries the auth service.
/// Falls back to returning the input display name if resolution fails.
pub async fn user_id(
    repository: Arc<RepositoryContext>,
    user_name: &str,
) -> Result<String, UserInfoError> {
    let remote = repository
        .remote()
        .await
        .forward::<UserInfoError>("Not connected")?;
    let auth_url = remote.auth_url().to_string();

    let current_user_id = execution_context().user_id().await;

    // Obtain an authorization token from the authentication token
    lore_debug!("Get authorization token for identity {current_user_id} using auth url {auth_url}",);
    let token_for_auth_service = lore_transport::auth::exchange::exchange(
        auth_url.as_str(),
        current_user_id.as_str(),
        repository.id,
        get_domain_or_empty(&auth_url),
    )
    .await
    .forward::<UserInfoError>("Failed authorization token exchange")?;

    let auth_impl = authentication::find(&auth_url)
        .forward::<UserInfoError>("Unable to connect to auth info endpoint")?;
    let correlation_id = execution_context().globals().correlation_id.to_string();

    lore_debug!(
        "Start user id request for {user_name} on repository {}",
        repository.id
    );
    let Ok(resolved) = auth_impl
        .get_user_id(
            &auth_url,
            &token_for_auth_service,
            repository.id,
            user_name,
            &correlation_id,
        )
        .await
        .inspect_err(|err| {
            lore_debug!("User id request returned error {err}, resolving to user name");
        })
    else {
        return Ok(user_name.to_string());
    };

    if let Some(user) = resolved {
        lore_debug!(
            "User info query successful, {} resolved to user id {}",
            user.user_name,
            user.user_id
        );
        return Ok(user.user_id);
    }

    lore_debug!("User id request for {user_name} returned nothing, resolving to user name");
    Ok(user_name.to_string())
}
