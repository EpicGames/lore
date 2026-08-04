// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str;
use std::sync::Arc;
use std::sync::OnceLock;

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
use zerocopy::IntoBytes;

use crate::jwt::domain_in_root_domains;
use crate::util::get_domain_or_empty;

const TAG_LEN: usize = 16;
const NONCE_SIZE_U32: usize = 4;
const ENCRYPTION_KEY_TARGET: &str = "lore_encryption_key";

#[error_set]
pub enum TokenStoreError {
    TokenNotFound,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Encryption {
    key: Vec<u8>,
    nonce: u32,
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

/// Base directory holding the auth store files (`tokens.toml` and the
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

fn token_map_path(create_dir: bool) -> Result<PathBuf, TokenStoreError> {
    let path = base_path(create_dir)?;
    Ok(path.join("tokens.toml"))
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

/// Clear token map and tokens.toml file.
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
    let path = path.to_path_buf();
    lore_base::lore_spawn_blocking!(move || FSLock::acquire_file_lock(path))
        .await
        .map_err(|e| TokenStoreError::internal_with_context(e, "Failed to lock store file"))?
        .map_err(|e| {
            lore_warn!("Failed to lock store file: {e}");
            TokenStoreError::internal_with_context(e, "Failed to lock store file")
        })
}

/// Cross-process guard for `tokens.toml`, creating the store directory so
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

/// Loads `tokens.toml`. The `_guard` parameter proves the caller holds the
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

/// Stores `tokens.toml`. The `_guard` parameter proves the caller holds the
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

static KEYRING_ENTRY: OnceLock<Option<Arc<keyring::Entry>>> = OnceLock::new();

/// In-memory cache of the loaded encryption key + next-use nonce counter.
///
/// The encryption key is invariant for the lifetime of the secure-store
/// entry; only the nonce advances on each encrypt. Caching avoids hitting
/// the OS keyring on every encrypt/decrypt and serializes the encrypt path
/// so two concurrent encrypts cannot reserve the same nonce (AES-GCM nonce
/// reuse is a key-recovery vulnerability).
static ENCRYPTION_CACHE: OnceLock<Mutex<Option<Encryption>>> = OnceLock::new();

fn encryption_cache() -> &'static Mutex<Option<Encryption>> {
    ENCRYPTION_CACHE.get_or_init(|| Mutex::new(None))
}

const SECURE_STORE_MSG: &str =
    "Failed to store secret in secure storage, encryption key will be stored in plain text";

#[cfg(target_os = "macos")]
fn new_keyring_entry(target: &str) -> Result<keyring::Entry, TokenStoreError> {
    keyring::Entry::new_with_target("User", "com.epicgames.urc", target).map_err(|e| {
        lore_warn!("{SECURE_STORE_MSG}: {e}");
        TokenStoreError::internal_with_context(e, SECURE_STORE_MSG)
    })
}

#[cfg(not(target_os = "macos"))]
fn new_keyring_entry(target: &str) -> Result<keyring::Entry, TokenStoreError> {
    keyring::Entry::new_with_target(target, "com.epicgames.urc", "identity").map_err(|e| {
        lore_warn!("{SECURE_STORE_MSG}: {e}");
        TokenStoreError::internal_with_context(e, SECURE_STORE_MSG)
    })
}

fn keyring_entry(target: &str) -> Result<Arc<keyring::Entry>, TokenStoreError> {
    KEYRING_ENTRY
        .get_or_init(|| new_keyring_entry(target).ok().map(Arc::new))
        .as_ref()
        .ok_or_else(|| TokenStoreError::internal(SECURE_STORE_MSG))
        .map(Arc::clone)
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
pub async fn load_user_token<P>(
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

    // Hold the cache lock across read -> reserve nonce -> persist -> update,
    // so concurrent encrypts cannot seal two blobs with the same nonce.
    let mut guard = encryption_cache().lock().await;
    if guard.is_none() {
        *guard = Some(load_or_init_encryption().await?);
    }
    let encryption = guard.as_ref().expect("just initialized").clone();
    let new_nonce = encryption.nonce + 1;
    // Persist before updating the cache: a failed write leaves the cache at
    // the old nonce so the next attempt retries with the same value, rather
    // than skipping ahead and risking nonce reuse on a later success.
    set_secret_in_store(
        ENCRYPTION_KEY_TARGET,
        get_encryption_key_with_nonce(encryption.key.clone(), new_nonce),
    )
    .await?;
    *guard = Some(Encryption {
        key: encryption.key.clone(),
        nonce: new_nonce,
    });
    drop(guard);

    let mut sealing_key = generate_sealing_key(encryption.clone())?;
    let mut encrypted_token = user_token.as_bytes().to_vec();
    encrypted_token.extend_from_slice(&[0u8; TAG_LEN]);

    sealing_key
        .seal_in_place_append_tag(Aad::empty(), &mut encrypted_token)
        .map_err(|e| {
            lore_warn!("Failed to encrypt user token: {e}");
            TokenStoreError::internal_with_context(e, "Failed to encrypt user token")
        })?;

    // Add nonce to front of encoded token.
    let mut encrypted_token_with_nonce = encryption.nonce.as_bytes().to_vec();
    encrypted_token_with_nonce.append(&mut encrypted_token);

    // Encode to base 64 for cleaner storage.
    Ok(BASE64_STANDARD.encode(encrypted_token_with_nonce))
}

async fn decrypt_token(token: String) -> Result<String, TokenStoreError> {
    lore_trace!("Decrypting user token");
    let encryption = get_token_encryption_key().await?;

    // Decode the base 64 value before decrypting aes.
    let encrypted_token_with_nonce = BASE64_STANDARD.decode(token).map_err(|e| {
        lore_warn!("Failed to decrypt user token: {e}");
        TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
    })?;

    // Get nonce from front of encoded token and use that to generate opening key.
    let (nonce_bytes, encrypted_token) = encrypted_token_with_nonce.split_at(NONCE_SIZE_U32);
    let nonce: [u8; NONCE_SIZE_U32] = nonce_bytes.try_into().map_err(|e| {
        lore_warn!("Failed to decrypt user token: {e}");
        TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
    })?;
    let nonce_val = u32::from_le_bytes(nonce);

    let mut opening_key = generate_opening_key(Encryption {
        key: encryption.key,
        nonce: nonce_val,
    })?;

    let mut decrypted_token = opening_key
        .open_in_place(Aad::empty(), &mut encrypted_token.to_vec())
        .map_err(|e| {
            lore_warn!("Failed to decrypt user token: {e}");
            TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
        })?
        .to_vec();

    // Truncate the empty values that are due to the in place tag usage.
    if decrypted_token.len() >= TAG_LEN {
        decrypted_token.truncate(decrypted_token.len() - TAG_LEN);
    }

    String::from_utf8(decrypted_token).map_err(|e| {
        lore_warn!("Failed to decrypt user token: {e}");
        TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
    })
}

async fn get_token_encryption_key() -> Result<Encryption, TokenStoreError> {
    // Decrypt-side accessor: returns the cached key (loading from the secure
    // store on first use). Decrypt does not mutate the nonce, so holding
    // the lock briefly to clone is enough — concurrent decrypts run in
    // parallel after the first load.
    let mut guard = encryption_cache().lock().await;
    if guard.is_none() {
        *guard = Some(load_or_init_encryption().await?);
    }
    Ok(guard.as_ref().expect("just initialized").clone())
}

/// Loads the encryption key from the secure store, generating and persisting
/// a new one (and resetting any existing tokens) if no key is stored.
/// Callers must serialize this with respect to other writers — it is intended
/// to be invoked only while holding the [`ENCRYPTION_CACHE`] lock.
async fn load_or_init_encryption() -> Result<Encryption, TokenStoreError> {
    let encryption_key_nonce = get_secret_from_store(ENCRYPTION_KEY_TARGET).await?;
    if let Ok(encryption) = get_encryption(encryption_key_nonce) {
        return Ok(encryption);
    }

    lore_debug!(
        "Encryption key not found in secure store or fallback, generate new key and reset tokens"
    );

    let encryption_key_nonce = generate_encryption_key_nonce();
    reset_tokens().await?;

    // Set encryption key nonce.
    set_secret_in_store(ENCRYPTION_KEY_TARGET, encryption_key_nonce.clone()).await?;

    get_encryption(encryption_key_nonce)
}

fn get_encryption(encryption_key_nonce: Vec<u8>) -> Result<Encryption, TokenStoreError> {
    if encryption_key_nonce.len() > NONCE_SIZE_U32 {
        let (nonce_bytes, encryption_key_bytes) = encryption_key_nonce.split_at(NONCE_SIZE_U32);
        let nonce: [u8; NONCE_SIZE_U32] = nonce_bytes.try_into().map_err(|e| {
            lore_warn!("Failed to decrypt user token: {e}");
            TokenStoreError::internal_with_context(e, "Failed to decrypt user token")
        })?;
        let nonce_val = u32::from_le_bytes(nonce);
        Ok(Encryption {
            key: encryption_key_bytes.to_vec(),
            nonce: nonce_val,
        })
    } else {
        Err(TokenStoreError::internal("Failed to decrypt user token"))
    }
}

async fn get_secret_from_store(target: &str) -> Result<Vec<u8>, TokenStoreError> {
    if use_secure_store()
        && let Ok(entry) = keyring_entry(target)
    {
        // A locked keychain blocks until the user answers a prompt.
        let loaded = lore_base::lore_spawn_blocking!(move || entry.get_secret())
            .await
            .map_err(|e| {
                TokenStoreError::internal_with_context(e, "Secure store read task failed")
            })?;
        match loaded {
            Ok(secret) => {
                lore_trace!("Loaded secret from secure store {target}");
                return Ok(secret);
            }
            Err(err) => {
                lore_debug!("Failed to load secret from secure store {target}: {err}");
            }
        }
    }

    let path = store_fallback_path(target, false).map_err(|e| {
        lore_warn!("Failed to make fallback path: {e}");
        TokenStoreError::internal_with_context(e, "Failed to make fallback path")
    })?;
    if !path.exists() {
        return Ok(Vec::default());
    }
    let _guard = lock_store_file(path.as_path()).await?;
    let mut options = store_open_options();
    options.read(true);
    let mut secret_file = match options.open(path.as_path()) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::default());
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
    Ok(secret)
}

async fn set_secret_in_store(target: &str, secret: Vec<u8>) -> Result<Vec<u8>, TokenStoreError> {
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
            return Ok(secret);
        }
        // If we fallback to disk storage, ensure further get calls use this
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
    Ok(secret)
}

fn generate_encryption_key_nonce() -> Vec<u8> {
    let rand = SystemRandom::new();
    let mut key_bytes = vec![0; AES_256_GCM.key_len()];
    let _ = rand.fill(&mut key_bytes);
    lore_debug!("Generated new encryption key");
    get_encryption_key_with_nonce(key_bytes, 1)
}

fn get_encryption_key_with_nonce(key: Vec<u8>, nonce: u32) -> Vec<u8> {
    let mut encryption_key_with_nonce = nonce.as_bytes().to_vec();
    encryption_key_with_nonce.append(&mut key.clone());
    encryption_key_with_nonce
}

fn generate_sealing_key(
    encryption: Encryption,
) -> Result<SealingKey<CounterNonceSequence>, TokenStoreError> {
    let unbound_key = UnboundKey::new(&AES_256_GCM, &encryption.key).map_err(|e| {
        lore_warn!("Failed to create unbound key: {e}");
        TokenStoreError::internal_with_context(e, "Failed to create unbound key")
    })?;
    let nonce_sequence = CounterNonceSequence(encryption.nonce);
    let sealing_key = SealingKey::new(unbound_key, nonce_sequence);
    Ok(sealing_key)
}

fn generate_opening_key(
    encryption: Encryption,
) -> Result<OpeningKey<CounterNonceSequence>, TokenStoreError> {
    let unbound_key = UnboundKey::new(&AES_256_GCM, &encryption.key).map_err(|e| {
        lore_warn!("Failed to create unbound key: {e}");
        TokenStoreError::internal_with_context(e, "Failed to create unbound key")
    })?;
    let nonce_sequence = CounterNonceSequence(encryption.nonce);
    let opening_key = OpeningKey::new(unbound_key, nonce_sequence);
    Ok(opening_key)
}

struct CounterNonceSequence(u32);
impl NonceSequence for CounterNonceSequence {
    fn advance(&mut self) -> Result<Nonce, Unspecified> {
        let mut nonce_bytes = vec![0; NONCE_LEN];

        let bytes = self.0.to_be_bytes();
        nonce_bytes[8..].copy_from_slice(&bytes);

        Nonce::try_assume_unique_for_key(&nonce_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_token_serde_default_none() {
        // Old tokens.toml format without refresh_token field
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
        // Simulates an old tokens.toml file structure
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
}
