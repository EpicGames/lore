// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Authentication against an `OpenID` Connect provider.

pub mod client_credentials;
pub mod device;
pub mod discovery;
pub mod exchange;
pub mod refresh;
pub mod revocation;
pub mod token;

use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::error::Disconnected;
use lore_base::lore_spawn_net;
use lore_base::lore_warn;
use lore_base::types::RepositoryId;
use tokio_util::task::AbortOnDropHandle;

use crate::error::ProtocolError;
use crate::traits::Authentication;
use crate::types::AuthSession;
use crate::types::AuthSessionPoll;
use crate::types::AuthenticationToken;
use crate::types::AuthorizationToken;
use crate::types::Oidc;
use crate::user_agent;

/// Cap on any document read from an identity provider. Generous
/// beside any real discovery document or key set, so the only documents it refuses are
/// ones no identity provider would send.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// How much of a rejected response body reaches the log. The body is whatever the endpoint
/// chose to send, so it is neither trustworthy nor necessarily small.
pub const LOGGED_BODY_LIMIT: usize = 512;

/// Caps on a request to the identity provider, so a provider that accepts a connection and
/// never answers cannot stall a login.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Redirects followed within one origin before the request fails.
const MAX_REDIRECTS: usize = 10;

/// The head of a response body, for diagnostics.
pub fn body_excerpt(body: &str) -> String {
    match body.char_indices().nth(LOGGED_BODY_LIMIT) {
        Some((end, _)) => format!("{}… ({} bytes total)", &body[..end], body.len()),
        None => body.to_string(),
    }
}

/// One pooled client for every request to an identity provider.
///
/// Redirects are followed only within the origin of the request that started them. The
/// scheme is part of the origin, so a redirect can neither move the request to another
/// host nor downgrade it to plain http. A cross-origin redirect is returned to the caller
/// as the 3xx response.
fn http_client() -> Result<&'static reqwest::Client, reqwest::Error> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .user_agent(user_agent())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let same_origin = attempt
                .previous()
                .first()
                .is_some_and(|first| first.origin() == attempt.url().origin());
            if !same_origin {
                attempt.stop()
            } else if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }))
        .build()?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Whether `url` may carry requests to an identity provider: `https`, or plain `http` to a
/// loopback host when `allow_loopback_http` is set. A URL carrying a username or password is
/// refused, so no credential reaches a log line or an error message through it.
fn is_permitted_url(url: &url::Url, allow_loopback_http: bool) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => true,
        "http" => allow_loopback_http && super::is_loopback_http_url(url),
        _ => false,
    }
}

/// Reads a response body, or `None` once it passes [`MAX_RESPONSE_BYTES`].
///
/// `Content-Length` is a claim, not a fact: it can be absent, understated, or the response
/// chunked. It refuses a body early, and the accumulating read enforces the cap.
async fn read_capped_body(
    response: &mut reqwest::Response,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    if response
        .content_length()
        .is_some_and(|declared| declared > MAX_RESPONSE_BYTES as u64)
    {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// Runs `request` on the net runtime, where network I/O runs, rather than on the caller's
/// runtime, which may be short-lived. Dropping the returned future aborts the request, so a
/// cancelled caller leaves none running.
async fn on_net_runtime<T: Send + 'static>(
    request: impl Future<Output = Result<T, ProtocolError>> + Send + 'static,
) -> Result<T, ProtocolError> {
    AbortOnDropHandle::new(lore_spawn_net!(request))
        .await
        .map_err(|source| {
            ProtocolError::internal_with_context(source, "requesting from the identity provider")
        })?
}

/// A response from an identity provider endpoint, read whole.
struct FormResponse {
    status: http::StatusCode,
    body: Vec<u8>,
}

impl FormResponse {
    fn body_text(&self) -> String {
        body_excerpt(&String::from_utf8_lossy(&self.body))
    }
}

/// Why a request to an identity provider failed, and whether the provider may have received
/// it.
pub(super) enum PostError {
    /// No connection was made (DNS, TCP, TLS or the connect timeout), so nothing was sent.
    NotSent(ProtocolError),
    /// The request may have reached the provider, and the provider may have acted on it.
    MaybeSent(ProtocolError),
}

impl From<PostError> for ProtocolError {
    fn from(error: PostError) -> Self {
        match error {
            PostError::NotSent(error) | PostError::MaybeSent(error) => error,
        }
    }
}

/// POSTs `form` to `url` as `application/x-www-form-urlencoded` (RFC 6749 §4.1.3), with
/// `authorization` as the `Authorization` header when given, and reads the answer. A request
/// that never reaches the provider is [`Disconnected`], as is a timeout; a body over
/// [`MAX_RESPONSE_BYTES`] is refused.
async fn post_form(
    what: &'static str,
    url: &str,
    form: &[(&str, &str)],
    authorization: Option<&str>,
) -> Result<FormResponse, PostError> {
    let client = http_client()
        .map_err(|source| PostError::NotSent(ProtocolError::internal_with_context(source, what)))?;
    let mut request = client.post(url).form(form);
    if let Some(authorization) = authorization {
        request = request.header(http::header::AUTHORIZATION, authorization);
    }
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(source) => {
            lore_warn!("OIDC {what} request failed: {source}");
            // A connect error, the connect timeout included, ends before any byte is written.
            let sent = !(source.is_connect() || source.is_builder());
            let error = if source.is_connect() || source.is_timeout() {
                ProtocolError::from(Disconnected)
            } else {
                ProtocolError::internal_with_context(source, what)
            };
            return Err(if sent {
                PostError::MaybeSent(error)
            } else {
                PostError::NotSent(error)
            });
        }
    };
    let status = response.status();
    match read_capped_body(&mut response).await {
        Ok(Some(body)) => Ok(FormResponse { status, body }),
        Ok(None) => {
            lore_warn!("OIDC {what} response exceeds {MAX_RESPONSE_BYTES} bytes");
            Err(PostError::MaybeSent(ProtocolError::internal(format!(
                "OIDC {what} response exceeds {MAX_RESPONSE_BYTES} bytes"
            ))))
        }
        Err(source) => {
            lore_warn!("OIDC {what} response could not be read: {source}");
            Err(PostError::MaybeSent(ProtocolError::from(Disconnected)))
        }
    }
}

/// The scheme tag an OIDC endpoint carries in front of its issuer URL, as `git+ssh` names a
/// route to a repository. The client adds it when it selects the OIDC path, so the endpoint,
/// and the token store entry under it, cannot be confused with a gRPC auth service the
/// server advertises under `https`, even one at the same URL as the issuer.
pub const SCHEME_PREFIX: &str = "oidc+";

pub fn endpoint(issuer: &str) -> String {
    format!("{SCHEME_PREFIX}{issuer}")
}

pub fn issuer(endpoint: &str) -> &str {
    endpoint.strip_prefix(SCHEME_PREFIX).unwrap_or(endpoint)
}

/// The OAuth 2.0 grants against the provider a server advertises, as the public client the
/// server names. Bound to that advertisement at selection, since the endpoint string alone
/// names the issuer and not the client.
///
/// The interactive login is the device authorization grant (RFC 8628), a login with an
/// external credential the token exchange (RFC 8693), a login as a registered confidential
/// client the client credentials grant (RFC 6749 §4.4), a new access token for a login the
/// refresh token grant (RFC 6749 §6), and authorization for a partition the token exchange
/// when the server advertises a template. A logout revokes the refresh token (RFC 7009) where
/// the provider advertises a revocation endpoint.
pub struct OidcAuthentication {
    oidc: Oidc,
}

impl OidcAuthentication {
    pub fn new(oidc: Oidc) -> Self {
        OidcAuthentication { oidc }
    }

    /// Refuses an `auth_url` naming an issuer other than the one the implementation was
    /// selected for.
    fn check_endpoint(&self, auth_url: &str) -> Result<(), ProtocolError> {
        let issuer = issuer(auth_url);
        if issuer != self.oidc.issuer {
            return Err(ProtocolError::internal(format!(
                "auth URL names OIDC issuer {issuer}, but the selected provider is {}",
                self.oidc.issuer
            )));
        }
        Ok(())
    }

    /// The provider's endpoints, from its discovery document.
    async fn discover(
        &self,
        auth_url: &str,
    ) -> Result<Arc<discovery::DiscoveryDocument>, ProtocolError> {
        self.check_endpoint(auth_url)?;
        discovery::discover(&self.oidc.issuer)
            .await
            .map_err(ProtocolError::from)
    }

    async fn authorize(
        &self,
        auth_url: &str,
        authn_token: &str,
        target: Option<exchange::Target>,
    ) -> Result<AuthorizationToken, ProtocolError> {
        self.check_endpoint(auth_url)?;
        match target {
            Some(target) => exchange::exchange(&self.oidc, authn_token, target).await,
            None => exchange::unexchanged(authn_token),
        }
    }
}

impl From<discovery::DiscoveryError> for ProtocolError {
    fn from(error: discovery::DiscoveryError) -> Self {
        if let discovery::DiscoveryError::Request { source, .. } = &error
            && (source.is_connect() || source.is_timeout())
        {
            lore_warn!("{error}");
            return ProtocolError::from(Disconnected);
        }
        ProtocolError::internal_with_context(error, "discovering the provider's endpoints")
    }
}

#[async_trait]
impl Authentication for OidcAuthentication {
    async fn start_auth_session(
        &self,
        auth_url: &str,
        _client_state: &str,
        _correlation_id: &str,
    ) -> Result<AuthSession, ProtocolError> {
        let document = self.discover(auth_url).await?;
        device::authorize(&self.oidc, &document).await
    }

    async fn poll_auth_session(
        &self,
        auth_url: &str,
        _client_state: &str,
        session_code: &str,
        _correlation_id: &str,
    ) -> Result<AuthSessionPoll, ProtocolError> {
        let document = self.discover(auth_url).await?;
        device::poll(&self.oidc, &document, session_code).await
    }

    async fn exchange_external_token(
        &self,
        auth_url: &str,
        token: &str,
        token_type: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        let document = self.discover(auth_url).await?;
        exchange::login(&self.oidc, &document, token, token_type).await
    }

    async fn authenticate_client(
        &self,
        auth_url: &str,
        client_id: &str,
        client_secret: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        let document = self.discover(auth_url).await?;
        client_credentials::login(&document, client_id, client_secret).await
    }

    /// Nothing is sent when the provider's endpoints cannot be discovered, so that failure is
    /// [`Disconnected`] whatever its cause, as the trait requires.
    async fn refresh_authentication(
        &self,
        auth_url: &str,
        refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        let document = self.discover(auth_url).await.map_err(|error| {
            lore_warn!("OIDC refresh grant not sent, the provider was not discovered: {error}");
            ProtocolError::from(Disconnected)
        })?;
        refresh::refresh(&self.oidc, &document, refresh_token).await
    }

    async fn revoke_refresh_token(
        &self,
        auth_url: &str,
        refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<(), ProtocolError> {
        let document = self.discover(auth_url).await?;
        revocation::revoke(&self.oidc, &document, refresh_token).await
    }

    async fn exchange_for_repository(
        &self,
        auth_url: &str,
        authn_token: &str,
        repository: RepositoryId,
        _correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        let target = exchange::partition_target(&self.oidc, repository)?;
        self.authorize(auth_url, authn_token, target).await
    }

    /// The resource is sent verbatim as `resource`, whichever template names a partition.
    async fn exchange_for_custom_resource(
        &self,
        auth_url: &str,
        authn_token: &str,
        resource_id: &str,
        _correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        let target = exchange::is_exchanging(&self.oidc)
            .then(|| exchange::Target::Resource(resource_id.to_owned()));
        self.authorize(auth_url, authn_token, target).await
    }
}
