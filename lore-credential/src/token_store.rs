// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;

use base64::prelude::BASE64_STANDARD;
use base64::prelude::Engine as _;
use lore_base::directories::project_directory;
use lore_base::error::TokenNotFound;
use lore_base::fs::lock::FSLock;
use lore_base::lore_debug;
use lore_base::lore_trace;
use lore_base::lore_warn;
use lore_error_set::prelude::*;
use ring::aead::AES_256_GCM;
use ring::aead::Aad;
use ring::aead::BoundKey;
use ring::aead::NONCE_LEN;
use ring::aead::Nonce;
use ring::aead::NonceSequence;
use ring::aead::OpeningKey;
use ring::aead::SealingKey;
use ring::aead::UnboundKey;
use ring::error::Unspecified;
use ring::rand::SecureRandom;
use ring::rand::SystemRandom;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use toml;

use crate::jwt::domain_in_root_domains;
use crate::util::get_domain_or_empty;

/// Secure-store service these secrets live under.
const SERVICE_NAME: &str = "org.lore";

/// Secure-store account holding the encryption key, and the stem of its
/// on-disk fallback file. Distinct from the name earlier versions used: they
/// store a different blob layout under the old name, and would overwrite this
/// one with it whenever both fall back to disk.
const ENCRYPTION_KEY_TARGET: &str = "tokenstore_encryption_key";

#[error_set]
pub enum TokenStoreError {
    TokenNotFound,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityToken {
    /// User identity
    user_id: String,
    /// Base64 encoded (encrypted) authentication token
    token: String,
    /// The root domains this token can be given to without security concerns
    #[serde(default)]
    acceptable_root_domains: Vec<String>,
    /// Base64 encoded (encrypted) one-time-use refresh token.
    /// Stored separately from the auth token because it has a different
    /// lifecycle: consumed on use and replaced atomically.
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteIdentity {
    /// Auth service remote URL
    remote: String,
    /// Token info
    token: Vec<IdentityToken>,
}

impl std::fmt::Debug for RemoteIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "remote: {}, token: [...]", self.remote)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TokenMap {
    /// Tokens per remote (an auth service URL) and user identity info
    remotes: Vec<RemoteIdentity>,
}

static TOKEN_MAP: OnceLock<Mutex<Option<TokenMap>>> = OnceLock::new();

pub fn tokens_only_for_recipient_domain(domain: String) -> impl FnMut(&&IdentityToken) -> bool {
    move |item: &&IdentityToken| {
        // backwards compatibility with old `IdentityToken` that don't have the acceptable_root_domains
        // Once end users are using the latest version of Lore then we can remove this case. Without
        // this check, new Lore clients with old tokens will have to run login again
        if item.acceptable_root_domains.is_empty() {
            true
        } else {
            domain_in_root_domains(&domain, &item.acceptable_root_domains)
        }
    }
}

/// No filter on the tokens you get back. Use with caution.
/// See comment at top of `urc-core::auth` - Check Token Recipient
pub fn vulnerable_all_tokens() -> impl FnMut(&&IdentityToken) -> bool {
    move |_item: &&IdentityToken| true
}

fn token_map() -> &'static Mutex<Option<TokenMap>> {
    TOKEN_MAP.get_or_init(|| Mutex::new(None))
}

/// Base directory holding the auth store files (`tokenstore.toml` and the
/// encryption-key fallback). The `LORE_AUTH_PATH` environment variable
/// overrides the default per-user configuration directory.
fn base_path(create_dir: bool) -> Result<PathBuf, TokenStoreError> {
    if let Ok(path) = std::env::var("LORE_AUTH_PATH")
        && !path.is_empty()
    {
        let path = PathBuf::from(path);
        if create_dir {
            fs::create_dir_all(path.as_path()).map_err(|e| {
                lore_warn!("Failed to find base path: {e}");
                TokenStoreError::internal_with_context(e, "Failed to find base path")
            })?;
        }
        return Ok(path);
    }

    let path =
        project_directory().ok_or_else(|| TokenStoreError::internal("Failed to find base path"))?;
    let path = path.config_local_dir();
    if create_dir {
        fs::create_dir_all(path).map_err(|e| {
            lore_warn!("Failed to find base path: {e}");
            TokenStoreError::internal_with_context(e, "Failed to find base path")
        })?;
    }
    Ok(path.to_path_buf())
}

/// Path to the token store. Earlier versions used `tokens.toml` in the same
/// directory and seal tokens in a layout this one does not read; that file is
/// left alone rather than migrated, so the two can coexist without either
/// resetting the other's tokens.
fn token_map_path(create_dir: bool) -> Result<PathBuf, TokenStoreError> {
    let path = base_path(create_dir)?;
    Ok(path.join("tokenstore.toml"))
}

/// Information about a stored identity token.
#[derive(Debug, Clone)]
pub struct StoredIdentityInfo {
    /// Auth service URL
    pub auth_url: String,
    /// Resource ID (empty for authentication tokens)
    pub resource: String,
    /// User identity
    pub user_id: String,
    /// Root domains this token is authorized for
    pub acceptable_root_domains: Vec<String>,
    /// Expiry time in milliseconds since UNIX epoch, or 0 if unavailable
    pub expires_ms: u64,
    /// Decrypted token (only populated when requested)
    pub token: String,
}

/// Splits a token store key into (`auth_url`, `resource_id`).
///
/// Authorization tokens are stored under `"{auth_url}/{repository_id}"` where
/// `repository_id` is a 32-character hex string. Legacy entries may use
/// `"{auth_url}/urc-{repository_id}"` with a `urc-` prefix.
/// Authentication tokens use just the `auth_url` with no resource suffix.
///
/// Only considers the path portion of the URL to avoid matching hostnames
/// like `urc-auth.example.com`.
fn split_remote_resource(store_key: &str) -> (String, String) {
    if let Ok(url) = url::Url::parse(store_key) {
        let path = url.path();
        // New format: last path segment is a 32-char hex repository ID
        if let Some(pos) = path.rfind('/') {
            let segment = &path[pos + 1..];
            if segment.len() == 32 && segment.chars().all(|c| c.is_ascii_hexdigit()) {
                let base_end = store_key.len() - path.len() + pos;
                return (store_key[..base_end].to_string(), segment.to_string());
            }
        }
        // Legacy format: last path segment starts with "urc-"
        if let Some(pos) = path.rfind("/urc-") {
            let resource = &path[pos + 1..];
            let base_end = store_key.len() - path.len() + pos;
            return (store_key[..base_end].to_string(), resource.to_string());
        }
    }
    (store_key.to_string(), String::new())
}

/// Load all stored identities across all remotes, decrypting tokens to extract expiry.
///
/// When `include_token` is true, the decrypted token string is included in the result.
pub async fn load_all_identities(
    include_token: bool,
) -> Result<Vec<StoredIdentityInfo>, TokenStoreError> {
    let identity_entries = {
        let token_map = token_map();
        let mut store = token_map.lock().await;
        if store.is_none()
            && let Ok(guard) = lock_token_map().await
            && let Ok(loaded_map) = load_token_map(&guard)
        {
            store.replace(loaded_map);
        }

        let mut entries = vec![];
        if let Some(map) = store.as_ref() {
            for remote in &map.remotes {
                let (auth_url, resource) = split_remote_resource(&remote.remote);
                for identity in &remote.token {
                    entries.push((auth_url.clone(), resource.clone(), identity.clone()));
                }
            }
        }
        entries
    };

    let mut result = vec![];
    for (auth_url, resource, identity) in identity_entries {
        let (expires_ms, token) = match decrypt_token(identity.token).await {
            Ok(token_str) => {
                let expires =
                    crate::jwt::user_info_from_token(token_str.clone()).map_or(0, |i| i.expires);
                let token = if include_token {
                    token_str
                } else {
                    String::new()
                };
                (expires, token)
            }
            Err(_) => (0, String::new()),
        };
        result.push(StoredIdentityInfo {
            auth_url,
            resource,
            user_id: identity.user_id,
            acceptable_root_domains: identity.acceptable_root_domains,
            expires_ms,
            token,
        });
    }

    Ok(result)
}

/// Clear token map and token store file.
pub async fn reset_tokens() -> Result<(), TokenStoreError> {
    let token_map = token_map();
    let mut store = token_map.lock().await;
    let guard = lock_token_map().await?;
    store_token_map(&guard, &TokenMap::default())?;
    if store.is_some() {
        store.replace(TokenMap::default());
    }
    Ok(())
}

/// Open options for the store files. On Windows the share mode admits
/// concurrent readers but denies other writers for as long as the file is
/// open, excluding even processes that do not take the store lock.
fn store_open_options() -> fs::OpenOptions {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut options = fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    }
    options
}

/// Serializes store file access across lore processes via the `<file>.lock`
/// sidecar, released when the returned guard drops. Hold the guard across a
/// whole load -> modify -> store span (not just the individual file
/// operations) so concurrent processes cannot interleave their updates.
async fn lock_store_file(path: &Path) -> Result<FSLock, TokenStoreError> {
    FSLock::acquire_file_lock(path).await.map_err(|e| {
        lore_warn!("Failed to lock store file: {e}");
        TokenStoreError::internal_with_context(e, "Failed to lock store file")
    })
}

/// Cross-process guard for `tokenstore.toml`, creating the store directory so
/// the lock sidecar can be placed next to the file.
async fn lock_token_map() -> Result<FSLock, TokenStoreError> {
    lock_store_file(token_map_path(true)?.as_path()).await
}

/// Refreshes the in-memory token map from disk ahead of a mutation: another
/// process may have updated the file since it was cached. Callers hold the
/// store lock, so the reloaded state cannot change before it is written back.
fn reload_token_map(guard: &FSLock, store: &mut Option<TokenMap>) {
    if let Ok(loaded_map) = load_token_map(guard) {
        store.replace(loaded_map);
    }
}

/// Loads `tokenstore.toml`. The `_guard` parameter proves the caller holds the
/// cross-process store lock for the duration of the read.
fn load_token_map(_guard: &FSLock) -> Result<TokenMap, TokenStoreError> {
    let path = token_map_path(false)?;
    let mut options = store_open_options();
    options.read(true);
    let mut config_file = match options.open(path.as_path()) {
        Ok(file) => file,
        Err(err) => {
            lore_debug!("Failed to load token map file: {err}");
            return Err(TokenStoreError::internal_with_context(
                err,
                "Failed to load token map",
            ));
        }
    };

    let mut config = String::default();
    // Read via the guarded handle; `fs::read_to_string` would re-open the
    // file without the cross-process guard.
    #[allow(clippy::verbose_file_reads)]
    config_file.read_to_string(&mut config).map_err(|err| {
        lore_warn!("Failed to read token map file in {}: {err}", path.display());
        TokenStoreError::internal_with_context(err, "Failed to load token map")
    })?;

    let config = toml::from_str(config.as_str()).map_err(|err| {
        lore_warn!(
            "Failed to parse token map file in {}: {err}",
            path.display()
        );
        TokenStoreError::internal_with_context(err, "Failed to load token map")
    })?;
    lore_trace!("Loaded token map {config:?}");

    Ok(config)
}

/// Stores `tokenstore.toml`. The `_guard` parameter proves the caller holds the
/// cross-process store lock for the duration of the write.
fn store_token_map(_guard: &FSLock, token_map: &TokenMap) -> Result<(), TokenStoreError> {
    let path = token_map_path(true)?;

    lore_trace!("Store token map: {token_map:?}");
    let config_string = toml::to_string_pretty(token_map).map_err(|e| {
        lore_warn!("Failed to store token map: {e}");
        TokenStoreError::internal_with_context(e, "Failed to store token map")
    })?;

    let mut options = store_open_options();
    options.create(true).write(true);
    let mut config_file = match options.open(path.as_path()) {
        Ok(file) => file,
        Err(err) => {
            lore_debug!("Failed to store token map file: {err}");
            return Err(TokenStoreError::internal_with_context(
                err,
                "Failed to store token map",
            ));
        }
    };

    // Truncate only after the write guard is held, so a concurrent reader
    // can never observe a partially written file.
    config_file
        .set_len(0)
        .and_then(|()| config_file.write_all(config_string.as_bytes()))
        .map_err(|e| {
            lore_warn!("Failed to store token map: {e}");
            TokenStoreError::internal_with_context(e, "Failed to store token map")
        })
}

fn use_secure_store() -> bool {
    if let Ok(store) = std::env::var("LORE_AUTH_STORE") {
        store != "fallback"
    } else {
        true
    }
}

fn store_fallback_path(name: &str, create_dir: bool) -> Result<PathBuf, TokenStoreError> {
    let path = base_path(create_dir)?;
    Ok(path.join(format!("sec-{name}")))
}

static KEYRING_ENTRIES: OnceLock<RwLock<HashMap<String, Arc<keyring::Entry>>>> = OnceLock::new();

/// In-memory cache of the loaded encryption key.
///
/// The key is invariant for the lifetime of the secure-store entry, so it is
/// read once per process. Nonces are drawn at random per seal and travel with
/// the sealed token, so nothing here has to be written back.
static ENCRYPTION_CACHE: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();

fn encryption_cache() -> &'static Mutex<Option<Vec<u8>>> {
    ENCRYPTION_CACHE.get_or_init(|| Mutex::new(None))
}

const SECURE_STORE_MSG: &str =
    "Failed to store secret in secure storage, encryption key will be stored in plain text";

#[cfg(target_os = "macos")]
fn new_keyring_entry(target: &str) -> Result<keyring::Entry, TokenStoreError> {
    keyring::Entry::new_with_target("User", SERVICE_NAME, target).map_err(|e| {
        lore_warn!("{SECURE_STORE_MSG}: {e}");
        TokenStoreError::internal_with_context(e, SECURE_STORE_MSG)
    })
}

#[cfg(not(target_os = "macos"))]
fn new_keyring_entry(target: &str) -> Result<keyring::Entry, TokenStoreError> {
    keyring::Entry::new_with_target(target, SERVICE_NAME, "identity").map_err(|e| {
        lore_warn!("{SECURE_STORE_MSG}: {e}");
        TokenStoreError::internal_with_context(e, SECURE_STORE_MSG)
    })
}

/// Returns the (cached) keyring entry for `target`. Entries are cached per
/// target: a single cache slot would pin whichever target was requested first
/// and hand that entry back for every other target.
fn keyring_entry(target: &str) -> Result<Arc<keyring::Entry>, TokenStoreError> {
    let entries = KEYRING_ENTRIES.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(cache) = entries.read()
        && let Some(entry) = cache.get(target)
    {
        return Ok(Arc::clone(entry));
    }

    let entry = Arc::new(new_keyring_entry(target)?);
    if let Ok(mut cache) = entries.write() {
        cache.insert(target.to_string(), Arc::clone(&entry));
    }
    Ok(entry)
}

pub async fn store_user_token(
    auth_endpoint: &str,
    identity: &str,
    token: &str,
    mut acceptable_root_domains: Vec<String>,
) -> Result<(), TokenStoreError> {
    let auth_endpoint = auth_endpoint.trim_end_matches('/');

    // If we got the token from this endpoint it stands to reason we can
    // also send it back to that endpoint if we need to.
    // This is a work-around for Auth Service's issuer being just a keyword rather
    // than a domain
    let auth_domain = get_domain_or_empty(auth_endpoint);
    acceptable_root_domains.push(auth_domain);

    let encrypted_token = encrypt_token(token).await?;

    lore_trace!(
        "Store user {identity} token for auth endpoint {auth_endpoint} and audiences '{acceptable_root_domains:?}'"
    );

    let identity_token = IdentityToken {
        user_id: identity.to_string(),
        token: encrypted_token,
        acceptable_root_domains,
        refresh_token: None,
    };

    let token_map = token_map();
    let mut map_lock = token_map.lock().await;
    let guard = lock_token_map().await?;
    reload_token_map(&guard, &mut map_lock);
    if let Some(map) = map_lock.as_mut() {
        if let Some(remote) = map
            .remotes
            .iter_mut()
            .find(|entry| entry.remote == auth_endpoint)
        {
            if let Some(existing_index) = remote
                .token
                .iter()
                .position(|entry| entry.user_id == identity_token.user_id)
            {
                // Preserve existing refresh token when updating the auth token
                let existing_refresh = remote.token[existing_index].refresh_token.take();
                let mut new_token = identity_token;
                new_token.refresh_token = existing_refresh;
                remote.token[existing_index] = new_token;
                lore_trace!(
                    "Replace user {identity} token for auth_endpoint {auth_endpoint} in existing entry"
                );
            } else {
                lore_trace!(
                    "Store user {identity} token for auth_endpoint {auth_endpoint} in new identity entry"
                );
                remote.token.push(identity_token);
            }
        } else {
            lore_trace!(
                "Store user {identity} token for auth_endpoint {auth_endpoint} in new remote entry"
            );
            map.remotes.push(RemoteIdentity {
                remote: auth_endpoint.to_string(),
                token: vec![identity_token],
            });
        }
    } else {
        lore_trace!(
            "Store user {identity} token for auth_endpoint {auth_endpoint} in new entry in new token map"
        );
        let map = TokenMap {
            remotes: vec![RemoteIdentity {
                remote: auth_endpoint.to_string(),
                token: vec![identity_token],
            }],
        };
        *map_lock = Some(map);
    }

    if let Some(map) = map_lock.as_ref() {
        store_token_map(&guard, map)
    } else {
        lore_debug!("Unexpected, no token map to store to file");
        Err(TokenStoreError::internal("Failed to store token map"))
    }
}

/// Load the first suitable token for the given identity from the shared store
///
/// filter - You almost certainly want to filter out tokens that are invalid for the domain you want
/// to use them against. See comment at top of `urc-core::auth` - Check Token Recipient
pub async fn load_user_token_from_store<P>(
    auth_endpoint: &str,
    identity: &str,
    mut base_filter: P,
) -> Result<String, TokenStoreError>
where
    P: FnMut(&&IdentityToken) -> bool,
{
    let auth_endpoint = auth_endpoint.trim_end_matches('/');

    if auth_endpoint.is_empty() {
        lore_debug!("Load user token failed, no auth endpoint provided");
        return Err(TokenNotFound.into());
    }
    if identity.is_empty() {
        lore_debug!("Load user token failed, no identity");
        return Err(TokenNotFound.into());
    }
    lore_trace!("Load user {identity} token for auth_endpoint {auth_endpoint}");

    let encrypted_token = {
        let token_map = token_map();
        let mut store = token_map.lock().await;
        if store.is_none()
            && let Ok(guard) = lock_token_map().await
            && let Ok(loaded_map) = load_token_map(&guard)
        {
            store.replace(loaded_map);
        }
        if let Some(map) = store.as_ref()
            && let Some(remote) = map
                .remotes
                .iter()
                .find(|entry| entry.remote == auth_endpoint)
        {
            let token_filter =
                move |item: &&IdentityToken| base_filter(item) && item.user_id == identity;

            if let Some(token_identity) = remote.token.iter().find(token_filter) {
                lore_trace!(
                    "Found user {identity} token for auth_endpoint {auth_endpoint}, loading"
                );
                Some(token_identity.token.clone())
            } else {
                None
            }
        } else {
            None
        }
    };
    match encrypted_token {
        Some(token) => decrypt_token(token).await,
        None => Err(TokenNotFound.into()),
    }
}

/// Load the authentication token for `identity`, preferring one the caller
/// supplied over the shared store.
pub async fn load_user_token<P>(
    auth_endpoint: &str,
    identity: &str,
    base_filter: P,
    identity_token: &str,
    access_token: &str,
) -> Result<String, TokenStoreError>
where
    P: FnMut(&&IdentityToken) -> bool,
{
    if !identity_token.is_empty() {
        lore_debug!("Using the supplied identity token for {identity}");
        return Ok(identity_token.to_string());
    }

    if !access_token.is_empty() {
        lore_debug!(
            "Only an access token was supplied, not reading an authentication token from the store for {identity}"
        );
        return Err(TokenNotFound.into());
    }

    load_user_token_from_store(auth_endpoint, identity, base_filter).await
}

/// Returns true if `remote` is the base `auth_url` or a resource-scoped entry
/// under it (either new `"{auth_url}/{hex_id}"` or legacy `"{auth_url}/urc-*"` format).
fn is_entry_for_auth_url(remote: &str, auth_url: &str) -> bool {
    if remote == auth_url {
        return true;
    }
    if let Some(suffix) = remote
        .strip_prefix(auth_url)
        .and_then(|s| s.strip_prefix('/'))
    {
        // New format: 32-char hex repository ID
        if suffix.len() == 32 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return true;
        }
        // Legacy format: urc- prefix
        if suffix.starts_with("urc-") {
            return true;
        }
    }
    false
}

/// Remove a user's tokens from the given auth URL and all its resource-scoped entries.
///
/// Removes the identity from both the base `auth_url` entry (authentication token)
/// and all resource-scoped entries (authorization tokens), matching both new
/// `"{auth_url}/{repository_id}"` and legacy `"{auth_url}/urc-*"` key formats.
pub async fn remove_user_tokens_for_auth_url(
    auth_url: &str,
    identity: &str,
) -> Result<(), TokenStoreError> {
    let auth_url = auth_url.trim_end_matches('/');

    let token_map = token_map();
    let mut store = token_map.lock().await;
    let guard = lock_token_map().await?;
    reload_token_map(&guard, &mut store);

    let mut modified = false;

    if let Some(map) = store.as_mut() {
        let mut indices_to_process: Vec<usize> = map
            .remotes
            .iter()
            .enumerate()
            .filter(|(_, entry)| is_entry_for_auth_url(&entry.remote, auth_url))
            .map(|(i, _)| i)
            .collect();

        // Process in reverse to preserve indices during removal
        indices_to_process.reverse();

        for idx in indices_to_process {
            let before_len = map.remotes[idx].token.len();
            map.remotes[idx].token.retain(|t| t.user_id != identity);

            if map.remotes[idx].token.len() < before_len {
                lore_trace!(
                    "Removed token for endpoint {} identity {identity}",
                    map.remotes[idx].remote
                );
                modified = true;
            }

            if map.remotes[idx].token.is_empty() {
                lore_trace!(
                    "Removed empty remote entry for endpoint {}",
                    map.remotes[idx].remote
                );
                map.remotes.remove(idx);
            }
        }
    }

    if modified && let Some(store) = store.as_ref() {
        store_token_map(&guard, store)?;
    }

    Ok(())
}

/// Remove all tokens for the given auth URL and all its resource-scoped entries.
///
/// Removes all identities from both the base `auth_url` entry and all
/// resource-scoped entries (both new and legacy key formats).
pub async fn remove_all_tokens_for_auth_url(auth_url: &str) -> Result<(), TokenStoreError> {
    let auth_url = auth_url.trim_end_matches('/');

    let token_map = token_map();
    let mut store = token_map.lock().await;
    let guard = lock_token_map().await?;
    reload_token_map(&guard, &mut store);

    let mut modified = false;

    if let Some(map) = store.as_mut() {
        let before_len = map.remotes.len();

        map.remotes
            .retain(|entry| !is_entry_for_auth_url(&entry.remote, auth_url));

        if map.remotes.len() < before_len {
            lore_trace!("Removed all token entries for auth URL {auth_url}");
            modified = true;
        }
    }

    if modified && let Some(store) = store.as_ref() {
        store_token_map(&guard, store)?;
    }

    Ok(())
}

pub async fn remove_user_token(endpoint: &str, identity: &str) -> Result<(), TokenStoreError> {
    lore_trace!("Remove user {identity} token for auth_endpoint {endpoint}");

    let token_map = token_map();
    let mut store = token_map.lock().await;
    let guard = lock_token_map().await?;
    reload_token_map(&guard, &mut store);

    let mut modified = false;

    if let Some(map) = store.as_mut() {
        let endpoint = endpoint.to_string();
        if let Some(remote_index) = map
            .remotes
            .iter_mut()
            .position(|entry| entry.remote == endpoint)
        {
            let before_len = map.remotes[remote_index].token.len();

            map.remotes[remote_index]
                .token
                .retain(|token_identity| token_identity.user_id != identity);

            if map.remotes[remote_index].token.len() < before_len {
                lore_trace!("Removed token for endpoint {endpoint} identity {identity}");
                modified = true;
            }

            if map.remotes[remote_index].token.is_empty() {
                lore_trace!("Removed empty remote entry for endpoint {endpoint}");
                map.remotes.remove(remote_index);
            }
        }
    }

    if modified && let Some(store) = store.as_ref() {
        store_token_map(&guard, store)?;
    }

    Ok(())
}

pub async fn load_identities(auth_endpoint: &str) -> Result<Vec<String>, TokenStoreError> {
    lore_trace!("Load user identities for endpoint {auth_endpoint}");

    let mut identities = vec![];

    let token_map = token_map();
    let mut store = token_map.lock().await;
    if store.is_none()
        && let Ok(guard) = lock_token_map().await
        && let Ok(loaded_map) = load_token_map(&guard)
    {
        store.replace(loaded_map);
    }

    if let Some(map) = store.as_mut() {
        let auth_endpoint = auth_endpoint.to_string();
        if let Some(remote_index) = map
            .remotes
            .iter_mut()
            .position(|entry| entry.remote == auth_endpoint)
        {
            identities = map.remotes[remote_index]
                .token
                .iter()
                .map(|entry| entry.user_id.clone())
                .collect();

            lore_trace!("Loaded user identities for endpoint {auth_endpoint}: {identities:?}");
        }
    }

    Ok(identities)
}

/// Encrypts and stores (or replaces) the refresh token for an identity.
///
/// Called by orchestration after login or successful refresh. Overwrites
/// any existing refresh token atomically.
pub async fn store_refresh_token(
    auth_endpoint: &str,
    identity: &str,
    refresh_token: &str,
) -> Result<(), TokenStoreError> {
    let auth_endpoint = auth_endpoint.trim_end_matches('/');

    let encrypted_refresh = encrypt_token(refresh_token).await?;

    lore_trace!("Store refresh token for {identity} at {auth_endpoint}");

    let token_map = token_map();
    let mut map_lock = token_map.lock().await;
    let guard = lock_token_map().await?;
    reload_token_map(&guard, &mut map_lock);

    if let Some(map) = map_lock.as_mut()
        && let Some(remote) = map
            .remotes
            .iter_mut()
            .find(|entry| entry.remote == auth_endpoint)
        && let Some(token_entry) = remote
            .token
            .iter_mut()
            .find(|entry| entry.user_id == identity)
    {
        token_entry.refresh_token = Some(encrypted_refresh);
    } else {
        lore_debug!(
            "No identity entry found for {identity} at {auth_endpoint}, cannot store refresh token"
        );
        return Err(TokenNotFound.into());
    }

    if let Some(map) = map_lock.as_ref() {
        store_token_map(&guard, map)
    } else {
        Err(TokenStoreError::internal("Failed to store token map"))
    }
}

/// Loads and decrypts the refresh token for an identity.
///
/// Returns `TokenStoreError::TokenNotFound` if no refresh token is stored.
pub async fn load_refresh_token(
    auth_endpoint: &str,
    identity: &str,
) -> Result<String, TokenStoreError> {
    let auth_endpoint = auth_endpoint.trim_end_matches('/');

    lore_trace!("Load refresh token for {identity} at {auth_endpoint}");

    let encrypted_refresh = {
        let token_map = token_map();
        let mut store = token_map.lock().await;
        if store.is_none()
            && let Ok(guard) = lock_token_map().await
            && let Ok(loaded_map) = load_token_map(&guard)
        {
            store.replace(loaded_map);
        }

        if let Some(map) = store.as_ref()
            && let Some(remote) = map
                .remotes
                .iter()
                .find(|entry| entry.remote == auth_endpoint)
            && let Some(token_entry) = remote.token.iter().find(|entry| entry.user_id == identity)
            && let Some(ref encrypted) = token_entry.refresh_token
        {
            Some(encrypted.clone())
        } else {
            None
        }
    };
    match encrypted_refresh {
        Some(token) => decrypt_token(token).await,
        None => Err(TokenNotFound.into()),
    }
}

async fn encrypt_token(user_token: &str) -> Result<String, TokenStoreError> {
    lore_trace!("Encrypting user token");
    let key = get_token_encryption_key().await?;
    seal_token(&key, user_token)
}

async fn decrypt_token(token: String) -> Result<String, TokenStoreError> {
    lore_trace!("Decrypting user token");
    let key = get_token_encryption_key().await?;
    open_token(&key, &token)
}

/// Seals a token as `nonce || ciphertext || tag`, under a 96-bit nonce drawn
/// fresh for each call.
///
/// Nothing is written back to the secure store: a random nonce needs no
/// persisted counter, and rewriting the keyring entry on every encrypt made
/// each lore process re-authorize against the OS keychain (and, on macOS,
/// prompt once per binary and per rebuild).
fn seal_token(key: &[u8], user_token: &str) -> Result<String, TokenStoreError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce_bytes).map_err(|e| {
        lore_warn!("Failed to generate token nonce: {e}");
        TokenStoreError::internal_with_context(e, "Failed to encrypt user token")
    })?;

    let mut sealing_key =
        SealingKey::new(unbound_key(key)?, SingleNonceSequence(Some(nonce_bytes)));
    let mut sealed = user_token.as_bytes().to_vec();
    sealing_key
        .seal_in_place_append_tag(Aad::empty(), &mut sealed)
        .map_err(|e| {
            lore_warn!("Failed to encrypt user token: {e}");
            TokenStoreError::internal_with_context(e, "Failed to encrypt user token")
        })?;

    let mut blob = Vec::with_capacity(NONCE_LEN + sealed.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.append(&mut sealed);

    Ok(BASE64_STANDARD.encode(blob))
}

/// Opens a token sealed by [`seal_token`].
fn open_token(key: &[u8], token: &str) -> Result<String, TokenStoreError> {
    let blob = BASE64_STANDARD.decode(token).map_err(|e| {
        lore_warn!("Failed to decrypt user token: {e}");
        TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
    })?;

    let Some((nonce_bytes, sealed)) = blob.split_at_checked(NONCE_LEN) else {
        lore_warn!("Failed to decrypt user token: malformed token blob");
        return Err(TokenStoreError::internal("Failed to decrypt user token"));
    };
    let nonce = nonce_bytes.try_into().map_err(|e| {
        lore_warn!("Failed to decrypt user token: {e}");
        TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
    })?;

    let mut opening_key = OpeningKey::new(unbound_key(key)?, SingleNonceSequence(Some(nonce)));
    let decrypted = opening_key
        .open_in_place(Aad::empty(), &mut sealed.to_vec())
        .map_err(|e| {
            lore_warn!("Failed to decrypt user token: {e}");
            TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
        })?
        .to_vec();

    String::from_utf8(decrypted).map_err(|e| {
        lore_warn!("Failed to decrypt user token: {e}");
        TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
    })
}

/// Returns the cached encryption key, loading it from the secure store on
/// first use.
async fn get_token_encryption_key() -> Result<Vec<u8>, TokenStoreError> {
    let mut guard = encryption_cache().lock().await;
    if guard.is_none() {
        *guard = Some(load_or_init_encryption_key().await?);
    }
    Ok(guard.as_ref().expect("just initialized").clone())
}

/// Loads the encryption key from the secure store.
///
/// A new key is generated (which resets every stored token, since they can no
/// longer be opened) only when the store reports that no key is there, or that
/// what is there is unusable. A store that exists but cannot be read yields an
/// error instead: treating a denied or cancelled keychain prompt as "no key"
/// would rotate the key and log every other lore process out.
///
/// Callers must serialize this with respect to other writers — it is intended
/// to be invoked only while holding the [`ENCRYPTION_CACHE`] lock.
async fn load_or_init_encryption_key() -> Result<Vec<u8>, TokenStoreError> {
    let stored = get_secret_from_store(ENCRYPTION_KEY_TARGET).await?;
    if let Some(key) = stored.as_deref().and_then(encryption_key_from_stored) {
        return Ok(key);
    }

    lore_debug!(
        "Encryption key not found in secure store or fallback, generate new key and reset tokens"
    );

    let key = generate_encryption_key()?;
    reset_tokens().await?;
    set_secret_in_store(ENCRYPTION_KEY_TARGET, key.clone()).await?;

    Ok(key)
}

/// Checks that a secure-store blob is a key of the right size. Blobs written
/// by earlier versions carry a nonce counter ahead of the key and so fail
/// here, but they live under a different target and are never read.
fn encryption_key_from_stored(stored: &[u8]) -> Option<Vec<u8>> {
    (stored.len() == AES_256_GCM.key_len()).then(|| stored.to_vec())
}

/// Reports a secret the store does not hold.
///
/// A secure store that could not be read is not the same as one holding no
/// secret, so an earlier read failure surfaces here as an error: reporting
/// absence would let the caller replace a key that is merely out of reach, and
/// invalidate every token the real key still protects.
fn secret_absent(
    secure_store_error: Option<TokenStoreError>,
) -> Result<Option<Vec<u8>>, TokenStoreError> {
    match secure_store_error {
        Some(err) => Err(err),
        None => Ok(None),
    }
}

/// Reads a secret from the OS secure store, if there is a usable one.
///
/// `Ok(None)` means the store holds no such secret, or that no secure store is
/// available at all; `Err` carries the failure of a store that exists but
/// could not be read, for [`secret_absent`] to weigh.
async fn secret_from_secure_store(target: &str) -> Result<Option<Vec<u8>>, TokenStoreError> {
    if !use_secure_store() {
        return Ok(None);
    }
    let Ok(entry) = keyring_entry(target) else {
        return Ok(None);
    };

    // A locked keychain blocks until the user answers a prompt.
    let loaded = lore_base::lore_spawn_blocking!(move || entry.get_secret())
        .await
        .map_err(|e| TokenStoreError::internal_with_context(e, "Secure store read task failed"))?;

    match loaded {
        Ok(secret) => {
            lore_trace!("Loaded secret from secure store {target}");
            Ok(Some(secret))
        }
        Err(keyring::Error::NoEntry) => {
            lore_debug!("No secret in secure store {target}");
            Ok(None)
        }
        Err(err) => {
            lore_warn!("Failed to load secret from secure store {target}: {err}");
            Err(TokenStoreError::internal_with_context(
                err,
                "Failed to load secret from secure storage",
            ))
        }
    }
}

/// Reads a secret from the secure store, falling back to the on-disk copy.
///
/// `Ok(None)` means the secret is genuinely absent from both — see
/// [`secret_absent`] for how an unreadable secure store is distinguished from
/// an empty one.
async fn get_secret_from_store(target: &str) -> Result<Option<Vec<u8>>, TokenStoreError> {
    let secure_store_error = match secret_from_secure_store(target).await {
        Ok(Some(secret)) => return Ok(Some(secret)),
        Ok(None) => None,
        Err(err) => Some(err),
    };

    let path = store_fallback_path(target, false).map_err(|e| {
        lore_warn!("Failed to make fallback path: {e}");
        TokenStoreError::internal_with_context(e, "Failed to make fallback path")
    })?;
    if !path.exists() {
        return secret_absent(secure_store_error);
    }
    let _guard = lock_store_file(path.as_path()).await?;
    let mut options = store_open_options();
    options.read(true);
    let mut secret_file = match options.open(path.as_path()) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return secret_absent(secure_store_error);
        }
        Err(err) => {
            lore_warn!("Failed to read secret from fallback path: {err}");
            return Err(TokenStoreError::internal_with_context(
                err,
                "Failed to read secret from fallback path",
            ));
        }
    };
    lore_trace!(
        "Loaded secret from insecure fallback path {}",
        path.display()
    );

    let mut secret = Vec::default();
    // Read via the guarded handle; `fs::read` would re-open the file
    // without the cross-process guard.
    #[allow(clippy::verbose_file_reads)]
    secret_file.read_to_end(&mut secret).map_err(|e| {
        lore_warn!("Failed to read secret from fallback path: {e}");
        TokenStoreError::internal_with_context(e, "Failed to read secret from fallback path")
    })?;
    Ok(Some(secret))
}

async fn set_secret_in_store(target: &str, secret: Vec<u8>) -> Result<(), TokenStoreError> {
    if use_secure_store()
        && let Ok(entry) = keyring_entry(target)
    {
        let stored = {
            let secret = secret.clone();
            lore_base::lore_spawn_blocking!(move || entry.set_secret(&secret))
                .await
                .map_err(|e| {
                    TokenStoreError::internal_with_context(e, "Secure store write task failed")
                })?
        };
        if stored
            .map_err(|e| {
                lore_warn!("{SECURE_STORE_MSG}: {e}");
                TokenStoreError::internal_with_context(e, SECURE_STORE_MSG)
            })
            .is_ok()
        {
            lore_trace!("Stored secret in secure store {target}");
            return Ok(());
        }
        // SAFETY: `set_var` races with concurrent environment access in other
        // threads. Redirecting subsequent reads to the fallback file is worth
        // that risk here: the alternative is silently losing the secret.
        unsafe {
            std::env::set_var("LORE_AUTH_STORE", "fallback");
        }
    }

    let path = store_fallback_path(target, true).map_err(|e| {
        lore_warn!("Failed to make fallback path: {e}");
        TokenStoreError::internal_with_context(e, "Failed to make fallback path")
    })?;
    let _guard = lock_store_file(path.as_path()).await?;
    let mut options = store_open_options();
    options.create(true).write(true);
    let mut secret_file = options.open(path.as_path()).map_err(|e| {
        lore_warn!("Failed to write secret to fallback path: {e}");
        TokenStoreError::internal_with_context(e, "Failed to write secret to fallback path")
    })?;
    secret_file
        .set_len(0)
        .and_then(|()| secret_file.write_all(&secret))
        .map_err(|e| {
            lore_warn!("Failed to write secret to fallback path: {e}");
            TokenStoreError::internal_with_context(e, "Failed to write secret to fallback path")
        })?;
    lore_trace!("Stored secret in insecure fallback path {}", path.display());
    Ok(())
}

/// Draws a new key. A failure here is propagated rather than ignored: the
/// buffer starts zeroed and is the right length either way, so a discarded
/// error would hand back an all-zero key that every later check accepts.
fn generate_encryption_key() -> Result<Vec<u8>, TokenStoreError> {
    let mut key_bytes = vec![0; AES_256_GCM.key_len()];
    SystemRandom::new().fill(&mut key_bytes).map_err(|e| {
        lore_warn!("Failed to generate encryption key: {e}");
        TokenStoreError::internal_with_context(e, "Failed to generate encryption key")
    })?;
    lore_debug!("Generated new encryption key");
    Ok(key_bytes)
}

fn unbound_key(key: &[u8]) -> Result<UnboundKey, TokenStoreError> {
    UnboundKey::new(&AES_256_GCM, key).map_err(|e| {
        lore_warn!("Failed to create unbound key: {e}");
        TokenStoreError::internal_with_context(e, "Failed to create unbound key")
    })
}

/// Yields one caller-supplied nonce and refuses to advance again, so a key
/// built from it can never seal or open twice under the same nonce.
struct SingleNonceSequence(Option<[u8; NONCE_LEN]>);
impl NonceSequence for SingleNonceSequence {
    fn advance(&mut self) -> Result<Nonce, Unspecified> {
        self.0
            .take()
            .map(Nonce::assume_unique_for_key)
            .ok_or(Unspecified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_token_serde_default_none() {
        // Old store format without refresh_token field
        let toml_str = r#"
user_id = "user-1"
token = "encrypted-token"
acceptable_root_domains = ["example.com"]
"#;
        let token: IdentityToken = toml::from_str(toml_str).unwrap();
        assert!(token.refresh_token.is_none());
        assert_eq!(token.user_id, "user-1");
        assert_eq!(token.token, "encrypted-token");
    }

    #[test]
    fn refresh_token_serde_roundtrip() {
        let token = IdentityToken {
            user_id: "user-1".into(),
            token: "encrypted-auth".into(),
            acceptable_root_domains: vec!["example.com".into()],
            refresh_token: Some("encrypted-refresh".into()),
        };
        let serialized = toml::to_string_pretty(&token).unwrap();
        let deserialized: IdentityToken = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.refresh_token.as_deref(),
            Some("encrypted-refresh")
        );
        assert_eq!(deserialized.user_id, "user-1");
    }

    #[test]
    fn identity_token_without_refresh_backward_compat() {
        // Simulates an old store file structure
        let toml_str = r#"
[[remotes]]
remote = "https://auth.example.com"

[[remotes.token]]
user_id = "alice"
token = "tok-a"
acceptable_root_domains = ["example.com"]

[[remotes.token]]
user_id = "bob"
token = "tok-b"
"#;
        let map: TokenMap = toml::from_str(toml_str).unwrap();
        assert_eq!(map.remotes.len(), 1);
        assert_eq!(map.remotes[0].token.len(), 2);
        assert!(map.remotes[0].token[0].refresh_token.is_none());
        assert!(map.remotes[0].token[1].refresh_token.is_none());
    }

    #[test]
    fn token_map_with_refresh_token_roundtrip() {
        let map = TokenMap {
            remotes: vec![RemoteIdentity {
                remote: "https://auth.example.com".into(),
                token: vec![IdentityToken {
                    user_id: "alice".into(),
                    token: "auth-tok".into(),
                    acceptable_root_domains: vec!["example.com".into()],
                    refresh_token: Some("refresh-tok".into()),
                }],
            }],
        };
        let serialized = toml::to_string_pretty(&map).unwrap();
        let deserialized: TokenMap = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.remotes[0].token[0].refresh_token.as_deref(),
            Some("refresh-tok")
        );
    }

    #[test]
    fn encryption_key_from_stored_accepts_a_bare_key() {
        let key = generate_encryption_key().unwrap();
        assert_eq!(key.len(), AES_256_GCM.key_len());
        assert_eq!(encryption_key_from_stored(&key).as_ref(), Some(&key));
    }

    #[test]
    fn encryption_key_from_stored_rejects_malformed() {
        assert!(encryption_key_from_stored(&[]).is_none());
        assert!(encryption_key_from_stored(&[0u8; 16]).is_none());
        // The layout earlier versions wrote: a nonce counter ahead of the key.
        assert!(encryption_key_from_stored(&[0u8; 4 + 32]).is_none());
    }

    #[test]
    fn seal_and_open_token_round_trip() {
        let key = generate_encryption_key().unwrap();
        let sealed = seal_token(&key, "a-user-token").unwrap();
        assert_eq!(open_token(&key, &sealed).unwrap(), "a-user-token");
    }

    #[test]
    fn seal_token_prefixes_the_nonce() {
        let key = generate_encryption_key().unwrap();
        let blob = BASE64_STANDARD
            .decode(seal_token(&key, "a-user-token").unwrap())
            .unwrap();
        assert_eq!(blob.len(), NONCE_LEN + "a-user-token".len() + 16);
    }

    #[test]
    fn seal_token_draws_a_fresh_nonce_each_time() {
        let key = generate_encryption_key().unwrap();
        let first = seal_token(&key, "a-user-token").unwrap();
        let second = seal_token(&key, "a-user-token").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn open_token_rejects_short_blobs() {
        let key = generate_encryption_key().unwrap();
        let short = BASE64_STANDARD.encode([0u8; NONCE_LEN - 1]);
        assert!(open_token(&key, &short).is_err());
    }

    #[test]
    fn open_token_rejects_a_foreign_key() {
        let sealed = seal_token(&generate_encryption_key().unwrap(), "a-user-token").unwrap();
        assert!(open_token(&generate_encryption_key().unwrap(), &sealed).is_err());
    }

    #[test]
    fn single_nonce_sequence_refuses_second_advance() {
        let mut sequence = SingleNonceSequence(Some([0u8; NONCE_LEN]));
        assert!(sequence.advance().is_ok());
        assert!(sequence.advance().is_err());
    }

    #[test]
    fn split_remote_resource_new_format() {
        let (auth, resource) =
            split_remote_resource("https://auth.example.com/00112233445566778899aabbccddeeff");
        assert_eq!(auth, "https://auth.example.com");
        assert_eq!(resource, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn split_remote_resource_legacy_format() {
        let (auth, resource) =
            split_remote_resource("https://auth.example.com/urc-00112233445566778899aabbccddeeff");
        assert_eq!(auth, "https://auth.example.com");
        assert_eq!(resource, "urc-00112233445566778899aabbccddeeff");
    }

    #[test]
    fn split_remote_resource_no_resource() {
        let (auth, resource) = split_remote_resource("https://auth.example.com");
        assert_eq!(auth, "https://auth.example.com");
        assert!(resource.is_empty());
    }

    #[test]
    fn split_remote_resource_scheme_with_hostname() {
        let (auth, resource) =
            split_remote_resource("ucs-auth://auth.example.com/aabbccdd00112233aabbccdd00112233");
        assert_eq!(auth, "ucs-auth://auth.example.com");
        assert_eq!(resource, "aabbccdd00112233aabbccdd00112233");
    }

    #[test]
    fn is_entry_for_auth_url_base() {
        assert!(is_entry_for_auth_url(
            "https://auth.example.com",
            "https://auth.example.com"
        ));
    }

    #[test]
    fn is_entry_for_auth_url_new_format() {
        assert!(is_entry_for_auth_url(
            "https://auth.example.com/00112233445566778899aabbccddeeff",
            "https://auth.example.com"
        ));
    }

    #[test]
    fn is_entry_for_auth_url_legacy_format() {
        assert!(is_entry_for_auth_url(
            "https://auth.example.com/urc-00112233445566778899aabbccddeeff",
            "https://auth.example.com"
        ));
    }

    #[test]
    fn is_entry_for_auth_url_different_host() {
        assert!(!is_entry_for_auth_url(
            "https://other.example.com/00112233445566778899aabbccddeeff",
            "https://auth.example.com"
        ));
    }

    #[test]
    fn is_entry_for_auth_url_non_hex_suffix() {
        assert!(!is_entry_for_auth_url(
            "https://auth.example.com/not-a-resource",
            "https://auth.example.com"
        ));
    }

    /// A supplied token is passed through untouched, so it need not be a JWT.
    const SUPPLIED_TOKEN: &str = "supplied-authentication-token";

    #[tokio::test]
    async fn supplied_identity_token_is_used_without_the_store() {
        // No auth endpoint and no store entry: the supplied token is returned on
        // its own, where a store read would report TokenNotFound.
        let token = load_user_token(
            "",
            "alice",
            tokens_only_for_recipient_domain("nowhere.example".to_string()),
            SUPPLIED_TOKEN,
            "",
        )
        .await
        .expect("the supplied token is used as given");
        assert_eq!(token, SUPPLIED_TOKEN);

        // An access token alongside it changes nothing: the identity token is
        // still the authentication token to use.
        let token = load_user_token(
            "",
            "alice",
            tokens_only_for_recipient_domain("nowhere.example".to_string()),
            SUPPLIED_TOKEN,
            "supplied-access-token",
        )
        .await
        .expect("the supplied token is used as given");
        assert_eq!(token, SUPPLIED_TOKEN);
    }

    #[tokio::test]
    async fn no_supplied_token_reads_the_store() {
        // Nothing supplied, so this is a plain store read, which has no entry
        // for an empty endpoint.
        let result = load_user_token(
            "",
            "alice",
            tokens_only_for_recipient_domain("nowhere.example".to_string()),
            "",
            "",
        )
        .await;
        assert!(result.is_err());
    }
}
