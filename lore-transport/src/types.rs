// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use bytes::Bytes;
use lore_base::types::*;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Environment types
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct EnvironmentConfig {
    pub endpoint: Option<Endpoint>,
    pub config: Option<EnvironmentServerConfig>,
    pub oidc: Option<Oidc>,
}

impl EnvironmentConfig {
    pub fn max_query_batch(&self) -> Option<usize> {
        self.config.as_ref().and_then(|c| c.max_query_batch)
    }

    /// The compression mode the server states it prefers, as the number it sent. The codec that
    /// number names is `lore_storage::CompressionMode`, which this crate does not depend on.
    pub fn compression_mode(&self) -> Option<u32> {
        self.config
            .as_ref()
            .and_then(|config| config.compression_mode.as_ref())
            .map(ServerCompressionMode::as_u32)
    }

    /// Per-service endpoint URL. If the environment's `endpoint.storage_url`
    /// is set and non-empty, it overrides `fallback`; otherwise `fallback` is
    /// returned unchanged. Same contract for the other `*_url` methods below.
    pub fn storage_url<'a>(&'a self, fallback: &'a str) -> &'a str {
        service_url_or(
            self.endpoint
                .as_ref()
                .and_then(|e| e.storage_url.as_deref()),
            fallback,
        )
    }

    pub fn revision_url<'a>(&'a self, fallback: &'a str) -> &'a str {
        service_url_or(
            self.endpoint
                .as_ref()
                .and_then(|e| e.revision_url.as_deref()),
            fallback,
        )
    }

    pub fn lock_url<'a>(&'a self, fallback: &'a str) -> &'a str {
        service_url_or(
            self.endpoint.as_ref().and_then(|e| e.lock_url.as_deref()),
            fallback,
        )
    }

    pub fn repository_url<'a>(&'a self, fallback: &'a str) -> &'a str {
        service_url_or(
            self.endpoint
                .as_ref()
                .and_then(|e| e.repository_url.as_deref()),
            fallback,
        )
    }

    pub fn notification_url<'a>(&'a self, fallback: &'a str) -> &'a str {
        service_url_or(
            self.endpoint
                .as_ref()
                .and_then(|e| e.notification_url.as_deref()),
            fallback,
        )
    }

    /// User directory endpoint: resolves user IDs to display names and back.
    /// Falls back to `auth_url` if empty.
    pub fn user_url<'a>(&'a self, fallback: &'a str) -> &'a str {
        service_url_or(
            self.endpoint.as_ref().and_then(|e| e.user_url.as_deref()),
            fallback,
        )
    }
}

fn service_url_or<'a>(override_url: Option<&'a str>, fallback: &'a str) -> &'a str {
    match override_url {
        Some(url) if !url.is_empty() => url,
        _ => fallback,
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Endpoint {
    pub auth_url: Option<String>,
    pub repository_url: Option<String>,
    pub storage_url: Option<String>,
    pub revision_url: Option<String>,
    pub lock_url: Option<String>,
    pub notification_url: Option<String>,
    /// User directory endpoint: resolves user IDs to display names and back.
    /// Falls back to `auth_url` if empty.
    pub user_url: Option<String>,
}

/// The OIDC provider a server advertises.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Oidc {
    /// Issuer URL. The provider's endpoints come from its discovery document,
    /// `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// The public client ID presented to the provider.
    pub client_id: String,
    /// Default scopes to request at login. Empty leaves the choice to the client.
    pub scopes: Vec<String>,
    /// Whether a client takes the OIDC path by default rather than `auth_url`.
    pub preferred: bool,
    /// Maps a partition to an RFC 8707 resource, `{id}` standing for the partition ID.
    pub resource_template: Option<String>,
    /// Maps a partition to a scope value, `{id}` standing for the partition ID.
    pub scope_template: Option<String>,
    /// The issuer of the RFC 8693 token-exchange endpoint that mints partition-scoped tokens.
    pub token_exchange_issuer: Option<String>,
    pub identity_claim: Option<String>,
}

impl Oidc {
    pub const DEFAULT_IDENTITY_CLAIM: &'static str = "sub";

    /// The claim recorded as the user identity: the advertised one, or `sub` when the server
    /// names none.
    pub fn identity_claim(&self) -> &str {
        match self.identity_claim.as_deref() {
            Some(claim) if !claim.is_empty() => claim,
            _ => Self::DEFAULT_IDENTITY_CLAIM,
        }
    }
}

/// A compression mode as it arrives from a server, held as the number it was sent as: the codec
/// it names is `lore_storage::CompressionMode`, which this crate does not depend on.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct ServerCompressionMode(u32);

impl ServerCompressionMode {
    pub fn from_u32(value: u32) -> Self {
        ServerCompressionMode(value)
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct EnvironmentServerConfig {
    pub max_query_batch: Option<usize>,
    pub compression_mode: Option<ServerCompressionMode>,
}

// ---------------------------------------------------------------------------
// Protocol response types
// ---------------------------------------------------------------------------

pub struct BranchPushResponse {
    /// True if the server performed a fast-forward merge
    pub fast_forward_merged: bool,
    /// New branch latest revision identifier
    pub revision: Hash,
    /// Revision number of new branch latest revision
    pub revision_number: u64,
    /// Optional message from the server
    pub message: Option<String>,
}

pub struct BranchQueryResponse {
    /// Branch ID
    pub id: BranchId,
    /// Latest revision
    pub latest: Hash,
    /// Metadata hash
    pub metadata: Hash,
    /// Whether the branch has been deleted (name->id mapping removed)
    pub deleted: bool,
}

pub struct BranchListResponse {
    /// Branch list
    pub list: Vec<BranchMetadata>,
}

pub struct RevisionListResponse {
    pub items: Vec<RevisionItem>,
    pub next_revision: Hash,
    pub previous_revision: Hash,
}

#[derive(Debug)]
pub struct RevisionItem {
    pub number: u64,
    pub signature: Hash,
    pub metadata: Hash,
    pub state: Bytes,
}

#[derive(Clone)]
pub enum RevisionListStart {
    Identifier(RevisionListIdentifier),
    Signature(Hash),
}

#[derive(Clone)]
pub struct RevisionListIdentifier {
    pub branch: BranchId,
    pub number: u64,
}

impl From<RevisionListIdentifier> for RevisionListStart {
    fn from(value: RevisionListIdentifier) -> Self {
        RevisionListStart::Identifier(value)
    }
}

impl From<Hash> for RevisionListStart {
    fn from(value: Hash) -> Self {
        RevisionListStart::Signature(value)
    }
}

#[derive(Default, Debug, Clone)]
pub struct RepositoryData {
    pub id: RepositoryId,
    pub name: String,
    pub metadata: Hash,
}

/// Result of a repository metadata compare-and-swap operation
#[derive(Default, Debug, Clone)]
pub struct MetadataSetResult {
    pub success: bool,
    pub current_hash: Hash,
}

// ---------------------------------------------------------------------------
// Authentication types
// ---------------------------------------------------------------------------

/// Result of an interactive login session initiation.
#[derive(Clone, Debug)]
pub struct AuthSession {
    /// Opaque session identifier for polling. The device grant's
    /// `device_code`.
    pub session_code: String,
    /// URL the user should visit to authenticate. The device grant's
    /// `verification_uri_complete`.
    pub login_url: String,
    /// Code the user types at `login_url` from another machine. Empty when
    /// the backend embeds it in `login_url` and offers no other entry.
    pub user_code: String,
    /// Minimum time between two polls of the session.
    pub interval: Duration,
    /// How long the session stays open for approval, counted from when it
    /// was started.
    pub expires_in: Duration,
}

#[derive(Clone, Debug)]
pub enum AuthSessionPoll {
    /// The user has not approved the login yet. Poll again after the
    /// session's `interval`.
    Pending,
    /// The user has not approved the login yet, and the backend wants a
    /// longer gap before the next poll than the session's `interval`.
    SlowDown,
    /// The user approved the login and the backend issued a token.
    Complete(AuthenticationToken),
}

/// Authentication token with user identity metadata.
///
/// Returned from login flows (interactive, token exchange, refresh).
/// This is the protocol-layer type -- transient, in-memory. The orchestration
/// layer converts it to `SerializedToken` (the token store's on-disk format)
/// when persisting to `tokenstore.toml`.
#[derive(Clone, Debug)]
pub struct AuthenticationToken {
    /// The bearer token string (typically a JWT, but opaque to the interface).
    pub token: String,
    /// Opaque user identity ID.
    pub user_id: String,
    /// Human-readable display name.
    pub user_name: String,
    /// Expiry as milliseconds since UNIX epoch.
    pub expires_ms: u64,
    /// Root domains this token is valid for.
    pub acceptable_root_domains: Vec<String>,
    /// One-time-use refresh token for obtaining a new authentication token
    /// without re-authenticating. `None` if the auth backend does not support
    /// refresh. Consumed on use -- the next refresh returns a new one.
    pub refresh_token: Option<String>,
    /// The scope the backend granted, space-delimited as RFC 6749 §3.3 has
    /// it. `None` when the backend reports no scope.
    pub scope: Option<String>,
}

/// Authorization token scoped to a specific resource.
///
/// Returned from `exchange_for_repository` or `exchange_for_custom_resource`.
/// Shorter-lived than the authentication token and re-obtained via exchange
/// when expired.
#[derive(Clone, Debug)]
pub struct AuthorizationToken {
    /// The bearer token string.
    pub token: String,
    /// Expiry as milliseconds since UNIX epoch.
    pub expires_ms: u64,
    /// Root domains this token is valid for.
    pub acceptable_root_domains: Vec<String>,
}

/// Resolved user identity information.
#[derive(Clone, Debug)]
pub struct ResolvedUser {
    /// Opaque user identity ID.
    pub user_id: String,
    /// Human-readable display name.
    pub user_name: String,
}
