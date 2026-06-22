// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
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
}

impl EnvironmentConfig {
    pub fn max_query_batch(&self) -> Option<usize> {
        self.config.as_ref().and_then(|c| c.max_query_batch)
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
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct CompressionMode(u32);

impl CompressionMode {
    pub fn from_u32(value: u32) -> Self {
        Self(value)
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct EnvironmentServerConfig {
    pub max_query_batch: Option<usize>,
    pub compression_mode: Option<CompressionMode>,
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
        Self::Identifier(value)
    }
}

impl From<Hash> for RevisionListStart {
    fn from(value: Hash) -> Self {
        Self::Signature(value)
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
    /// Opaque session identifier for polling.
    pub session_code: String,
    /// URL the user should visit to authenticate.
    pub login_url: String,
}

/// Authentication token with user identity metadata.
///
/// Returned from login flows (interactive, token exchange, refresh).
/// This is the protocol-layer type -- transient, in-memory. The orchestration
/// layer converts it to `SerializedToken` (the token store's on-disk format)
/// when persisting to `tokens.toml`.
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

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK: &str = "grpc://fallback.example:1234";

    fn env_with(endpoint: Endpoint) -> EnvironmentConfig {
        EnvironmentConfig {
            endpoint: Some(endpoint),
            config: None,
        }
    }

    #[test]
    fn service_url_returns_override_when_set() {
        let env = env_with(Endpoint {
            storage_url: Some("quic://storage.example:7000".into()),
            ..Default::default()
        });
        assert_eq!(env.storage_url(FALLBACK), "quic://storage.example:7000");
    }

    #[test]
    fn service_url_falls_back_when_field_is_none() {
        let env = env_with(Endpoint::default());
        assert_eq!(env.storage_url(FALLBACK), FALLBACK);
        assert_eq!(env.revision_url(FALLBACK), FALLBACK);
        assert_eq!(env.lock_url(FALLBACK), FALLBACK);
        assert_eq!(env.repository_url(FALLBACK), FALLBACK);
        assert_eq!(env.notification_url(FALLBACK), FALLBACK);
    }

    #[test]
    fn service_url_falls_back_when_field_is_empty_string() {
        // An empty Option<String> from proto decoding must behave identically
        // to None — the field is "unset."
        let env = env_with(Endpoint {
            storage_url: Some(String::new()),
            revision_url: Some(String::new()),
            ..Default::default()
        });
        assert_eq!(env.storage_url(FALLBACK), FALLBACK);
        assert_eq!(env.revision_url(FALLBACK), FALLBACK);
    }

    #[test]
    fn service_url_falls_back_when_endpoint_section_missing() {
        let env = EnvironmentConfig {
            endpoint: None,
            config: None,
        };
        assert_eq!(env.storage_url(FALLBACK), FALLBACK);
        assert_eq!(env.repository_url(FALLBACK), FALLBACK);
    }

    #[test]
    fn per_service_overrides_are_independent() {
        // Only some services have overrides; the others must fall back.
        let env = env_with(Endpoint {
            storage_url: Some("quic://storage.example:7000".into()),
            lock_url: Some("grpc://lock.example:8000".into()),
            ..Default::default()
        });
        assert_eq!(env.storage_url(FALLBACK), "quic://storage.example:7000");
        assert_eq!(env.lock_url(FALLBACK), "grpc://lock.example:8000");
        assert_eq!(env.revision_url(FALLBACK), FALLBACK);
        assert_eq!(env.repository_url(FALLBACK), FALLBACK);
        assert_eq!(env.notification_url(FALLBACK), FALLBACK);
    }
}
