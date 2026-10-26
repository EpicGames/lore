// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod exchange;
pub mod mode;
pub mod oidc;
pub mod token_only;
pub mod ucs_auth;
pub mod user_directory;
pub mod user_service_v1;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;

use lore_base::error::NotSupported;
pub use mode::AuthMode;
pub use mode::AuthPath;
pub use mode::UnknownAuthMode;
use parking_lot::Mutex;

use crate::error::ProtocolError;
use crate::traits::Authentication;
use crate::types::EnvironmentConfig;
use crate::types::Oidc;

static REGISTER_BUILTIN: Once = Once::new();

/// The authentication path a connection takes: the endpoint every token
/// store entry and authorization cache key is named by, and the
/// implementation that serves it. The two travel together, since the
/// endpoint alone does not name the implementation on every path.
#[derive(Clone)]
pub struct AuthEndpoint {
    /// The server's `auth_url` on the gRPC path, [`oidc::endpoint`] of the
    /// issuer on the OIDC path, empty for a server that advertises no
    /// authentication.
    pub url: String,
    pub auth: Arc<dyn Authentication>,
}

impl AuthEndpoint {
    pub fn new(url: String, auth: Arc<dyn Authentication>) -> Self {
        AuthEndpoint { url, auth }
    }

    /// The endpoint of a server that advertises no authentication. Every
    /// grant fails with [`NotSupported`], as the exchange refused it before.
    pub fn none() -> Self {
        AuthEndpoint {
            url: String::new(),
            auth: Arc::new(NoAuthentication),
        }
    }

    /// The endpoint `url` names through the scheme registry, for an auth URL
    /// given explicitly rather than selected from a server's advertisement.
    /// An empty URL is [`AuthEndpoint::none`].
    pub fn resolve(url: &str) -> Result<Self, ProtocolError> {
        if url.is_empty() {
            return Ok(Self::none());
        }
        Ok(AuthEndpoint {
            url: url.to_string(),
            auth: authentication::find(url)?,
        })
    }

    pub fn is_none(&self) -> bool {
        self.url.is_empty()
    }
}

impl std::fmt::Display for AuthEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.url)
    }
}

impl std::fmt::Debug for AuthEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthEndpoint")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// The implementation behind [`AuthEndpoint::none`].
struct NoAuthentication;

impl NoAuthentication {
    fn refused() -> ProtocolError {
        NotSupported {
            operation: "No authentication configured on server".to_string(),
        }
        .into()
    }
}

#[async_trait::async_trait]
impl Authentication for NoAuthentication {
    async fn start_auth_session(
        &self,
        _auth_url: &str,
        _client_state: &str,
        _correlation_id: &str,
    ) -> Result<crate::types::AuthSession, ProtocolError> {
        Err(Self::refused())
    }

    async fn poll_auth_session(
        &self,
        _auth_url: &str,
        _client_state: &str,
        _session_code: &str,
        _correlation_id: &str,
    ) -> Result<crate::types::AuthSessionPoll, ProtocolError> {
        Err(Self::refused())
    }

    async fn exchange_external_token(
        &self,
        _auth_url: &str,
        _token: &str,
        _token_type: &str,
        _correlation_id: &str,
    ) -> Result<crate::types::AuthenticationToken, ProtocolError> {
        Err(Self::refused())
    }

    async fn authenticate_client(
        &self,
        _auth_url: &str,
        _client_id: &str,
        _client_secret: &str,
        _correlation_id: &str,
    ) -> Result<crate::types::AuthenticationToken, ProtocolError> {
        Err(Self::refused())
    }

    async fn refresh_authentication(
        &self,
        _auth_url: &str,
        _refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<crate::types::AuthenticationToken, ProtocolError> {
        Err(Self::refused())
    }

    async fn revoke_refresh_token(
        &self,
        _auth_url: &str,
        _refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<(), ProtocolError> {
        Err(Self::refused())
    }

    async fn exchange_for_repository(
        &self,
        _auth_url: &str,
        _authn_token: &str,
        _repository: lore_base::types::RepositoryId,
        _correlation_id: &str,
    ) -> Result<crate::types::AuthorizationToken, ProtocolError> {
        Err(Self::refused())
    }

    async fn exchange_for_custom_resource(
        &self,
        _auth_url: &str,
        _authn_token: &str,
        _resource_id: &str,
        _correlation_id: &str,
    ) -> Result<crate::types::AuthorizationToken, ProtocolError> {
        Err(Self::refused())
    }
}

fn register_builtin() {
    REGISTER_BUILTIN.call_once(|| {
        let ucs_auth = Arc::new(ucs_auth::UcsAuthentication);
        for scheme in ucs_auth::SCHEMES {
            let _ = authentication::add(scheme, ucs_auth.clone());
        }
    });
}

/// Whether `url` is a plain-http URL naming a loopback host, where the traffic never leaves
/// the machine. A URL carrying a username or password is refused, so
/// `http://localhost:pass@evil.com` cannot pass.
pub(crate) fn is_loopback_http_url(url: &url::Url) -> bool {
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Extracts the scheme from an auth URL (the part before `://`).
pub fn parse_scheme(auth_url: &str) -> Result<&str, ProtocolError> {
    auth_url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .ok_or_else(|| {
            ProtocolError::internal(format!("invalid auth URL (missing scheme): '{auth_url}'"))
        })
}

/// Scheme-based registry for `Authentication` implementations.
///
/// A server advertises up to two authentication paths: its gRPC auth service
/// at `auth_url`, and an OIDC issuer. [`select`] chooses between them and
/// names the endpoint the chosen implementation is addressed by: `auth_url`
/// as the server set it on the gRPC path, the issuer behind the client's own
/// [`oidc::SCHEME_PREFIX`] on the OIDC path, so an issuer equal to a gRPC
/// `auth_url` still yields two distinct endpoints and token store entries.
///
/// The registry serves the gRPC path, whose endpoint names everything the
/// implementation needs. The OIDC implementation also needs the client the
/// server advertises, which the endpoint does not carry, so [`select`] binds
/// it to the advertisement and [`find`] does not serve the OIDC schemes.
pub mod authentication {
    use super::*;

    static AUTHENTICATION_MAP: Mutex<Option<HashMap<String, Arc<dyn Authentication>>>> =
        Mutex::new(None);

    /// The authentication path a client takes to a server, and how to reach it.
    #[derive(Clone, Debug)]
    pub struct Selected {
        pub path: AuthPath,
        pub endpoint: AuthEndpoint,
    }

    /// Selects the authentication path to a server from the client's `mode`
    /// and the paths the server advertises in `environment`.
    pub fn select(
        mode: AuthMode,
        environment: &EnvironmentConfig,
    ) -> Result<Option<Selected>, ProtocolError> {
        register_builtin();

        let auth_url = environment
            .endpoint
            .as_ref()
            .and_then(|endpoint| endpoint.auth_url.as_deref())
            .filter(|auth_url| !auth_url.is_empty());
        let oidc = environment.oidc.as_ref();

        let selected = match (auth_url, oidc) {
            (None, None) => return Ok(None),
            (Some(auth_url), None) => match mode {
                AuthMode::Oidc => {
                    return Err(NotSupported {
                        operation: format!(
                            "auth mode '{mode}': the server advertises no OIDC issuer"
                        ),
                    }
                    .into());
                }
                AuthMode::Auto | AuthMode::Grpc => grpc_path(auth_url)?,
            },
            (None, Some(oidc)) => match mode {
                AuthMode::Grpc => {
                    return Err(NotSupported {
                        operation: format!(
                            "auth mode '{mode}': the server advertises no gRPC auth service"
                        ),
                    }
                    .into());
                }
                AuthMode::Auto | AuthMode::Oidc => oidc_path(oidc),
            },
            (Some(auth_url), Some(oidc)) => match mode.path(oidc.preferred) {
                AuthPath::Grpc => grpc_path(auth_url)?,
                AuthPath::Oidc => oidc_path(oidc),
            },
        };
        Ok(Some(selected))
    }

    /// The gRPC path, through the implementation `auth_url`'s scheme names.
    fn grpc_path(auth_url: &str) -> Result<Selected, ProtocolError> {
        Ok(Selected {
            path: AuthPath::Grpc,
            endpoint: AuthEndpoint::new(auth_url.to_string(), find(auth_url)?),
        })
    }

    /// The OIDC path: the grants against the advertised provider, as the
    /// advertised client, at the endpoint naming the issuer.
    fn oidc_path(oidc: &Oidc) -> Selected {
        Selected {
            path: AuthPath::Oidc,
            endpoint: AuthEndpoint::new(
                oidc::endpoint(&oidc.issuer),
                Arc::new(oidc::OidcAuthentication::new(oidc.clone())),
            ),
        }
    }

    /// Finds the `Authentication` implementation for the given auth URL by
    /// parsing its scheme. Registers builtin implementations on first call.
    ///
    /// The full auth URL (including scheme) is passed to trait methods as-is --
    /// the implementation decides how to interpret it.
    pub fn find(auth_url: &str) -> Result<Arc<dyn Authentication>, ProtocolError> {
        register_builtin();

        let scheme = parse_scheme(auth_url)?;
        // The lookup and the scheme list the error reports share one lock
        // acquisition, but the list is only collected on a miss, so a hit never
        // pays to clone every registered key.
        let found = {
            let map = AUTHENTICATION_MAP.lock();
            let auth = map.as_ref().and_then(|m| m.get(scheme).cloned());
            auth.ok_or_else(|| {
                map.as_ref()
                    .map(|m| m.keys().cloned().collect::<Vec<String>>())
                    .unwrap_or_default()
            })
        };
        found.map_err(|available| {
            ProtocolError::internal(format!(
                "no authentication implementation registered for scheme '{scheme}' (available: {available:?})",
            ))
        })
    }

    /// Registers an `Authentication` implementation for the given scheme.
    pub fn add(scheme: &str, auth: Arc<dyn Authentication>) -> Result<(), ProtocolError> {
        let mut map = AUTHENTICATION_MAP.lock();
        if map.is_none() {
            *map = Some(HashMap::new());
        }
        map.as_mut().unwrap().insert(scheme.to_string(), auth);
        Ok(())
    }

    /// Lists registered scheme names (for diagnostics).
    pub fn schemes() -> Vec<String> {
        let map = AUTHENTICATION_MAP.lock();
        match map.as_ref() {
            Some(m) => m.keys().cloned().collect(),
            None => Vec::new(),
        }
    }
}
