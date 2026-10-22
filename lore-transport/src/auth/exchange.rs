// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::error::AddressNotFound;
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
use lore_base::lore_spawn_guarded;
use lore_base::lore_trace;
use lore_base::lore_warn;
use lore_base::types::RepositoryId;
use lore_credential::get_domain_or_empty;
use lore_credential::insecure_decode_token;
use lore_credential::token_store;
use lore_credential::token_store::Refreshed;
use lore_credential::token_store::tokens_only_for_recipient_domain;
use lore_credential::verify_jwt_usage_for_remote;
use lore_error_set::prelude::*;
use tokio::sync::Mutex;

use crate::auth::AuthEndpoint;
use crate::connection::CredentialStore;
use crate::connection::credential_store;

#[error_set]
pub enum ExchangeError {
    NotAuthenticated,
    NotAuthorized,
    Disconnected,
    SlowDown,
    Maintenance,
    NotFound,
    AddressNotFound,
    NoRemote,
    NotSupported,
    Oversized,
}

type AuthEndpointUrl = String;
type Identity = String;
type CacheResourceId = String;
type RecipientDomain = String;
type CredentialFingerprint = String;
type AuthzCacheKey = (
    AuthEndpointUrl,
    Identity,
    CacheResourceId,
    RecipientDomain,
    CredentialFingerprint,
    CredentialStore,
);
type AuthzCache = Mutex<HashMap<AuthzCacheKey, String>>;

static AUTHZ_CACHE: std::sync::OnceLock<AuthzCache> = std::sync::OnceLock::new();

fn cache() -> &'static AuthzCache {
    AUTHZ_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The key an authorization for `resource` is cached under: the endpoint, the identity, the
/// resource, the recipient domain, the fingerprint of the identity token the caller supplied,
/// empty for credentials from the token store, and the token store the call reads.
fn authorization_cache_key(
    auth_endpoint: &AuthEndpoint,
    identity: &str,
    resource: &str,
    recipient_domain: &str,
    identity_token: &str,
) -> AuthzCacheKey {
    (
        auth_endpoint.url.clone(),
        identity.to_string(),
        resource.to_string(),
        recipient_domain.to_string(),
        lore_credential::token_fingerprint(identity_token),
        credential_store(),
    )
}

/// The key an authorization for `resource` is stored under in the token store.
#[lore_macro::test_pub]
fn authorization_store_key(auth_endpoint: &AuthEndpoint, resource: &str) -> String {
    format!("{auth_endpoint}/{resource}")
}

/// Forgets `refused`, an authorization for `repository` that [`exchange`] handed out and the
/// server has since refused, so the next exchange does not serve `refused` until it expires.
///
/// It is forgotten from the cache and, unless the caller supplied the identity token, from the
/// token store, which the exchange reads on a cache miss. The cache stays locked until both are
/// done, so no exchange reads `refused` back from the store meanwhile. An entry holding another
/// token is left alone, as another caller has replaced it already. The store's entry is compared
/// as [`token_store::remove_user_token_if`] compares it, so one another process has stored is
/// left alone too.
#[lore_macro::test_pub]
pub(crate) async fn forget_refused_authorization(
    auth_endpoint: &AuthEndpoint,
    identity: &str,
    repository: RepositoryId,
    recipient_domain: &str,
    identity_token: &str,
    refused: &str,
) {
    let repository = repository.to_string();
    let cache_key = authorization_cache_key(
        auth_endpoint,
        identity,
        &repository,
        recipient_domain,
        identity_token,
    );
    let mut cache = cache().lock().await;
    if cache.get(&cache_key).is_some_and(|token| token == refused) {
        cache.remove(&cache_key);
    }
    if identity_token.is_empty() {
        let token_store_key = authorization_store_key(auth_endpoint, &repository);
        let _ = token_store::remove_user_token_if(&token_store_key, identity, refused)
            .await
            .inspect_err(|err| lore_warn!("Failed to remove a refused authorization: {err}"));
    }
}

pub fn is_expired(expires: u64) -> bool {
    let expires = expires as u128;
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    current_time >= expires
}

fn has_expired(token: &str) -> bool {
    lore_credential::user_info_from_token(token.to_owned())
        .is_some_and(|info| is_expired(info.expires))
}

/// `token`, `identity`'s authentication token from the store, or the token that replaces it
/// when it has expired: one obtained with the stored refresh token, or one another caller has
/// stored meanwhile. `None` when it has expired and nothing replaces it.
///
/// The replacement is read back from the store for `remote_domain`, so it is sent only where
/// the issuer named in it allows.
async fn refresh_if_expired(
    auth_endpoint: &AuthEndpoint,
    identity: &str,
    token: String,
    remote_domain: &str,
) -> Option<String> {
    if !has_expired(&token) {
        return Some(token);
    }
    // The refresh runs in a guarded task, so neither a caller that gives up, such as a
    // connection being cancelled, nor the process shutting down stops it part way, which
    // would lose the refresh token the issuer has consumed.
    let (answer, refreshed) = tokio::sync::oneshot::channel();
    let endpoint = auth_endpoint.clone();
    let user = identity.to_owned();
    lore_spawn_guarded!(async move {
        let refreshed =
            token_store::refresh_user_token(&endpoint.url, &user, &token, |refresh_token| {
                let endpoint = &endpoint;
                async move {
                    // As for the exchange, no correlation ID is available at this layer.
                    match endpoint
                        .auth
                        .refresh_authentication(&endpoint.url, &refresh_token, "")
                        .await
                    {
                        Ok(authn) => Refreshed::Token {
                            token: authn.token,
                            refresh_token: authn.refresh_token,
                            scope: authn.scope,
                        },
                        Err(err) if err.is_not_authenticated() => Refreshed::Refused(err),
                        Err(err) if err.is_disconnected() => Refreshed::Unsent(err),
                        Err(err) => Refreshed::Failed(err),
                    }
                }
            })
            .await;
        let _ = answer.send(refreshed);
    });
    let refreshed = refreshed.await.unwrap_or_else(|_| {
        Err(token_store::TokenStoreError::internal(
            "The token refresh task ended without an answer",
        ))
    });
    match refreshed {
        Ok(_) => {}
        Err(err) if err.is_token_not_found() => {
            lore_debug!("Skipping identity {identity}, authn token is expired");
            return None;
        }
        Err(err) => {
            lore_warn!("Skipping identity {identity}, authn token is expired: {err}");
            return None;
        }
    }
    let token = token_store::load_user_token_from_store(
        &auth_endpoint.url,
        identity,
        tokens_only_for_recipient_domain(remote_domain.to_string()),
    )
    .await
    .ok()
    .filter(|token| !has_expired(token));
    if token.is_none() {
        lore_debug!("Skipping identity {identity}, no unexpired authn token for {remote_domain}");
    }
    token
}

/// The login token to trade at `auth_endpoint` for `identity`: `identity_token` when supplied,
/// else the stored one, refreshed if it has expired. `None` when there is none that is usable.
async fn login_token_for_auth_service(
    auth_endpoint: &AuthEndpoint,
    identity: &str,
    identity_token: &str,
) -> Option<String> {
    if !identity_token.is_empty() {
        return lore_credential::user_info_from_token(identity_token.to_owned())
            .map(|info| info.token);
    }
    let auth_domain = get_domain_or_empty(&auth_endpoint.url);
    let token = token_store::load_user_token_from_store(
        &auth_endpoint.url,
        identity,
        tokens_only_for_recipient_domain(auth_domain.clone()),
    )
    .await
    .ok()?;
    refresh_if_expired(auth_endpoint, identity, token, &auth_domain).await
}

/// Exchanges an authentication token for a repository-scoped authorization
/// token via the registered `Authentication` implementation.
///
/// Checks the in-memory cache and on-disk token store first. On miss,
/// loads the authn token and delegates to the implementation's
/// `exchange_for_repository`. The returned authz token is cached in memory
/// and persisted to the token store. If the caller supplies external
/// `identity_token` or `access_token`, the exchanged token is not persisted
/// to on-disk store to keep tokens related to external identities out of
/// the store. Also the on-disk store is not read in this case to not mix the
/// external tokens with cached login tokens. The in-memory cache keeps them
/// apart too: an entry is keyed by the credential that earned it, so a supplied
/// token is never served an authorization that another credential produced, and
/// by the token store the call reads, so a call is never served an authorization
/// another caller's store produced.
///
/// Token store keys use `"{auth_endpoint.url}/{repository_id}"` (no implementation-
/// specific prefix). The `Authentication` implementation handles resource ID
/// formatting internally.
///
/// A non-empty `identity_token` replaces the authentication token.
///
/// A non-empty `access_token` the authorization token.
pub async fn exchange(
    auth_endpoint: &AuthEndpoint,
    identity: &str,
    repository: RepositoryId,
    recipient_domain: String,
    identity_token: &str,
    access_token: &str,
) -> Result<String, ExchangeError> {
    if !access_token.is_empty() {
        lore_debug!("Using the supplied access token for repository {repository}");
        return Ok(access_token.to_string());
    }
    if auth_endpoint.is_none() {
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

    let auth_url = auth_endpoint.url.clone();
    let repo_id_str = repository.to_string();
    // A supplied access token returned above, so the identity token is the only
    // credential that reaches the cache or the store from here.
    let supplied_credentials = !identity_token.is_empty();
    let cache_key = authorization_cache_key(
        auth_endpoint,
        identity,
        &repo_id_str,
        &recipient_domain,
        identity_token,
    );
    let mut cache = cache().lock().await;

    lore_trace!(
        "Check for cached authz token for {cache_key:?} in cache with {} tokens",
        cache.len()
    );

    let mut token = cache.get(&cache_key).cloned().unwrap_or_default();

    let token_store_key = authorization_store_key(auth_endpoint, &repo_id_str);

    if !token.is_empty() {
        lore_trace!("Found cached authz token for {cache_key:?}");
    } else if !supplied_credentials {
        lore_trace!("Check for token store authz token for {token_store_key:?}");
        token = token_store::load_user_token_from_store(
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
    let Some(auth_service_only_token) =
        login_token_for_auth_service(auth_endpoint, identity, identity_token).await
    else {
        lore_debug!("Not authenticated, unable to perform authz exchange");
        return Err(NotAuthenticated.into());
    };
    lore_trace!("Authorizing using endpoint: {auth_endpoint}");

    let time_start = Instant::now();

    // Delegate to the Authentication implementation
    let auth_impl = &auth_endpoint.auth;
    // The correlation_id is no longer available from ExecutionContext in lore-transport.
    // Pass an empty string -- the gRPC interceptor may inject it from ambient state.
    let correlation_id = String::new();

    lore_trace!("Send auth exchange request");
    let authz = auth_impl
        .exchange_for_repository(
            &auth_url,
            &auth_service_only_token,
            repository,
            &correlation_id,
        )
        .await
        .map_err(|err| {
            if err.is_not_authorized() {
                ExchangeError::from(NotAuthorized)
            } else if err.is_not_authenticated() {
                ExchangeError::from(NotAuthenticated)
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

    if !supplied_credentials {
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
    }

    Ok(token)
}

/// Exchanges an authentication token for an authorization token scoped to an
/// arbitrary resource identifier (non-repository). Mirrors `exchange` but
/// delegates to the implementation's `exchange_for_custom_resource`, letting
/// callers authorize against resources the `RepositoryId` model cannot express.
///
/// The `resource_id` is used verbatim as the cache/token-store key and is
/// passed unmodified to the auth backend.
///
/// `identity_token` and `access_token` are the caller-supplied credentials.
pub async fn exchange_custom_resource(
    auth_endpoint: &AuthEndpoint,
    identity: &str,
    resource_id: &str,
    recipient_domain: String,
    identity_token: &str,
    access_token: &str,
) -> Result<String, ExchangeError> {
    if !access_token.is_empty() {
        lore_debug!("Using the supplied access token for resource {resource_id}");
        return Ok(access_token.to_string());
    }
    if auth_endpoint.is_none() {
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

    let auth_url = auth_endpoint.url.clone();
    // A supplied access token returned above, so the identity token is the only
    // credential that reaches the cache or the store from here.
    let supplied_credentials = !identity_token.is_empty();
    let cache_key = authorization_cache_key(
        auth_endpoint,
        identity,
        resource_id,
        &recipient_domain,
        identity_token,
    );
    let mut cache = cache().lock().await;

    lore_trace!(
        "Check for cached authz token for {cache_key:?} in cache with {} tokens",
        cache.len()
    );

    let mut token = cache.get(&cache_key).cloned().unwrap_or_default();

    let token_store_key = authorization_store_key(auth_endpoint, resource_id);

    if !token.is_empty() {
        lore_trace!("Found cached authz token for {cache_key:?}");
    } else if !supplied_credentials {
        lore_trace!("Check for token store authz token for {token_store_key:?}");
        token = token_store::load_user_token_from_store(
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
    let Some(auth_service_only_token) =
        login_token_for_auth_service(auth_endpoint, identity, identity_token).await
    else {
        lore_debug!("Not authenticated, unable to perform authz exchange");
        return Err(NotAuthenticated.into());
    };
    lore_trace!("Authorizing using endpoint: {auth_endpoint}");

    let time_start = Instant::now();

    let auth_impl = &auth_endpoint.auth;
    // The correlation_id is no longer available from ExecutionContext in lore-transport.
    // Pass an empty string -- the gRPC interceptor may inject it from ambient state.
    let correlation_id = String::new();

    lore_trace!("Send auth exchange request");
    let authz = auth_impl
        .exchange_for_custom_resource(
            &auth_url,
            &auth_service_only_token,
            resource_id,
            &correlation_id,
        )
        .await
        .map_err(|err| {
            if err.is_not_authorized() {
                ExchangeError::from(NotAuthorized)
            } else if err.is_not_authenticated() {
                ExchangeError::from(NotAuthenticated)
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

    if !supplied_credentials {
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
    }

    Ok(token)
}

/// Resolves an identity and obtains authentication/authorization tokens.
///
/// Returned tuple: (`authentication_token`, `authorization_token`, `resolved_identity`)
///
/// If `identity` is empty, iterates over available identities for the given
/// `auth_endpoint` and tries to find one that can authenticate (and optionally
/// authorize for the given repository).
///
/// `identity_token` and `access_token` are the caller-supplied credentials.
pub async fn auth_exchange(
    auth_endpoint: &AuthEndpoint,
    remote_domain: &str,
    identity: &str,
    repository: RepositoryId,
    identity_token: &str,
    access_token: &str,
) -> (String, String, String) {
    if !identity.is_empty() {
        return auth_exchange_for_identity(
            auth_endpoint,
            remote_domain,
            identity,
            repository,
            identity_token,
            access_token,
        )
        .await;
    }

    // No identity given, resolve one from available identities
    let Ok(identities) = token_store::load_identities(&auth_endpoint.url).await else {
        lore_debug!("No identities found for {auth_endpoint}");
        return (String::new(), String::new(), String::new());
    };

    if repository.is_zero() {
        // No resource, pick first identity with a valid authn token
        for entry in &identities {
            let result =
                auth_exchange_for_identity(auth_endpoint, remote_domain, entry, repository, "", "")
                    .await;
            if !result.0.is_empty() {
                return result;
            }
        }
        return (String::new(), String::new(), String::new());
    }

    // Try each identity: first check for cached/stored authz token, then try exchange
    for entry in &identities {
        let result =
            auth_exchange_for_identity(auth_endpoint, remote_domain, entry, repository, "", "")
                .await;
        if !result.1.is_empty() {
            return result;
        }
    }

    lore_debug!("No identity could be authorized for repository {repository}");
    (String::new(), String::new(), String::new())
}

async fn auth_exchange_for_identity(
    auth_endpoint: &AuthEndpoint,
    remote_domain: &str,
    identity: &str,
    repository: RepositoryId,
    identity_token: &str,
    access_token: &str,
) -> (String, String, String) {
    let authentication_token = token_store::load_user_token(
        &auth_endpoint.url,
        identity,
        tokens_only_for_recipient_domain(remote_domain.to_string()),
        identity_token,
        access_token,
    )
    .await
    .unwrap_or_default();

    // A supplied access token authorizes on its own, so carry on without an
    // authentication token: the services that need one fail where they use it,
    // and the ones that only need authorization still work.
    if authentication_token.is_empty() && access_token.is_empty() {
        lore_debug!("Auth exchange failed, no user authentication token found for {identity}");
        return (String::new(), String::new(), String::new());
    }

    // Refresh an expired authn token, or else reject it, but only one resolved
    // from the store. That check is there to skip a stale stored identity while
    // picking one; a credential the caller supplied is not a candidate to skip,
    // it is an instruction. An expired supplied token is handed over for the
    // server to reject, so the caller sees an authentication failure rather
    // than requests going out carrying no credential at all.
    let authentication_token = if identity_token.is_empty() && access_token.is_empty() {
        match refresh_if_expired(auth_endpoint, identity, authentication_token, remote_domain).await
        {
            Some(token) => token,
            None => return (String::new(), String::new(), String::new()),
        }
    } else {
        authentication_token
    };

    // This will return the cached authz token if it is still valid,
    // or perform an authz exchange if needed
    let authorization_token = if !repository.is_zero() {
        exchange(
            auth_endpoint,
            identity,
            repository,
            remote_domain.to_string(),
            identity_token,
            access_token,
        )
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
    auth_endpoint: &AuthEndpoint,
    remote_domain: &str,
    identity: &str,
    resource_id: &str,
    identity_token: &str,
    access_token: &str,
) -> (String, String, String) {
    if !identity.is_empty() {
        return auth_exchange_custom_resource_for_identity(
            auth_endpoint,
            remote_domain,
            identity,
            resource_id,
            identity_token,
            access_token,
        )
        .await;
    }

    let Ok(identities) = token_store::load_identities(&auth_endpoint.url).await else {
        lore_debug!("No identities found for {auth_endpoint}");
        return (String::new(), String::new(), String::new());
    };

    // Store-resolved identities use store credentials, pass empty tokens.
    for entry in &identities {
        let result = auth_exchange_custom_resource_for_identity(
            auth_endpoint,
            remote_domain,
            entry,
            resource_id,
            "",
            "",
        )
        .await;
        if !result.1.is_empty() {
            return result;
        }
    }

    lore_debug!("No identity could be authorized for resource {resource_id}");
    (String::new(), String::new(), String::new())
}

async fn auth_exchange_custom_resource_for_identity(
    auth_endpoint: &AuthEndpoint,
    remote_domain: &str,
    identity: &str,
    resource_id: &str,
    identity_token: &str,
    access_token: &str,
) -> (String, String, String) {
    let authentication_token = token_store::load_user_token(
        &auth_endpoint.url,
        identity,
        tokens_only_for_recipient_domain(remote_domain.to_string()),
        identity_token,
        access_token,
    )
    .await
    .unwrap_or_default();

    // A supplied access token authorizes on its own, so carry on without an
    // authentication token: the services that need one fail where they use it,
    // and the ones that only need authorization still work.
    if authentication_token.is_empty() && access_token.is_empty() {
        lore_debug!("Auth exchange failed, no user authentication token found for {identity}");
        return (String::new(), String::new(), String::new());
    }

    // As in `auth_exchange_for_identity`: only a store-resolved identity is
    // refreshed or skipped for expiry. A supplied credential is handed over
    // regardless.
    let authentication_token = if identity_token.is_empty() && access_token.is_empty() {
        match refresh_if_expired(auth_endpoint, identity, authentication_token, remote_domain).await
        {
            Some(token) => token,
            None => return (String::new(), String::new(), String::new()),
        }
    } else {
        authentication_token
    };

    let authorization_token = exchange_custom_resource(
        auth_endpoint,
        identity,
        resource_id,
        remote_domain.to_string(),
        identity_token,
        access_token,
    )
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
