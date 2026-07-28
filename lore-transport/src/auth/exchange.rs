// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::error::Disconnected;
use lore_base::error::Maintenance;
use lore_base::error::NoRemote;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::error::NotSupported;
use lore_base::error::Oversized;
use lore_base::error::SlowDown;
use lore_base::lore_debug;
use lore_base::lore_trace;
use lore_base::lore_warn;
use lore_base::types::RepositoryId;
use lore_credential::get_domain_or_empty;
use lore_credential::insecure_decode_token;
use lore_credential::token_store;
use lore_credential::token_store::tokens_only_for_recipient_domain;
use lore_credential::verify_jwt_usage_for_remote;
use lore_error_set::prelude::*;
use tokio::sync::Mutex;

use crate::auth::authentication;

#[error_set]
pub enum ExchangeError {
    NotAuthenticated,
    NotAuthorized,
    Disconnected,
    SlowDown,
    Maintenance,
    NotFound,
    NoRemote,
    NotSupported,
    Oversized,
}

type AuthUrl = String;
type Identity = String;
type CacheResourceId = String;
type RecipientDomain = String;
type AuthzCache = Mutex<HashMap<(AuthUrl, Identity, CacheResourceId, RecipientDomain), String>>;

static AUTHZ_CACHE: std::sync::OnceLock<AuthzCache> = std::sync::OnceLock::new();

fn cache() -> &'static AuthzCache {
    AUTHZ_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn is_expired(expires: u64) -> bool {
    let expires = expires as u128;
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    current_time >= expires
}

/// Exchanges an authentication token for a repository-scoped authorization
/// token via the registered `Authentication` implementation.
///
/// Checks the in-memory cache and on-disk token store first. On miss,
/// loads the authn token and delegates to the implementation's
/// `exchange_for_repository`. The returned authz token is cached in memory
/// and persisted to the token store.
///
/// Token store keys use `"{auth_url}/{repository_id}"` (no implementation-
/// specific prefix). The `Authentication` implementation handles resource ID
/// formatting internally.
pub async fn exchange(
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
    recipient_domain: String,
) -> Result<String, ExchangeError> {
    if auth_url.is_empty() {
        lore_debug!("No auth url, unable to perform authz exchange");
        return Err(NotSupported {
            operation: "No authentication configured on server".to_string(),
        }
        .into());
    }
    if identity.is_empty() {
        lore_debug!("No identity, unable to perform authz exchange");
        return Err(NotAuthenticated.into());
    }

    let auth_domain = get_domain_or_empty(auth_url);
    let auth_url = auth_url.to_string();
    let repo_id_str = repository.to_string();
    let cache_key = (
        auth_url.clone(),
        identity.to_string(),
        repo_id_str.clone(),
        recipient_domain.clone(),
    );
    let mut cache = cache().lock().await;

    lore_trace!(
        "Check for cached authz token for {cache_key:?} in cache with {} tokens",
        cache.len()
    );

    let mut token = cache.get(&cache_key).cloned().unwrap_or_default();

    // Token store key: "{auth_url}/{repository_id}" (no urc- prefix)
    let token_store_key = format!("{auth_url}/{repo_id_str}");

    if !token.is_empty() {
        lore_trace!("Found cached authz token for {cache_key:?}");
    } else {
        lore_trace!("Check for token store authz token for {token_store_key:?}");
        token = token_store::load_user_token(
            &token_store_key,
            identity,
            tokens_only_for_recipient_domain(recipient_domain.clone()),
        )
        .await
        .unwrap_or_default();
    }

    if !token.is_empty() {
        lore_trace!("Validating token expiry");
        if let Some(user_info) = lore_credential::user_info_from_token(token.clone()) {
            if !is_expired(user_info.expires) {
                lore_trace!("Using authz token for {cache_key:?}");
                cache.insert(cache_key, token.clone());
                return Ok(token.clone());
            } else {
                lore_debug!("Authz token for {cache_key:?} has expired");
            }
        } else {
            lore_warn!("Invalid authz token found for {cache_key:?}");
        }
    } else {
        lore_trace!("No stored authz token found for {cache_key:?}");
    }

    // Load authn token for the auth service domain
    lore_trace!("Authorizing using authn identity: {identity}");
    let Some(auth_service_only_token) = lore_credential::user_info(
        auth_url.as_str(),
        identity,
        tokens_only_for_recipient_domain(auth_domain),
    )
    .await
    else {
        lore_debug!("Not authenticated, unable to perform authz exchange");
        return Err(NotAuthenticated.into());
    };
    lore_trace!("Authorizing using endpoint: {auth_url}");

    let time_start = Instant::now();

    // Delegate to the Authentication implementation
    let auth_impl = authentication::find(&auth_url)
        .forward::<ExchangeError>("Unable to connect to auth exchange endpoint")?;
    // The correlation_id is no longer available from ExecutionContext in lore-transport.
    // Pass an empty string -- the gRPC interceptor may inject it from ambient state.
    let correlation_id = String::new();

    lore_trace!("Send auth exchange request");
    let authz = auth_impl
        .exchange_for_repository(
            &auth_url,
            &auth_service_only_token.token,
            repository,
            &correlation_id,
        )
        .await
        .map_err(|err| {
            if err.is_not_authorized() {
                ExchangeError::from(NotAuthorized)
            } else {
                ExchangeError::internal_with_context(err, "Failed to exchange token")
            }
        })?;

    let token = authz.token;
    if token.is_empty() {
        return Err(ExchangeError::internal("Empty token response"));
    }
    let decoded_token = insecure_decode_token(&token)
        .internal("Could not decode token")
        .map_err(ExchangeError::from)?;
    verify_jwt_usage_for_remote(&decoded_token.claims, &recipient_domain).map_err(|err| {
        lore_warn!("{err}");
        ExchangeError::internal_with_context(
            err,
            "The token is not suitable for what you intend to do",
        )
    })?;

    lore_trace!(
        "Authorization with user token successful in {} ms",
        time_start.elapsed().as_millis()
    );

    lore_trace!("Cached authz token for {cache_key:?}");

    cache.insert(cache_key, token.clone());

    let _ = token_store::store_user_token(
        &token_store_key,
        identity,
        &token,
        decoded_token.claims.acceptable_root_domains(),
    )
    .await
    .map_err(|err| {
        lore_warn!("Failed to store token: {err}");
    });

    Ok(token)
}

/// Exchanges an authentication token for an authorization token scoped to an
/// arbitrary resource identifier (non-repository). Mirrors `exchange` but
/// delegates to the implementation's `exchange_for_custom_resource`, letting
/// callers authorize against resources the `RepositoryId` model cannot express.
///
/// The `resource_id` is used verbatim as the cache/token-store key and is
/// passed unmodified to the auth backend.
pub async fn exchange_custom_resource(
    auth_url: &str,
    identity: &str,
    resource_id: &str,
    recipient_domain: String,
) -> Result<String, ExchangeError> {
    if auth_url.is_empty() {
        lore_debug!("No auth url, unable to perform authz exchange");
        return Err(NotSupported {
            operation: "No authentication configured on server".to_string(),
        }
        .into());
    }
    if identity.is_empty() {
        lore_debug!("No identity, unable to perform authz exchange");
        return Err(NotAuthenticated.into());
    }
    if resource_id.is_empty() {
        lore_debug!("No resource_id, unable to perform authz exchange");
        return Err(ExchangeError::internal(
            "Failed to exchange token: empty resource_id",
        ));
    }

    let auth_domain = get_domain_or_empty(auth_url);
    let auth_url = auth_url.to_string();
    let cache_key = (
        auth_url.clone(),
        identity.to_string(),
        resource_id.to_string(),
        recipient_domain.clone(),
    );
    let mut cache = cache().lock().await;

    lore_trace!(
        "Check for cached authz token for {cache_key:?} in cache with {} tokens",
        cache.len()
    );

    let mut token = cache.get(&cache_key).cloned().unwrap_or_default();

    // Token store key: "{auth_url}/{resource_id}" -- same shape as the
    // repository variant, with the resource ID taking the repository slot.
    let token_store_key = format!("{auth_url}/{resource_id}");

    if !token.is_empty() {
        lore_trace!("Found cached authz token for {cache_key:?}");
    } else {
        lore_trace!("Check for token store authz token for {token_store_key:?}");
        token = token_store::load_user_token(
            &token_store_key,
            identity,
            tokens_only_for_recipient_domain(recipient_domain.clone()),
        )
        .await
        .unwrap_or_default();
    }

    if !token.is_empty() {
        lore_trace!("Validating token expiry");
        if let Some(user_info) = lore_credential::user_info_from_token(token.clone()) {
            if !is_expired(user_info.expires) {
                lore_trace!("Using authz token for {cache_key:?}");
                cache.insert(cache_key, token.clone());
                return Ok(token.clone());
            } else {
                lore_debug!("Authz token for {cache_key:?} has expired");
            }
        } else {
            lore_warn!("Invalid authz token found for {cache_key:?}");
        }
    } else {
        lore_trace!("No stored authz token found for {cache_key:?}");
    }

    lore_trace!("Authorizing using authn identity: {identity}");
    let Some(auth_service_only_token) = lore_credential::user_info(
        auth_url.as_str(),
        identity,
        tokens_only_for_recipient_domain(auth_domain),
    )
    .await
    else {
        lore_debug!("Not authenticated, unable to perform authz exchange");
        return Err(NotAuthenticated.into());
    };
    lore_trace!("Authorizing using endpoint: {auth_url}");

    let time_start = Instant::now();

    let auth_impl = authentication::find(&auth_url)
        .forward::<ExchangeError>("Unable to connect to auth exchange endpoint")?;
    // The correlation_id is no longer available from ExecutionContext in lore-transport.
    // Pass an empty string -- the gRPC interceptor may inject it from ambient state.
    let correlation_id = String::new();

    lore_trace!("Send auth exchange request");
    let authz = auth_impl
        .exchange_for_custom_resource(
            &auth_url,
            &auth_service_only_token.token,
            resource_id,
            &correlation_id,
        )
        .await
        .map_err(|err| {
            if err.is_not_authorized() {
                ExchangeError::from(NotAuthorized)
            } else {
                ExchangeError::internal_with_context(err, "Failed to exchange token")
            }
        })?;

    let token = authz.token;
    if token.is_empty() {
        return Err(ExchangeError::internal("Empty token response"));
    }
    let decoded_token = insecure_decode_token(&token)
        .internal("Could not decode token")
        .map_err(ExchangeError::from)?;
    verify_jwt_usage_for_remote(&decoded_token.claims, &recipient_domain).map_err(|err| {
        lore_warn!("{err}");
        ExchangeError::internal_with_context(
            err,
            "The token is not suitable for what you intend to do",
        )
    })?;

    lore_trace!(
        "Authorization with user token successful in {} ms",
        time_start.elapsed().as_millis()
    );

    lore_trace!("Cached authz token for {cache_key:?}");

    cache.insert(cache_key, token.clone());

    let _ = token_store::store_user_token(
        &token_store_key,
        identity,
        &token,
        decoded_token.claims.acceptable_root_domains(),
    )
    .await
    .map_err(|err| {
        lore_warn!("Failed to store token: {err}");
    });

    Ok(token)
}

/// Resolves an identity and obtains authentication/authorization tokens.
///
/// Returned tuple: (`authentication_token`, `authorization_token`, `resolved_identity`)
///
/// If `identity` is empty, iterates over available identities for the given
/// `auth_url` and tries to find one that can authenticate (and optionally
/// authorize for the given repository).
pub async fn auth_exchange(
    auth_url: &str,
    remote_domain: &str,
    identity: &str,
    repository: RepositoryId,
) -> (String, String, String) {
    if !identity.is_empty() {
        return auth_exchange_for_identity(auth_url, remote_domain, identity, repository).await;
    }

    // No identity given, resolve one from available identities
    let Ok(identities) = token_store::load_identities(auth_url).await else {
        lore_debug!("No identities found for {auth_url}");
        return (String::new(), String::new(), String::new());
    };

    if repository.is_zero() {
        // No resource, pick first identity with a valid authn token
        for entry in &identities {
            let result =
                auth_exchange_for_identity(auth_url, remote_domain, entry, repository).await;
            if !result.0.is_empty() {
                return result;
            }
        }
        return (String::new(), String::new(), String::new());
    }

    // Try each identity: first check for cached/stored authz token, then try exchange
    for entry in &identities {
        let result = auth_exchange_for_identity(auth_url, remote_domain, entry, repository).await;
        if !result.1.is_empty() {
            return result;
        }
    }

    lore_debug!("No identity could be authorized for repository {repository}");
    (String::new(), String::new(), String::new())
}

async fn auth_exchange_for_identity(
    auth_url: &str,
    remote_domain: &str,
    identity: &str,
    repository: RepositoryId,
) -> (String, String, String) {
    let Ok(authentication_token) = token_store::load_user_token(
        auth_url,
        identity,
        tokens_only_for_recipient_domain(remote_domain.to_string()),
    )
    .await
    else {
        lore_debug!("Auth exchange failed, no user authentication token found for {identity}");
        return (String::new(), String::new(), String::new());
    };

    // Reject expired authn tokens
    if let Some(info) = lore_credential::user_info_from_token(authentication_token.clone())
        && is_expired(info.expires)
    {
        lore_debug!("Skipping identity {identity}, authn token is expired");
        return (String::new(), String::new(), String::new());
    }

    // This will return the cached authz token if it is still valid,
    // or perform an authz exchange if needed
    let authorization_token = if !repository.is_zero() {
        exchange(auth_url, identity, repository, remote_domain.to_string())
            .await
            .inspect_err(|err| {
                lore_debug!("Auth exchange failed for repository {repository}: {err}");
            })
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Dedupe these debug lines: the same identity getting reselected for the
    // same repository/domain pair on every authz refresh is the steady-state
    // and just spams the log. Re-emit only when the inputs change. The lock
    // is dropped before we log so the dispatch (file write, event channel)
    // cannot block other callers.
    if !authorization_token.is_empty() {
        static LAST_AUTHORIZED: parking_lot::Mutex<Option<(String, RepositoryId, String)>> =
            parking_lot::Mutex::new(None);
        let key = (identity.to_string(), repository, remote_domain.to_string());
        let changed = {
            let mut last = LAST_AUTHORIZED.lock();
            if last.as_ref() != Some(&key) {
                *last = Some(key);
                true
            } else {
                false
            }
        };
        if changed {
            lore_debug!(
                "Selected identity {identity}, authorized for repository {repository} on {remote_domain}"
            );
        }
    } else if repository.is_zero() {
        static LAST_AUTHENTICATED: parking_lot::Mutex<Option<(String, String)>> =
            parking_lot::Mutex::new(None);
        let key = (identity.to_string(), remote_domain.to_string());
        let changed = {
            let mut last = LAST_AUTHENTICATED.lock();
            if last.as_ref() != Some(&key) {
                *last = Some(key);
                true
            } else {
                false
            }
        };
        if changed {
            lore_debug!("Selected identity {identity}, authenticated for {remote_domain}");
        }
    }

    (
        authentication_token,
        authorization_token,
        identity.to_string(),
    )
}

/// Resolves an identity and obtains authentication/authorization tokens for an
/// arbitrary resource identifier.
///
/// Returned tuple: (`authentication_token`, `authorization_token`, `resolved_identity`)
///
/// Mirrors `auth_exchange`, but authorizes against a caller-supplied resource
/// identifier rather than a repository.
pub async fn auth_exchange_custom_resource(
    auth_url: &str,
    remote_domain: &str,
    identity: &str,
    resource_id: &str,
) -> (String, String, String) {
    if !identity.is_empty() {
        return auth_exchange_custom_resource_for_identity(
            auth_url,
            remote_domain,
            identity,
            resource_id,
        )
        .await;
    }

    let Ok(identities) = token_store::load_identities(auth_url).await else {
        lore_debug!("No identities found for {auth_url}");
        return (String::new(), String::new(), String::new());
    };

    for entry in &identities {
        let result =
            auth_exchange_custom_resource_for_identity(auth_url, remote_domain, entry, resource_id)
                .await;
        if !result.1.is_empty() {
            return result;
        }
    }

    lore_debug!("No identity could be authorized for resource {resource_id}");
    (String::new(), String::new(), String::new())
}

async fn auth_exchange_custom_resource_for_identity(
    auth_url: &str,
    remote_domain: &str,
    identity: &str,
    resource_id: &str,
) -> (String, String, String) {
    let Ok(authentication_token) = token_store::load_user_token(
        auth_url,
        identity,
        tokens_only_for_recipient_domain(remote_domain.to_string()),
    )
    .await
    else {
        lore_debug!("Auth exchange failed, no user authentication token found for {identity}");
        return (String::new(), String::new(), String::new());
    };

    if let Some(info) = lore_credential::user_info_from_token(authentication_token.clone())
        && is_expired(info.expires)
    {
        lore_debug!("Skipping identity {identity}, authn token is expired");
        return (String::new(), String::new(), String::new());
    }

    let authorization_token =
        exchange_custom_resource(auth_url, identity, resource_id, remote_domain.to_string())
            .await
            .inspect_err(|err| {
                lore_debug!("Auth exchange failed for resource {resource_id}: {err}");
            })
            .unwrap_or_default();

    // Dedupe: same identity reselected for the same resource/domain on every
    // refresh is the steady-state — re-emit only when the inputs change.
    // Drop the lock before logging so dispatch can't block other callers.
    if !authorization_token.is_empty() {
        static LAST_RESOURCE_AUTHORIZED: parking_lot::Mutex<Option<(String, String, String)>> =
            parking_lot::Mutex::new(None);
        let key = (
            identity.to_string(),
            resource_id.to_string(),
            remote_domain.to_string(),
        );
        let changed = {
            let mut last = LAST_RESOURCE_AUTHORIZED.lock();
            if last.as_ref() != Some(&key) {
                *last = Some(key);
                true
            } else {
                false
            }
        };
        if changed {
            lore_debug!(
                "Selected identity {identity}, authorized for resource {resource_id} on {remote_domain}"
            );
        }
    }

    (
        authentication_token,
        authorization_token,
        identity.to_string(),
    )
}
