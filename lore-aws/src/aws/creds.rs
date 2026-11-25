// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Where credentials come from, and how they stay fresh.
//!
//! The chain covers every way Lore is deployed — environment, ECS or EKS Pod Identity, EKS IRSA,
//! EC2 instance profile — plus static keys from `~/.aws/credentials` for a developer machine. It
//! deliberately stops there: `source_profile` role chaining, `credential_process`, SSO and
//! `mfa_serial` would need an `AssumeRole` call signed with a recursively-resolved credential, and
//! a TOTP prompt, inside a server process. A developer who needs a profile's chain resolves it in
//! their shell instead:
//!
//! ```text
//! eval "$(aws configure export-credentials --profile prod --format env)"
//! ```
//!
//! which feeds [`EnvProvider`], the first link.
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use time::UtcDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::debug;
use tracing::warn;

/// How long before expiry a refresh starts.
///
/// Long enough that a slow STS call finishes while the current credentials still sign, short enough
/// that a process does not refresh constantly against a one-hour lifetime.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// One set of credentials.
///
/// `Debug` redacts the secret and the token. They reach the `Authorization` and
/// `x-amz-security-token` headers and nothing else — never a log line, never a span field.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: Box<str>,
    pub secret_access_key: Box<str>,
    pub session_token: Option<Box<str>>,
    /// When these stop working. `None` for static keys, which do not expire.
    pub expires_at: Option<UtcDateTime>,
}

impl Credentials {
    pub fn new(
        access_key_id: impl Into<Box<str>>,
        secret_access_key: impl Into<Box<str>>,
        session_token: Option<Box<str>>,
        expires_at: Option<UtcDateTime>,
    ) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token,
            expires_at,
        }
    }

    /// Whether these can no longer sign.
    pub fn is_expired(&self, now: UtcDateTime) -> bool {
        self.expires_at.is_some_and(|expiry| now >= expiry)
    }

    /// Whether a refresh should start now, ahead of expiry.
    pub fn needs_refresh(&self, now: UtcDateTime) -> bool {
        match self.expires_at {
            Some(expiry) => now + REFRESH_MARGIN >= expiry,
            None => false,
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Why a provider did not produce credentials.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    /// Nothing this provider needs is configured, so the chain should try the next one.
    #[error("{0} is not configured")]
    NotProvided(&'static str),
    /// Configured, but asking for something this client does not implement.
    #[error("{provider} is configured for something this client does not support: {detail}")]
    Unsupported {
        provider: &'static str,
        detail: String,
    },
    /// Configured, tried, failed.
    #[error("{provider} failed: {detail}")]
    Failed {
        provider: &'static str,
        detail: String,
    },
    /// Configured, tried, refused for load or never reached.
    #[error("{provider} could not be asked right now: {detail}")]
    Transient {
        provider: &'static str,
        detail: String,
    },
    /// Every provider declined.
    #[error("no credentials provider is configured")]
    NoProvider,
}

impl CredentialsError {
    /// Whether the chain should keep looking.
    #[lore_macro::test_pub]
    fn is_not_provided(&self) -> bool {
        matches!(self, Self::NotProvided(_))
    }

    /// Whether asking again later could succeed, so a caller should back off rather than report a
    /// failed operation.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

/// A source of credentials.
#[async_trait]
pub trait ProvideCredentials: Send + Sync + fmt::Debug {
    async fn provide(&self) -> Result<Credentials, CredentialsError>;

    /// The provider's name, for log lines and errors.
    fn name(&self) -> &'static str;
}

/// Static credentials, for tests and for an explicitly configured caller.
#[derive(Debug)]
pub struct StaticProvider(Credentials);

impl StaticProvider {
    pub fn new(credentials: Credentials) -> Self {
        Self(credentials)
    }
}

#[async_trait]
impl ProvideCredentials for StaticProvider {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        Ok(self.0.clone())
    }

    fn name(&self) -> &'static str {
        "static"
    }
}

/// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_SESSION_TOKEN`.
#[derive(Debug, Default)]
pub struct EnvProvider;

#[async_trait]
impl ProvideCredentials for EnvProvider {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        // Set to nothing is how a wrapper clears the variable, so it declines here rather than
        // reading as an id whose secret has gone missing.
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
        if access_key_id.is_empty() {
            return Err(CredentialsError::NotProvided("env"));
        }

        // The id without the secret is a half-configured environment, which is worth saying out
        // loud rather than skipping past.
        let Ok(secret_access_key) = std::env::var("AWS_SECRET_ACCESS_KEY") else {
            return Err(CredentialsError::Failed {
                provider: "env",
                detail: "AWS_ACCESS_KEY_ID is set but AWS_SECRET_ACCESS_KEY is not".to_owned(),
            });
        };

        if secret_access_key.is_empty() {
            return Err(CredentialsError::NotProvided("env"));
        }

        Ok(Credentials::new(
            access_key_id,
            secret_access_key,
            std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|token| !token.is_empty())
                .map(Into::into),
            None,
        ))
    }

    fn name(&self) -> &'static str {
        "env"
    }
}

/// Static keys from `~/.aws/credentials`, or `AWS_SHARED_CREDENTIALS_FILE`.
///
/// Static keys only. A profile asking for a role, a process or an SSO session is refused with the
/// workaround named, because silently skipping it would look like "no credentials found" to someone
/// whose configuration is right for the AWS CLI.
#[derive(Debug, Default)]
pub struct ProfileProvider;

impl ProfileProvider {
    fn credentials_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("AWS_SHARED_CREDENTIALS_FILE")
            && !path.is_empty()
        {
            return Some(PathBuf::from(path));
        }

        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(".aws").join("credentials"))
    }

    fn profile_name() -> String {
        std::env::var("AWS_PROFILE")
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "default".to_owned())
    }
}

#[async_trait]
impl ProvideCredentials for ProfileProvider {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        let path = Self::credentials_path().ok_or(CredentialsError::NotProvided("profile"))?;
        // No file is the ordinary case in a container, so it declines rather than failing.
        let Ok(contents) = tokio::fs::read_to_string(&path).await else {
            return Err(CredentialsError::NotProvided("profile"));
        };

        parse_profile(&contents, &Self::profile_name())
    }

    fn name(&self) -> &'static str {
        "profile"
    }
}

/// Read one profile's static keys out of a credentials file.
///
/// Sections are matched both bare (`[default]`, as the credentials file writes them) and prefixed
/// (`[profile default]`, as the config file does), because a hand-written file may use either.
#[lore_macro::test_pub]
fn parse_profile(contents: &str, wanted: &str) -> Result<Credentials, CredentialsError> {
    let mut in_profile = false;
    let mut access_key_id = None;
    let mut secret_access_key = None;
    let mut session_token = None;
    let mut unsupported = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let section = section.trim();
            let name = section.strip_prefix("profile ").unwrap_or(section).trim();
            in_profile = name == wanted;
            continue;
        }

        if !in_profile {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_owned();

        match key.trim() {
            "aws_access_key_id" => access_key_id = Some(value),
            "aws_secret_access_key" => secret_access_key = Some(value),
            "aws_session_token" => session_token = Some(value),
            key @ ("role_arn" | "credential_process" | "sso_session" | "sso_start_url"
            | "credential_source") => unsupported = Some(key.to_owned()),
            _ => {}
        }
    }

    match (access_key_id, secret_access_key) {
        (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => Ok(Credentials::new(
            id,
            secret,
            session_token
                .filter(|token| !token.is_empty())
                .map(Into::into),
            None,
        )),
        _ => match unsupported {
            Some(key) => Err(CredentialsError::Unsupported {
                provider: "profile",
                detail: format!(
                    "profile {wanted:?} uses {key}, which this client does not resolve; run \
                     `eval \"$(aws configure export-credentials --profile {wanted} --format env)\"` \
                     and let the environment provider take it"
                ),
            }),
            None => Err(CredentialsError::NotProvided("profile")),
        },
    }
}

/// Every source, tried in order.
#[derive(Debug)]
pub struct CredentialsChain {
    providers: Vec<Box<dyn ProvideCredentials>>,
}

impl CredentialsChain {
    pub fn new(providers: Vec<Box<dyn ProvideCredentials>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl ProvideCredentials for CredentialsChain {
    /// The first provider that answers wins.
    ///
    /// A provider that is configured but fails does not abort the chain — a metadata endpoint that
    /// times out should not shadow a working profile — but its error is remembered and returned if
    /// nothing later succeeds, so the message names the real problem instead of "no provider".
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        let mut first_real_failure = None;

        for provider in &self.providers {
            match provider.provide().await {
                Ok(credentials) => {
                    debug!(provider = provider.name(), "Resolved AWS credentials");
                    return Ok(credentials);
                }
                Err(error) if error.is_not_provided() => {
                    debug!(provider = provider.name(), "Credentials provider declined");
                }
                Err(error) => {
                    warn!(
                        provider = provider.name(),
                        %error, "Credentials provider failed, trying the next"
                    );
                    first_real_failure = first_real_failure.or(Some(error));
                }
            }
        }

        Err(first_real_failure.unwrap_or(CredentialsError::NoProvider))
    }

    fn name(&self) -> &'static str {
        "chain"
    }
}

/// Credentials held for reuse, refreshed ahead of expiry.
///
/// The hot path takes a read lock and clones an `Arc`, allocating nothing. Refreshes are serialized
/// by a `tokio` mutex, so a hundred concurrent requests hitting expiry make one call to the
/// provider, and a caller holding credentials that still sign is served them rather than waiting on
/// a refresh already in flight.
#[lore_macro::test_pub]
#[derive(Debug)]
pub struct CachedCredentials {
    provider: Arc<dyn ProvideCredentials>,
    state: parking_lot::RwLock<CacheState>,
    refreshing: tokio::sync::Mutex<()>,
}

#[lore_macro::test_pub]
#[derive(Debug, Default)]
struct CacheState {
    credentials: Option<Arc<Credentials>>,
    /// When a refresh was last *attempted*, successful or not.
    last_attempt: Option<UtcDateTime>,
}

/// The floor between refresh attempts.
///
/// The gate serializes refreshes but cannot on its own deduplicate them: a provider that returns
/// credentials already inside the refresh margin — a short session lifetime, or a clock skewed
/// enough to make fresh credentials look stale — leaves every waiting caller still wanting a
/// refresh, and the endpoint gets one call per request. This bounds that to one attempt per second
/// per process, which a normal hourly refresh never notices.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

impl CachedCredentials {
    pub fn new(provider: Arc<dyn ProvideCredentials>) -> Self {
        Self {
            provider,
            state: parking_lot::RwLock::new(CacheState::default()),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// Credentials good to sign with at `now`.
    ///
    /// `now` is the caller's so it can judge freshness by the service's clock, and so the refresh
    /// schedule is testable.
    ///
    /// Held credentials that want refreshing but still sign are served at once when another
    /// caller's refresh is in flight. Only a caller with nothing usable waits for the refresh to
    /// finish.
    pub async fn get_at(&self, now: UtcDateTime) -> Result<Arc<Credentials>, CredentialsError> {
        let held = self.state.read().credentials.clone();

        let _guard = match held {
            Some(credentials) if !credentials.needs_refresh(now) => return Ok(credentials),
            Some(credentials) if !credentials.is_expired(now) => {
                let Ok(guard) = self.refreshing.try_lock() else {
                    return Ok(credentials);
                };
                guard
            }
            _ => self.refreshing.lock().await,
        };

        // Another task may have refreshed while this one waited for the gate.
        if let Some(credentials) = self.fresh_enough(now) {
            return Ok(credentials);
        }

        let (credentials, last_attempt) = {
            let state = self.state.read();
            (state.credentials.clone(), state.last_attempt)
        };

        if let Some(attempted) = last_attempt
            && Duration::try_from(now - attempted).is_ok_and(|since| since < MIN_REFRESH_INTERVAL)
        {
            return match credentials {
                Some(credentials) if !credentials.is_expired(now) => Ok(credentials),
                _ => Err(CredentialsError::Transient {
                    provider: self.provider.name(),
                    detail: "credentials expired and a refresh was attempted too recently"
                        .to_owned(),
                }),
            };
        }

        self.state.write().last_attempt = Some(now);

        match self.provider.provide().await {
            Ok(fresh) => {
                let fresh = Arc::new(fresh);
                self.state.write().credentials = Some(fresh.clone());
                Ok(fresh)
            }
            Err(error) => {
                // Refreshing early means a failure often still has working credentials behind it.
                // Serving them beats failing a request that would have succeeded.
                match credentials {
                    Some(credentials) if !credentials.is_expired(now) => {
                        warn!(
                            %error,
                            "Failed to refresh AWS credentials; the current ones are still valid"
                        );
                        Ok(credentials)
                    }
                    _ => Err(error),
                }
            }
        }
    }

    /// The held credentials, if they do not want refreshing yet.
    pub(crate) fn fresh_enough(&self, now: UtcDateTime) -> Option<Arc<Credentials>> {
        let credentials = self.state.read().credentials.clone()?;

        (!credentials.needs_refresh(now)).then_some(credentials)
    }

    /// Discard `rejected`, because the service rejected it as expired.
    ///
    /// The cache decides freshness from the expiry the provider reported, so it cannot detect the
    /// case where that disagrees with the service — a provider that reports an expiry it does not
    /// honour, or a host clock far enough out that credentials look valid here and expired there.
    /// The service saying `ExpiredToken` is the only evidence of it, and this is how that evidence
    /// gets back in.
    ///
    /// Nothing changes unless `rejected` is what the cache holds. A rejection can arrive after a
    /// refresh has already replaced the credentials it names, and discarding the replacement would
    /// cost another refresh and fail requests signed with credentials the service accepts.
    ///
    /// The refresh floor is cleared along with the credentials. It exists to stop a provider that
    /// keeps returning almost-stale credentials from being called once per request; a rejection is
    /// a fact from the service, and each operation acts on one at most once, so holding the refresh
    /// off behind a recent attempt would fail the operation with credentials both sides agree are
    /// dead.
    pub fn invalidate(&self, rejected: &Arc<Credentials>) {
        let mut state = self.state.write();

        if state
            .credentials
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(held, rejected))
        {
            state.credentials = None;
            state.last_attempt = None;
        }
    }
}

/// Parse an expiry as the credential endpoints and STS all render it: RFC3339.
///
/// An absent expiry means credentials that do not expire. One that is present but unreadable is a
/// failure rather than that: read as no expiry it would hold a temporary credential as permanent,
/// so the refresh that keeps it working would never run and every call would fail once the session
/// ended.
#[lore_macro::test_pub]
pub(crate) fn parse_expiry(
    provider: &'static str,
    text: Option<&str>,
) -> Result<Option<UtcDateTime>, CredentialsError> {
    text.map(|text| {
        OffsetDateTime::parse(text, &Rfc3339)
            .map(OffsetDateTime::to_utc)
            .map_err(|error| CredentialsError::Failed {
                provider,
                detail: format!("credentials expiry is not RFC3339: {error}"),
            })
    })
    .transpose()
}
