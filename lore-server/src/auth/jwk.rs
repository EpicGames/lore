// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use dashmap::DashMap;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::jwk::AlgorithmParameters;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::jwk::KeyAlgorithm;
use jsonwebtoken::jwk::KeyOperations;
use jsonwebtoken::jwk::PublicKeyUse;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use lore_transport::auth::oidc::body_excerpt;
use lore_transport::user_agent;
use opentelemetry::KeyValue;
use serde::Deserialize;
use smallvec::SmallVec;
use thiserror::Error;
use tracing::Instrument;
use tracing::info;
use tracing::warn;

#[lore_macro::test_pub]
#[derive(Clone)]
struct JWKServiceKey {
    /// Kept so a refresh can tell whether the material behind a key id actually changed.
    /// `DecodingKey` is opaque and not comparable; this is the only thing that can answer
    /// "did the identity provider rotate this key, or is it the same one?".
    jwk: Jwk,
    decoding_key: DecodingKey,
    algorithm: jsonwebtoken::Algorithm,
}

#[derive(Clone, Default, Deserialize, Debug)]
pub struct JWKServiceSettings {
    /// Where to fetch the key set. Optional: when unset the `jwks_uri` is
    /// resolved through OIDC discovery against `jwt_issuer`, so this is the
    /// override for providers with non-standard discovery.
    pub endpoint: Option<String>,
}

#[derive(Error, Debug)]
pub enum JWKServiceError {
    #[error("Internal Error")]
    InternalError,
    #[error("Could not parse jwks endpoint response")]
    ParseError(#[from] serde_json::Error),
    #[error("Could not decode jwk key")]
    DecodingError(#[from] jsonwebtoken::errors::Error),
    #[error("Key for kid not found")]
    NotFound,
    #[error("JWKS endpoint returned no key usable for signature verification")]
    NoUsableKeys,
    #[error("JWKS document is larger than this server will read")]
    ResponseTooLarge,
    #[error(
        "no JWKS endpoint: set [server.auth.jwk].endpoint, or set jwt_issuer to an issuer URL \
         for OIDC discovery"
    )]
    EndpointUnresolvable,
    #[error(
        "OIDC discovery document names issuer '{actual}', but the configured issuer is '{expected}'"
    )]
    DiscoveryIssuerMismatch { expected: String, actual: String },
    #[error(
        "OIDC discovery returned jwks_uri '{jwks_uri}', which this server will not fetch keys \
         over: an https issuer's keys are only fetched over https"
    )]
    JwksUriNotHttps { jwks_uri: String },
}

#[async_trait]
pub trait JWKService: Send + Sync {
    /// Get the public key for the specified key id. Note: this may potentially result in a network
    /// call if the key for key id is not already cached locally by the implementer of this trait.
    async fn get_key(
        &self,
        kid: &str,
    ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError>;

    /// Cache-only lookup that never performs I/O or blocks. Returns `None` when the key
    /// is not cached, letting a synchronous caller (the tonic auth interceptor, which
    /// cannot `.await`) serve the hot path and fall back to [`get_key`] only on a miss.
    fn get_cached_key(&self, kid: &str) -> Option<(DecodingKey, jsonwebtoken::Algorithm)>;

    /// Re-fetch the key set on the suspicion that `kid`'s cached material is stale, returning
    /// the key only if it changed.
    ///
    /// [`get_key`](Self::get_key) cannot serve this: it is satisfied by the cache holding *a* key
    /// for the id, which is what a provider creates by rotating material under an unchanged key
    /// id. `None` for unchanged keeps the caller from repeating a verification certain to fail
    /// identically.
    ///
    /// **Reachable by anyone who can send a bearer token**, since a bad signature against a known
    /// id is indistinguishable from a rotation until the keys are compared. An implementation must
    /// bound how often it fetches, and must not evict the cached key to do so — an empty cache is
    /// precisely the state that legitimately bypasses throttling.
    async fn refresh_key(
        &self,
        kid: &str,
    ) -> Result<Option<(DecodingKey, jsonwebtoken::Algorithm)>, JWKServiceError>;
}

/// Caps on the JWKS request. The auth interceptor reaches this through
/// `block_in_place`, so an identity provider that accepts a connection and never answers
/// would otherwise pin a worker indefinitely.
const JWKS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const JWKS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Shortest interval between JWKS fetches once any key is cached, which bounds outbound
/// requests however many unknown key ids arrive. A per-kid negative cache cannot do
/// this: an unauthenticated caller can cycle key ids and never repeat one.
#[lore_macro::test_pub]
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Cap on the JWKS and discovery documents this server will hold in memory.
/// [`JWKS_REQUEST_TIMEOUT`] bounds how long a fetch may run, which is not the same as
/// bounding what it delivers.
#[lore_macro::test_pub]
const JWKS_MAX_RESPONSE_BYTES: usize = lore_transport::auth::oidc::MAX_RESPONSE_BYTES;

/// Read a response body, refusing anything past [`JWKS_MAX_RESPONSE_BYTES`].
///
/// `Content-Length` is consulted first when the endpoint offers one, but it is a claim
/// rather than a fact — it can be absent, understated, or the response chunked — so the
/// accumulating read is what actually enforces the cap. `what` names the document in the
/// log ("JWKS", "OIDC discovery"), which both fetches share.
async fn read_capped_body(
    what: &str,
    response: &mut reqwest::Response,
) -> Result<String, JWKServiceError> {
    if let Some(declared) = response.content_length()
        && declared > JWKS_MAX_RESPONSE_BYTES as u64
    {
        warn!("{what} response declares {declared} bytes, over the {JWKS_MAX_RESPONSE_BYTES} cap");
        return Err(JWKServiceError::ResponseTooLarge);
    }

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        warn!("failed to read {what} response body: {e:?}");
        JWKServiceError::InternalError
    })? {
        if body.len() + chunk.len() > JWKS_MAX_RESPONSE_BYTES {
            warn!("{what} response exceeded the {JWKS_MAX_RESPONSE_BYTES} byte cap");
            return Err(JWKServiceError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body).map_err(|e| {
        warn!("{what} response was not valid UTF-8: {e}");
        JWKServiceError::InternalError
    })
}

/// The algorithm assumed for an RSA signing key whose JWK omits `alg`.
///
/// `alg` is OPTIONAL in RFC 7517 §4.4 and some providers omit it, Microsoft Entra ID
/// among them. RS256 is the mandatory-to-implement signing algorithm in `OpenID` Connect
/// Core, which makes it the only defensible assumption. Assuming one algorithm rather
/// than accepting the whole RSA family keeps verification pinned: [`Validation`] is built
/// from this value, so a token header naming anything else is rejected outright.
///
/// [`Validation`]: jsonwebtoken::Validation
const INFERRED_RSA_ALGORITHM: KeyAlgorithm = KeyAlgorithm::RS256;

/// Whether the JWK permits signature verification.
///
/// `use` and `key_ops` are both OPTIONAL (RFC 7517 §4.2, §4.3) and a key stating neither
/// is unrestricted, so absence permits. Only an explicit statement to the contrary — `use`
/// that is not `sig`, or a `key_ops` without `verify` — rejects.
fn permits_signature_verification(jwk: &Jwk) -> bool {
    let use_permits = jwk
        .common
        .public_key_use
        .as_ref()
        .is_none_or(|public_key_use| matches!(public_key_use, PublicKeyUse::Signature));
    let ops_permit = jwk.common.key_operations.as_ref().is_none_or(|operations| {
        operations
            .iter()
            .any(|operation| matches!(operation, KeyOperations::Verify))
    });

    use_permits && ops_permit
}

/// The algorithm to verify tokens signed by this key with, or `None` when the key cannot
/// serve that purpose and should be dropped from the set.
///
/// Inference is confined to RSA keys on purpose. Deriving a symmetric algorithm from an
/// asymmetric key is the classic algorithm-confusion forgery — the public key, which
/// anyone can read from the JWKS, becomes the HMAC secret — so a key that does not say
/// what it is only ever gets the RSA treatment, never HS\*.
///
/// A declared `alg` is taken at its word: the provider stated the key's purpose, and this
/// server has no better information. The `use`/`key_ops` check applies only to inference,
/// where the guess has to be unambiguous to be worth making.
#[lore_macro::test_pub]
fn signature_algorithm(jwk: &Jwk) -> Option<jsonwebtoken::Algorithm> {
    let declared = jwk.common.key_algorithm.or_else(|| {
        let is_rsa = matches!(jwk.algorithm, AlgorithmParameters::RSA(_));
        (is_rsa && permits_signature_verification(jwk)).then_some(INFERRED_RSA_ALGORITHM)
    })?;

    // `KeyAlgorithm` also covers key-management algorithms (`RSA-OAEP` and friends) that
    // have no signing counterpart in `Algorithm`. Those are encryption keys, and they drop
    // out here rather than failing the fetch that carried them.
    jsonwebtoken::Algorithm::from_str(&declared.to_string()).ok()
}

/// Ask `jsonwebtoken` whether it will ever pair this key with this algorithm, rather than
/// keeping a second copy of its key-type table.
///
/// A JWK naming an algorithm from a different family than its `kty` is malformed, and one
/// such pairing matters more than the rest: an asymmetric key labelled with an HMAC
/// algorithm, which is the algorithm-confusion forgery where the public value anyone can
/// read from the JWKS becomes the shared secret.
///
/// `decode` compares the key's family against the validation algorithm *before* it looks at
/// the token at all, so a string that is not a token gets an answer out of it without any
/// crypto running and without any claim being read: a mismatch answers `InvalidAlgorithm`,
/// while a usable pairing gets as far as `InvalidToken`. Going through `decode` rather than
/// `crypto::verify` is what makes that safe — verifying an HMAC signature against an RSA key
/// panics inside `DecodingKey::as_bytes`, and this check running first is precisely what
/// stops it.
///
/// This exists for the diagnostic, not for the guarantee. `decode` enforces the same thing
/// on the request path either way, so if the two checks were ever reordered this would cost
/// a warning at start-up rather than a rejection at verification — which is the right way
/// round, and the reason not to hand-roll the table instead. A table that drifted would
/// refuse keys that are perfectly good.
#[lore_macro::test_pub]
fn key_is_usable_with(key: &DecodingKey, algorithm: jsonwebtoken::Algorithm) -> bool {
    let probe = jsonwebtoken::decode::<serde_json::Value>(
        "not-a-token",
        key,
        &jsonwebtoken::Validation::new(algorithm),
    );

    !matches!(
        probe,
        Err(ref e) if *e.kind() == jsonwebtoken::errors::ErrorKind::InvalidAlgorithm
    )
}

/// One pooled client for every fetch. Building it per request meant a fresh TLS
/// handshake each time and no connection reuse.
fn http_client() -> Result<&'static reqwest::Client, JWKServiceError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = client_builder().build().map_err(|e| {
        warn!("Failed to construct HTTP client: {e:?}");
        JWKServiceError::InternalError
    })?;
    Ok(CLIENT.get_or_init(|| client))
}

/// The client for the OIDC discovery fetch, which follows no redirects.
///
/// The scheme check on the discovered `jwks_uri` ensures that the discovered JWKS URI
/// follows the same security scheme as the original issuer. The issuer can still answer
/// with a 302 to `http://…`, and we want to prevent that class of downgrades by not
/// following redirects.
fn no_redirect_client() -> Result<&'static reqwest::Client, JWKServiceError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| {
            warn!("Failed to construct no-redirect HTTP client: {e:?}");
            JWKServiceError::InternalError
        })?;
    Ok(CLIENT.get_or_init(|| client))
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .user_agent(user_agent())
        .connect_timeout(JWKS_CONNECT_TIMEOUT)
        .timeout(JWKS_REQUEST_TIMEOUT)
}

/// Where the JWKS endpoint came from, which decides how it is fetched.
#[derive(Clone, Copy)]
enum EndpointSource {
    /// Operator-authored `[server.auth.jwk].endpoint`.
    Explicit,
    /// The `jwks_uri` of the issuer's discovery document.
    Discovered,
}

/// The fields this server reads from an OIDC discovery document
/// (RFC 8414 / `OpenID` Connect Discovery §3).
#[derive(Deserialize)]
pub struct DiscoveryDocument {
    pub issuer: String,
    pub jwks_uri: String,
}

/// Fetch `<issuer>/.well-known/openid-configuration`. Redirects are refused, the
/// body is capped, and a document naming any issuer other than `issuer` is rejected.
pub async fn fetch_discovery_document(issuer: &str) -> Result<DiscoveryDocument, JWKServiceError> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let client = no_redirect_client()?;
    let mut response = client.get(&url).send().await.map_err(|e| {
        warn!("failed to fetch OIDC discovery document: {e:?}");
        JWKServiceError::InternalError
    })?;

    let status = response.status();
    let body = read_capped_body("OIDC discovery", &mut response).await?;
    if status.is_redirection() {
        warn!(
            status = %status.as_u16(),
            "OIDC discovery endpoint answered with a redirect."
        );
        return Err(JWKServiceError::InternalError);
    }
    if !status.is_success() {
        warn!(
            status = %status.as_u16(),
            "OIDC discovery endpoint returned error, response: {}",
            body_excerpt(&body)
        );
        return Err(JWKServiceError::InternalError);
    }

    let document: DiscoveryDocument = serde_json::from_str(&body).map_err(|e| {
        warn!(
            "failed to parse OIDC discovery document: {}",
            body_excerpt(&body)
        );
        JWKServiceError::ParseError(e)
    })?;

    // Check that document issuer matches the issuer URL, to avoid redirect
    // attacks.
    if document.issuer != issuer {
        warn!(
            expected = %issuer,
            actual = %document.issuer,
            "OIDC discovery document names a different issuer than the configured one"
        );
        return Err(JWKServiceError::DiscoveryIssuerMismatch {
            expected: issuer.to_string(),
            actual: document.issuer,
        });
    }
    Ok(document)
}

/// Whether `issuer` is an http or https URL, which an issuer whose keys are found through
/// OIDC discovery must be.
pub fn is_issuer_url(issuer: &str) -> bool {
    reqwest::Url::parse(issuer).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// Whether a discovered `jwks_uri` may be fetched from.
///
/// `https` always may. `http` may only when the configured issuer is itself plain
/// `http`. This supports local testing, and the decision is made at server configuration,
/// not through an external discovery document. An `https` issuer's discovery
/// response must never downgrade key retrieval to a connection that can be intercepted.
/// Every other scheme is refused outright: unlike the operator-authored
/// `endpoint` (where `file://` is legitimate), this URL arrives over the network, and
/// following it anywhere else is an SSRF primitive.
#[lore_macro::test_pub]
fn discovered_jwks_uri_scheme_permitted(issuer: &str, jwks_uri: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(jwks_uri) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => issuer.starts_with("http://"),
        _ => false,
    }
}

#[lore_macro::test_pub]
#[derive(Clone, Default)]
pub struct JwkServiceImpl {
    // Shared across clones; refetched from different threads if needed.
    cached_set: Arc<DashMap<String, JWKServiceKey>>,
    /// Held for the duration of a refresh so only one runs at a time. Readers stay
    /// lock-free on `cached_set`; only refreshers contend here.
    refresh: Arc<tokio::sync::Mutex<()>>,
    /// When the last refresh completed, for [`MIN_REFRESH_INTERVAL`]. Separate from
    /// `refresh` so a throttled caller answers without queueing behind a live fetch.
    last_refresh: Arc<std::sync::Mutex<Option<Instant>>>,
    settings: JWKServiceSettings,
    /// The issuer discovery resolves against when no explicit endpoint is configured.
    discovery_issuer: Option<String>,
    /// `jwks_uri` per issuer, so discovery runs once rather than on every key refresh.
    discovered_jwks_uri: Arc<DashMap<String, String>>,
}

impl JwkServiceImpl {
    pub fn new(settings: JWKServiceSettings) -> Self {
        JwkServiceImpl {
            cached_set: Default::default(),
            refresh: Default::default(),
            last_refresh: Default::default(),
            settings,
            discovery_issuer: None,
            discovered_jwks_uri: Default::default(),
        }
    }

    /// Construct with the configured issuers so an unset `endpoint` can be resolved
    /// through OIDC discovery. An explicit `endpoint` wins and discovery is never
    /// attempted. Otherwise the **first** issuer is the discovery target — the list
    /// exists for accepting tokens during an issuer cutover, not for discovering
    /// several providers, and two entries with different discovery documents is a
    /// configuration error.
    pub fn with_issuers(
        settings: JWKServiceSettings,
        issuers: Option<&[String]>,
    ) -> Result<Self, JWKServiceError> {
        let mut service = Self::new(settings);
        if service.settings.endpoint.is_some() {
            info!("JWKS endpoint configured explicitly. OIDC discovery is skipped");
            return Ok(service);
        }

        let issuer = issuers
            .and_then(|issuers| issuers.first())
            .filter(|issuer| is_issuer_url(issuer))
            .ok_or(JWKServiceError::EndpointUnresolvable)?;

        if let Some([_]) = issuers {
            info!(%issuer, "Resolving jwks_uri through OIDC discovery");
        } else {
            info!(
                %issuer,
                "Resolving jwks_uri through OIDC discovery against the first configured issuer"
            );
        }
        service.discovery_issuer = Some(issuer.clone());
        Ok(service)
    }

    /// The URL to fetch the key set from: the explicit endpoint when configured,
    /// otherwise the `jwks_uri` the issuer's discovery document names. The flag says
    /// which, because the two are fetched differently: a discovered endpoint follows
    /// no redirects (see [`no_redirect_client`]), while the operator-authored one
    /// keeps the historical redirect-following behaviour.
    async fn resolve_jwks_endpoint(&self) -> Result<(String, EndpointSource), JWKServiceError> {
        if let Some(endpoint) = self.settings.endpoint.as_ref() {
            return Ok((endpoint.clone(), EndpointSource::Explicit));
        }
        let issuer = self
            .discovery_issuer
            .as_ref()
            .ok_or(JWKServiceError::EndpointUnresolvable)?;
        if let Some(cached) = self.discovered_jwks_uri.get(issuer) {
            return Ok((cached.clone(), EndpointSource::Discovered));
        }

        let document = fetch_discovery_document(issuer).await?;

        if !discovered_jwks_uri_scheme_permitted(issuer, &document.jwks_uri) {
            warn!(
                %issuer,
                jwks_uri = %document.jwks_uri,
                "refusing discovered jwks_uri: keys for an https issuer are only fetched over https"
            );
            return Err(JWKServiceError::JwksUriNotHttps {
                jwks_uri: document.jwks_uri,
            });
        }

        info!(%issuer, jwks_uri = %document.jwks_uri, "Resolved jwks_uri through OIDC discovery");
        self.discovered_jwks_uri
            .insert(issuer.clone(), document.jwks_uri.clone());
        Ok((document.jwks_uri, EndpointSource::Discovered))
    }

    /// Whether a refresh happened too recently to warrant another. Always false while
    /// the cache is empty, so start-up and total-loss recovery are never throttled.
    #[lore_macro::test_pub]
    fn throttled(&self) -> bool {
        if self.cached_set.is_empty() {
            return false;
        }
        self.last_refresh
            .lock()
            .ok()
            .and_then(|last| *last)
            .is_some_and(|at| at.elapsed() < MIN_REFRESH_INTERVAL)
    }

    /// The raw key material behind a key id, for comparing a cache entry across a refresh.
    fn cached_jwk(&self, kid: &str) -> Option<Jwk> {
        self.cached_set.get(kid).map(|key| key.jwk.clone())
    }

    #[lore_macro::test_pub]
    fn mark_refreshed(&self) {
        if let Ok(mut last) = self.last_refresh.lock() {
            *last = Some(Instant::now());
        }
    }

    /// Fetch the latest keys and refresh the local cache.
    ///
    /// Only one refresh runs at a time, so concurrent misses collapse into a single
    /// request and a slow response can never publish its set over a newer one. Once any
    /// key is cached, refreshes are additionally throttled to [`MIN_REFRESH_INTERVAL`];
    /// a throttled or redundant call returns `Ok` without fetching, leaving the caller
    /// to observe the miss through the cache.
    pub async fn fetch_new_keys(&self, desired: Option<&str>) -> Result<(), JWKServiceError> {
        if let Some(desired) = desired
            && self.cached_set.contains_key(desired)
        {
            return Ok(());
        }
        if self.throttled() {
            return Ok(());
        }

        let _refresh = self.refresh.lock().await;

        // Whoever held the lock may have published the key, or refreshed recently
        // enough that another request is not warranted.
        if let Some(desired) = desired
            && self.cached_set.contains_key(desired)
        {
            return Ok(());
        }
        if self.throttled() {
            return Ok(());
        }

        // Record the attempt before making it, so the throttle bounds attempts rather than
        // successes. An endpoint that is failing is exactly when the bound matters, and
        // marking only on success would lift it precisely then: every miss would fetch
        // again, turning an unhealthy provider into a request storm. Marked again on the
        // way out so a healthy interval is measured from completion.
        self.mark_refreshed();

        // At the server's default level, so the warnings of a fetch name the key set they
        // are about, which tells apart those of a server with a separate token exchange
        // issuer. Built only once a fetch is due, which the throttle bounds.
        let span = tracing::warn_span!(
            "jwks",
            issuer = self.discovery_issuer.as_deref(),
            endpoint = self.settings.endpoint.as_deref()
        );
        self.fetch_and_publish().instrument(span).await
    }

    /// Fetch the key set and replace the cache with it. The caller holds `refresh`.
    async fn fetch_and_publish(&self) -> Result<(), JWKServiceError> {
        let (endpoint, endpoint_source) = self.resolve_jwks_endpoint().await?;
        let endpoint = reqwest::Url::parse(&endpoint).map_err(|e| {
            warn!("failed to parse JWKS endpoint as a URL: {e:?}");
            JWKServiceError::InternalError
        })?;

        let is_file = endpoint.scheme() == "file";

        let response_body = if is_file {
            let path = endpoint.to_file_path().map_err(|_err| {
                warn!("failed to resolve JWKS file:// endpoint to a path: {endpoint}");
                JWKServiceError::InternalError
            })?;

            // Sized before reading, for the same reason the HTTP body is capped: the file is
            // configuration this server does not author.
            let size = tokio::fs::metadata(&path)
                .await
                .map_err(|e| {
                    warn!("failed to stat JWKS file at {}: {e:?}", path.display());
                    JWKServiceError::InternalError
                })?
                .len();
            if size > JWKS_MAX_RESPONSE_BYTES as u64 {
                warn!(
                    "JWKS file at {} is {size} bytes, over the {JWKS_MAX_RESPONSE_BYTES} cap",
                    path.display()
                );
                return Err(JWKServiceError::ResponseTooLarge);
            }

            tokio::fs::read_to_string(&path).await.map_err(|e| {
                warn!("failed to read JWKS file at {}: {e:?}", path.display());
                JWKServiceError::InternalError
            })?
        } else {
            // A discovered endpoint follows no redirects for the same reason discovery
            // itself does not: the scheme check on the discovered jwks_uri ensures
            // nothing if a redirect can then steer the key fetch onto a plaintext fetch.
            let client = match endpoint_source {
                EndpointSource::Explicit => http_client()?,
                EndpointSource::Discovered => no_redirect_client()?,
            };

            let mut response = timed!(
                self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
                &self.get_labels_for_operation_context("get_keys"),
                {
                    client.get(endpoint).send().await.map_err(|e| {
                        warn!("failed to fetch JWKS endpoint: {e:?}");
                        JWKServiceError::InternalError
                    })
                }
            )
            .result?;

            let status = response.status();
            let body = read_capped_body("JWKS", &mut response).await?;

            if status.is_redirection() {
                warn!(
                    status = %status.as_u16(),
                    "Discovered JWKS endpoint answered a redirect. Not supported."
                );
                return Err(JWKServiceError::InternalError);
            }
            if !status.is_success() {
                warn!(
                    status = %status.as_u16(),
                    "JWKS endpoint returned error, response: {}",
                    body_excerpt(&body)
                );

                return Err(JWKServiceError::InternalError);
            }

            body
        };

        let new_jwks: JwkSet = serde_json::from_str(response_body.as_str()).map_err(|e| {
            let excerpt = body_excerpt(&response_body);
            if is_file {
                warn!("invalid JWKS file contents: {excerpt}");
            } else {
                warn!("failed to parse JWKS response: {excerpt}");
            }
            JWKServiceError::ParseError(e)
        })?;

        // Build the full set first so a parse failure leaves the cache untouched.
        //
        // A key this server cannot use is skipped, not fatal. A JWKS legitimately carries
        // keys that are none of its business — encryption keys, key types it does not
        // implement — and one of them must not cost every other key in the document, which
        // during start-up is the whole of authentication.
        let mut new_entries: Vec<(String, JWKServiceKey)> = Vec::with_capacity(new_jwks.keys.len());
        for jwk in new_jwks.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                warn!("Skipping JWK with no 'kid': it could never be selected by a token header");
                continue;
            };

            let Some(algorithm) = signature_algorithm(&jwk) else {
                warn!(
                    %kid,
                    "Skipping JWK with no usable signature algorithm: 'alg' is absent and not \
                     inferable, or names an algorithm that does not sign"
                );
                continue;
            };

            let decoding_key = match DecodingKey::from_jwk(&jwk) {
                Ok(decoding_key) => decoding_key,
                Err(e) => {
                    warn!(%kid, "Skipping JWK whose key material could not be decoded: {e:?}");
                    continue;
                }
            };

            if !key_is_usable_with(&decoding_key, algorithm) {
                warn!(
                    %kid,
                    ?algorithm,
                    "Skipping JWK whose algorithm does not belong to its key type: caching it \
                     would reject every token naming this key id, with nothing to say why"
                );
                continue;
            }

            new_entries.push((
                kid,
                JWKServiceKey {
                    decoding_key,
                    jwk,
                    algorithm,
                },
            ));
        }

        // Nothing usable is a failed fetch, not a successful fetch of nothing. Publishing an
        // empty set would evict every cached key, and an empty cache deliberately bypasses
        // the refresh throttle (see `throttled`), so it would cost the bound on outbound
        // requests as well as the keys.
        if new_entries.is_empty() {
            warn!("JWKS endpoint returned no key usable for signature verification");
            return Err(JWKServiceError::NoUsableKeys);
        }

        // Insert the fresh keys, then drop any the endpoint no longer lists. Readers
        // see a superset during the swap, never an empty window (as clear-then-insert
        // would leave), so concurrent lookups never spuriously miss.
        let fresh: HashSet<String> = new_entries.iter().map(|(kid, _)| kid.clone()).collect();
        for (kid, key) in new_entries {
            self.cached_set.insert(kid, key);
        }
        self.cached_set.retain(|kid, _| fresh.contains(kid));
        self.mark_refreshed();

        Ok(())
    }
}

#[async_trait]
impl JWKService for JwkServiceImpl {
    async fn get_key(
        &self,
        kid: &str,
    ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError> {
        if let Some(hit) = self.get_cached_key(kid) {
            return Ok(hit);
        }
        self.fetch_new_keys(Some(kid)).await?;
        self.get_cached_key(kid).ok_or(JWKServiceError::NotFound)
    }

    fn get_cached_key(&self, kid: &str) -> Option<(DecodingKey, jsonwebtoken::Algorithm)> {
        let key = self.cached_set.get(kid)?;
        Some((key.decoding_key.clone(), key.algorithm))
    }

    async fn refresh_key(
        &self,
        kid: &str,
    ) -> Result<Option<(DecodingKey, jsonwebtoken::Algorithm)>, JWKServiceError> {
        let before = self.cached_jwk(kid);
        // `None` rather than `Some(kid)`: the caller already has a key for this id, so the
        // "already cached, nothing to do" short-circuit is the very thing being worked
        // around. The refresh throttle still applies and is what bounds this.
        self.fetch_new_keys(None).await?;
        if self.cached_jwk(kid) == before {
            return Ok(None);
        }

        // Changed — but "changed" includes the endpoint dropping the id altogether, which is
        // a revocation rather than a rotation. There is no replacement to hand back, and the
        // caller's original failure is the right verdict, so it reads the same `None`. Said
        // out loud here because falling into it silently reads like an unchanged key.
        let Some(rotated) = self.get_cached_key(kid) else {
            info!(%kid, "Key id retired by the JWKS endpoint; tokens naming it are no longer accepted");
            return Ok(None);
        };

        Ok(Some(rotated))
    }
}

impl InstrumentProvider for JwkServiceImpl {
    fn namespace(&self) -> &'static str {
        "urc.auth.jwk_service"
    }
}
