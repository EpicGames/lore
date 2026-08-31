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
use lore_transport::grpc::user_agent;
use opentelemetry::KeyValue;
use serde::Deserialize;
use smallvec::SmallVec;
use thiserror::Error;
use tracing::info;
use tracing::warn;

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
    pub endpoint: String,
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
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Cap on the JWKS document this server will hold in memory. Generous beside any real key
/// set — even a large provider publishes single-digit kilobytes — so the only documents it
/// refuses are ones no identity provider would send. [`JWKS_REQUEST_TIMEOUT`] bounds how
/// long a fetch may run, which is not the same as bounding what it delivers.
const JWKS_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// How much of a rejected response body reaches the log. The body is whatever the endpoint
/// chose to send, so it is neither trustworthy nor necessarily small.
const LOGGED_BODY_LIMIT: usize = 512;

/// The head of a response body, for diagnostics.
fn body_excerpt(body: &str) -> String {
    match body.char_indices().nth(LOGGED_BODY_LIMIT) {
        Some((end, _)) => format!("{}… ({} bytes total)", &body[..end], body.len()),
        None => body.to_string(),
    }
}

/// Read a response body, refusing anything past [`JWKS_MAX_RESPONSE_BYTES`].
///
/// `Content-Length` is consulted first when the endpoint offers one, but it is a claim
/// rather than a fact — it can be absent, understated, or the response chunked — so the
/// accumulating read is what actually enforces the cap.
async fn read_capped_body(response: &mut reqwest::Response) -> Result<String, JWKServiceError> {
    if let Some(declared) = response.content_length()
        && declared > JWKS_MAX_RESPONSE_BYTES as u64
    {
        warn!("JWKS response declares {declared} bytes, over the {JWKS_MAX_RESPONSE_BYTES} cap");
        return Err(JWKServiceError::ResponseTooLarge);
    }

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        warn!("failed to read JWKS response body: {e:?}");
        JWKServiceError::InternalError
    })? {
        if body.len() + chunk.len() > JWKS_MAX_RESPONSE_BYTES {
            warn!("JWKS response exceeded the {JWKS_MAX_RESPONSE_BYTES} byte cap");
            return Err(JWKServiceError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body).map_err(|e| {
        warn!("JWKS response was not valid UTF-8: {e}");
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
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .user_agent(user_agent())
        .connect_timeout(JWKS_CONNECT_TIMEOUT)
        .timeout(JWKS_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| {
            warn!("Failed to construct HTTP client: {e:?}");
            JWKServiceError::InternalError
        })?;
    Ok(CLIENT.get_or_init(|| client))
}

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
    #[allow(dead_code)]
    settings: JWKServiceSettings,
}

impl JwkServiceImpl {
    pub fn new(settings: JWKServiceSettings) -> Self {
        JwkServiceImpl {
            cached_set: Default::default(),
            refresh: Default::default(),
            last_refresh: Default::default(),
            settings,
        }
    }

    /// Whether a refresh happened too recently to warrant another. Always false while
    /// the cache is empty, so start-up and total-loss recovery are never throttled.
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

        let endpoint = reqwest::Url::parse(&self.settings.endpoint).map_err(|e| {
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
            let client = http_client()?;

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
            let body = read_capped_body(&mut response).await?;

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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use axum::Json;
    use axum::Router;
    use axum::extract::State;
    use axum::routing::get;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    async fn jwks_handler(State(requests): State<Arc<AtomicUsize>>) -> Json<serde_json::Value> {
        let kid = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
            "old-kid"
        } else {
            "new-kid"
        };

        Json(json!({
            "keys": [{
                "kty": "oct",
                "use": "sig",
                "kid": kid,
                "alg": "HS256",
                "k": "c2VjcmV0"
            }]
        }))
    }

    async fn spawn_jwks_server(requests: Arc<AtomicUsize>) -> SocketAddr {
        let app = Router::new()
            .route("/jwks", get(jwks_handler))
            .with_state(requests);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test jwks server");
        let address = listener.local_addr().expect("get test jwks server address");

        lore_base::lore_spawn!(async move {
            axum::serve(listener, app)
                .await
                .expect("serve test jwks server");
        });

        address
    }

    #[tokio::test]
    async fn fetch_new_keys_refreshes_when_desired_key_is_missing() {
        let requests = Arc::new(AtomicUsize::new(0));
        let address = spawn_jwks_server(requests.clone()).await;
        let service = JwkServiceImpl::new(JWKServiceSettings {
            endpoint: format!("http://{address}/jwks"),
        });

        service
            .fetch_new_keys(None)
            .await
            .expect("initial key fetch should succeed");

        // The refresh throttle would otherwise absorb the second fetch, which is its job and is
        // asserted by `throttled_fetch_does_not_contact_the_endpoint`.
        *service.last_refresh.lock().expect("throttle lock") = None;

        service
            .get_key("new-kid")
            .await
            .expect("missing desired key should trigger a refresh");

        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn loads_keys_from_file_url() {
        let temp_dir = std::env::temp_dir();
        let jwks_path = temp_dir.join("jwk_test_loads_keys_from_file_url.json");

        std::fs::write(
            &jwks_path,
            r#"{"keys":[{"kty":"EC","crv":"P-256","x":"MKBCTNIcKUSDii11ySs3526iDZ8AiTo7Tu6KPAqv7D4","y":"4Etl6SRW2YiLUrN5vfvVHuhp7x8PxltmWWlbbM4IFGI","alg":"ES256","kid":"test-key-1"}]}"#,
        )
        .unwrap();

        let endpoint = reqwest::Url::from_file_path(&jwks_path)
            .unwrap()
            .to_string();
        let settings = JWKServiceSettings { endpoint };
        let service = JwkServiceImpl::new(settings);

        let result = service.fetch_new_keys(None).await;
        assert!(result.is_ok(), "{result:?}");

        let (_, algorithm) = service
            .get_key("test-key-1")
            .await
            .expect("key should be cached after loading from file");
        assert_eq!(algorithm, jsonwebtoken::Algorithm::ES256);

        std::fs::remove_file(&jwks_path).ok();
    }

    #[tokio::test]
    async fn file_url_missing_file_returns_error() {
        let settings = JWKServiceSettings {
            endpoint: "file:///tmp/jwk_test_file_that_does_not_exist.json".to_string(),
        };
        let service = JwkServiceImpl::new(settings);

        let result = service.fetch_new_keys(None).await;

        assert!(result.is_err());
    }

    fn service() -> JwkServiceImpl {
        JwkServiceImpl::new(JWKServiceSettings {
            endpoint: "http://127.0.0.1:1/jwks".to_string(),
        })
    }

    fn cache_a_key(service: &JwkServiceImpl) {
        let jwk: Jwk = serde_json::from_str(
            r#"{"kty":"oct","alg":"HS256","kid":"cached","k":"c2VjcmV0LWtleS1tYXRlcmlhbA"}"#,
        )
        .expect("parse test jwk");
        service.cached_set.insert(
            "cached".to_string(),
            JWKServiceKey {
                decoding_key: DecodingKey::from_secret(b"secret-key-material"),
                algorithm: jsonwebtoken::Algorithm::HS256,
                jwk,
            },
        );
    }

    /// An empty cache must never be throttled, or a server that has not yet fetched (or
    /// lost every key) could never recover.
    #[test]
    fn empty_cache_is_never_throttled() {
        let service = service();
        service.mark_refreshed();
        assert!(!service.throttled());
    }

    /// The throttle is what bounds outbound requests when an unauthenticated caller
    /// cycles unknown key ids, so a recent refresh must suppress the next fetch.
    #[test]
    fn recent_refresh_throttles_while_keys_are_cached() {
        let service = service();
        cache_a_key(&service);
        assert!(!service.throttled(), "no refresh recorded yet");
        service.mark_refreshed();
        assert!(service.throttled());
    }

    /// A throttled fetch reports success without touching the network; the caller sees
    /// the miss through the cache instead. The endpoint here would fail if contacted.
    #[tokio::test]
    async fn throttled_fetch_does_not_contact_the_endpoint() {
        let service = service();
        cache_a_key(&service);
        service.mark_refreshed();

        service
            .fetch_new_keys(Some("absent"))
            .await
            .expect("throttled fetch returns Ok without fetching");
        assert!(service.get_cached_key("absent").is_none());
    }

    /// The refresh has to reach the endpoint even though the key id is cached — being
    /// cached is exactly the condition a key rotated under an unchanged id produces. The
    /// endpoint here refuses connections, so an error is the proof that it was contacted;
    /// `fetch_new_keys(Some("cached"))` returns `Ok` without going near it.
    #[tokio::test]
    async fn refresh_fetches_even_though_the_kid_is_cached() {
        let service = service();
        cache_a_key(&service);

        assert!(
            service.refresh_key("cached").await.is_err(),
            "refresh must attempt a fetch and fail against a dead endpoint"
        );
    }

    /// And it is bounded by the same throttle as every other fetch. This one matters: a bad
    /// signature against a known key id is what triggers a refresh, and anyone can send one.
    #[tokio::test]
    async fn refresh_is_throttled_like_any_other_fetch() {
        let service = service();
        cache_a_key(&service);
        service.mark_refreshed();

        let refreshed = service.refresh_key("cached").await;
        assert!(
            matches!(refreshed, Ok(None)),
            "a throttled refresh reports no change without contacting the endpoint"
        );
    }

    /// The key must survive a refresh that fails or is throttled. Dropping it would empty
    /// the cache, and an empty cache is deliberately never throttled — so an evicting
    /// refresh would hand an unauthenticated caller a way to drive outbound requests.
    #[tokio::test]
    async fn refresh_never_evicts_the_key_it_was_checking() {
        let service = service();
        cache_a_key(&service);

        let _ = service.refresh_key("cached").await;
        assert!(service.get_cached_key("cached").is_some());

        service.mark_refreshed();
        let _ = service.refresh_key("cached").await;
        assert!(service.get_cached_key("cached").is_some());
        assert!(service.throttled(), "still throttled, so still bounded");
    }

    /// A cached key must short-circuit before any network work.
    #[tokio::test]
    async fn cached_key_short_circuits_fetch() {
        let service = service();
        cache_a_key(&service);

        service
            .fetch_new_keys(Some("cached"))
            .await
            .expect("cached kid needs no fetch");
        assert!(service.get_cached_key("cached").is_some());
    }

    /// The throttle has to bound failed fetches too. Marking only successes would lift the
    /// bound exactly when the provider is unhealthy — every miss would attempt again — which
    /// is the request storm the throttle exists to prevent.
    #[tokio::test]
    async fn failed_fetch_is_throttled_like_a_successful_one() {
        let service = service();
        cache_a_key(&service);

        assert!(
            service.fetch_new_keys(Some("absent")).await.is_err(),
            "the fetch is attempted against a dead endpoint and fails"
        );
        assert!(
            service.throttled(),
            "a failed attempt still opens the throttle window"
        );
    }

    /// Modulus and exponent of the RSA example key from RFC 7515 Appendix A.2. Only their
    /// encoding matters here; nothing in these tests verifies a signature.
    const RSA_N: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4\
                         cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiF\
                         V4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6C\
                         f0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9\
                         c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTW\
                         hAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1\
                         jF44-csFCur-kEgU8awapJzKnqDKgw";
    const RSA_E: &str = "AQAB";

    fn rsa_jwk(extra_fields: &str) -> Jwk {
        let json = format!(r#"{{"kty":"RSA",{extra_fields}"n":"{RSA_N}","e":"{RSA_E}"}}"#);
        serde_json::from_str(&json).expect("parse test RSA jwk")
    }

    /// The case the inference exists for: providers such as Microsoft Entra ID publish
    /// signing keys with no `alg`.
    #[test]
    fn rsa_signing_key_without_alg_infers_rs256() {
        let jwk = rsa_jwk(r#""use":"sig","kid":"k","#);
        assert_eq!(
            signature_algorithm(&jwk),
            Some(jsonwebtoken::Algorithm::RS256)
        );
    }

    /// `use` is as optional as `alg` (RFC 7517 §4.2), so a key declaring neither is
    /// unrestricted and still inferable. Requiring `use` would leave such providers broken.
    #[test]
    fn rsa_key_without_alg_or_use_infers_rs256() {
        let jwk = rsa_jwk(r#""kid":"k","#);
        assert_eq!(
            signature_algorithm(&jwk),
            Some(jsonwebtoken::Algorithm::RS256)
        );
    }

    /// `key_ops` is the other way RFC 7517 states that a key verifies signatures.
    #[test]
    fn rsa_key_with_verify_key_ops_infers_rs256() {
        let jwk = rsa_jwk(r#""key_ops":["verify"],"kid":"k","#);
        assert_eq!(
            signature_algorithm(&jwk),
            Some(jsonwebtoken::Algorithm::RS256)
        );
    }

    /// A key the provider marked as an encryption key must never be inferred into a
    /// verification key, whichever field carries the statement.
    #[test]
    fn rsa_encryption_key_without_alg_is_not_inferred() {
        assert_eq!(
            signature_algorithm(&rsa_jwk(r#""use":"enc","kid":"k","#)),
            None
        );
        assert_eq!(
            signature_algorithm(&rsa_jwk(r#""key_ops":["encrypt"],"kid":"k","#)),
            None
        );
    }

    /// The forgery this inference must never enable. An RSA public key is published to the
    /// world; inferring an HMAC algorithm for it would make that public value the shared
    /// secret and let anyone mint tokens. Inference is RSA-only for exactly this reason, so
    /// a symmetric key that omits `alg` has to drop out.
    #[test]
    fn symmetric_key_without_alg_is_not_inferred() {
        let jwk: Jwk =
            serde_json::from_str(r#"{"kty":"oct","use":"sig","kid":"k","k":"c2VjcmV0"}"#)
                .expect("parse test oct jwk");
        assert_eq!(signature_algorithm(&jwk), None);
    }

    /// EC keys bind the hash to the curve, so an EC key without `alg` is not a guess worth
    /// making — P-256 with ES384 is a different key, not a different preference.
    #[test]
    fn ec_key_without_alg_is_not_inferred() {
        let jwk: Jwk = serde_json::from_str(
            r#"{"kty":"EC","crv":"P-256","kid":"k","x":"MKBCTNIcKUSDii11ySs3526iDZ8AiTo7Tu6KPAqv7D4","y":"4Etl6SRW2YiLUrN5vfvVHuhp7x8PxltmWWlbbM4IFGI"}"#,
        )
        .expect("parse test EC jwk");
        assert_eq!(signature_algorithm(&jwk), None);
    }

    /// A key-management algorithm is not a signing algorithm. These have no counterpart in
    /// `Algorithm`, and before the set was built key-by-key one of them failed the entire
    /// fetch rather than just itself.
    #[test]
    fn key_management_algorithm_is_not_a_signature_algorithm() {
        let jwk = rsa_jwk(r#""use":"enc","alg":"RSA-OAEP","kid":"k","#);
        assert_eq!(signature_algorithm(&jwk), None);
    }

    /// A declared `alg` is honoured rather than replaced by the inferred default.
    #[test]
    fn declared_alg_wins_over_inference() {
        let jwk = rsa_jwk(r#""use":"sig","alg":"PS256","kid":"k","#);
        assert_eq!(
            signature_algorithm(&jwk),
            Some(jsonwebtoken::Algorithm::PS256)
        );
    }

    const EC_X: &str = "MKBCTNIcKUSDii11ySs3526iDZ8AiTo7Tu6KPAqv7D4";
    const EC_Y: &str = "4Etl6SRW2YiLUrN5vfvVHuhp7x8PxltmWWlbbM4IFGI";
    const ED_X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";

    fn rsa_key() -> DecodingKey {
        DecodingKey::from_rsa_components(RSA_N, RSA_E).expect("rsa decoding key")
    }

    fn oct_key() -> DecodingKey {
        DecodingKey::from_secret(b"secret-key-material")
    }

    fn ec_key() -> DecodingKey {
        DecodingKey::from_ec_components(EC_X, EC_Y).expect("ec decoding key")
    }

    fn ed_key() -> DecodingKey {
        DecodingKey::from_ed_components(ED_X).expect("ed decoding key")
    }

    /// The malformed pairing that matters. An RSA key labelled with an HMAC algorithm is the
    /// algorithm-confusion setup: the modulus is published to the world, so treating it as an
    /// HMAC secret would let anyone who can read the JWKS mint tokens.
    #[test]
    fn an_rsa_key_is_not_usable_with_an_hmac_algorithm() {
        assert!(!key_is_usable_with(
            &rsa_key(),
            jsonwebtoken::Algorithm::HS256
        ));
    }

    /// And the mirror image, malformed for the same reason in the other direction.
    #[test]
    fn a_symmetric_key_is_not_usable_with_an_rsa_algorithm() {
        assert!(!key_is_usable_with(
            &oct_key(),
            jsonwebtoken::Algorithm::RS256
        ));
    }

    /// An EC key named with an RSA algorithm is equally unusable.
    #[test]
    fn an_ec_key_is_not_usable_with_an_rsa_algorithm() {
        assert!(!key_is_usable_with(
            &ec_key(),
            jsonwebtoken::Algorithm::RS256
        ));
    }

    /// An upgrade canary rather than a test of logic this server owns. The load-time check
    /// asks `jsonwebtoken` which pairings it will accept, so this pins that answer: if a
    /// future version widened or narrowed it, or moved the family check after the token is
    /// parsed, this fails here rather than somewhere further from the cause.
    ///
    /// The probe answers from the key's family alone, so it does not care that none of these
    /// keys ever signed anything.
    #[test]
    fn jsonwebtoken_pairs_each_algorithm_with_one_key_type() {
        use jsonwebtoken::Algorithm;

        const EVERY_ALGORITHM: [Algorithm; 12] = [
            Algorithm::HS256,
            Algorithm::HS384,
            Algorithm::HS512,
            Algorithm::ES256,
            Algorithm::ES384,
            Algorithm::RS256,
            Algorithm::RS384,
            Algorithm::RS512,
            Algorithm::PS256,
            Algorithm::PS384,
            Algorithm::PS512,
            Algorithm::EdDSA,
        ];

        let cases: [(&str, DecodingKey, &[Algorithm]); 4] = [
            (
                "RSA",
                rsa_key(),
                &[
                    Algorithm::RS256,
                    Algorithm::RS384,
                    Algorithm::RS512,
                    Algorithm::PS256,
                    Algorithm::PS384,
                    Algorithm::PS512,
                ],
            ),
            (
                "oct",
                oct_key(),
                &[Algorithm::HS256, Algorithm::HS384, Algorithm::HS512],
            ),
            ("EC", ec_key(), &[Algorithm::ES256, Algorithm::ES384]),
            ("OKP", ed_key(), &[Algorithm::EdDSA]),
        ];

        for (key_type, key, usable) in &cases {
            for algorithm in EVERY_ALGORITHM {
                assert_eq!(
                    key_is_usable_with(key, algorithm),
                    usable.contains(&algorithm),
                    "{algorithm:?} against a {key_type} key"
                );
            }
        }
    }

    /// A body longer than the log excerpt is cut, and says so.
    #[test]
    fn a_long_body_is_excerpted_for_the_log() {
        let short = "not a jwks";
        assert_eq!(body_excerpt(short), short);

        let long = "x".repeat(LOGGED_BODY_LIMIT * 3);
        let excerpt = body_excerpt(&long);
        assert!(excerpt.len() < long.len());
        assert!(excerpt.contains(&format!("({} bytes total)", long.len())));
    }

    /// Multi-byte characters must not be split when excerpting, or the log line panics.
    #[test]
    fn excerpting_respects_character_boundaries() {
        let body = "é".repeat(LOGGED_BODY_LIMIT * 2);
        let excerpt = body_excerpt(&body);
        assert!(excerpt.contains('é'));
    }

    /// A service whose endpoint is a `file://` URL, which exercises the whole
    /// parse-and-publish path without standing up a server.
    fn service_over_jwks(name: &str, jwks: &str) -> (JwkServiceImpl, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("jwk_test_{name}.json"));
        std::fs::write(&path, jwks).expect("write test jwks");
        let endpoint = reqwest::Url::from_file_path(&path)
            .expect("jwks path as a file url")
            .to_string();

        (JwkServiceImpl::new(JWKServiceSettings { endpoint }), path)
    }

    /// One unusable key must not cost the usable ones. A JWKS carrying an encryption key
    /// beside a signing key is ordinary, and at start-up failing the fetch over it takes
    /// down every key the server has.
    #[tokio::test]
    async fn unusable_key_does_not_discard_the_rest() {
        let (service, path) = service_over_jwks(
            "unusable_key_does_not_discard_the_rest",
            &format!(
                r#"{{"keys":[
                    {{"kty":"RSA","use":"enc","alg":"RSA-OAEP","kid":"enc","n":"{RSA_N}","e":"{RSA_E}"}},
                    {{"kty":"RSA","use":"sig","kid":"sig","n":"{RSA_N}","e":"{RSA_E}"}}
                ]}}"#
            ),
        );

        service
            .fetch_new_keys(None)
            .await
            .expect("the signing key is usable, so the fetch succeeds");

        assert!(
            service.get_cached_key("enc").is_none(),
            "the encryption key is skipped"
        );
        let (_, algorithm) = service
            .get_cached_key("sig")
            .expect("the signing key survives its neighbour");
        assert_eq!(algorithm, jsonwebtoken::Algorithm::RS256);

        std::fs::remove_file(&path).ok();
    }

    /// The load-time check earns its keep here. Without it this key is cached and rejects
    /// every token naming its id at request time, with nothing in the log pointing at which
    /// key is misconfigured.
    #[tokio::test]
    async fn a_key_whose_algorithm_does_not_match_its_type_is_skipped() {
        let (service, path) = service_over_jwks(
            "a_key_whose_algorithm_does_not_match_its_type_is_skipped",
            &format!(
                r#"{{"keys":[
                    {{"kty":"RSA","use":"sig","alg":"HS256","kid":"confused","n":"{RSA_N}","e":"{RSA_E}"}},
                    {{"kty":"RSA","use":"sig","kid":"sig","n":"{RSA_N}","e":"{RSA_E}"}}
                ]}}"#
            ),
        );

        service
            .fetch_new_keys(None)
            .await
            .expect("the well-formed key is usable, so the fetch succeeds");

        assert!(
            service.get_cached_key("confused").is_none(),
            "an RSA key labelled HS256 must not be cached"
        );
        assert!(service.get_cached_key("sig").is_some());

        std::fs::remove_file(&path).ok();
    }

    /// A key with no `kid` can never be selected by a token header, so it is skipped rather
    /// than failing the document that carried it.
    #[tokio::test]
    async fn key_without_kid_is_skipped() {
        let (service, path) = service_over_jwks(
            "key_without_kid_is_skipped",
            &format!(
                r#"{{"keys":[
                    {{"kty":"RSA","use":"sig","n":"{RSA_N}","e":"{RSA_E}"}},
                    {{"kty":"RSA","use":"sig","kid":"sig","n":"{RSA_N}","e":"{RSA_E}"}}
                ]}}"#
            ),
        );

        service
            .fetch_new_keys(None)
            .await
            .expect("the identified key is usable");
        assert!(service.get_cached_key("sig").is_some());

        std::fs::remove_file(&path).ok();
    }

    /// Nothing usable is a failed fetch, not a successful fetch of nothing. Publishing an
    /// empty set would evict the working keys, and an empty cache is deliberately never
    /// throttled — so it would cost the bound on outbound requests along with the keys.
    #[tokio::test]
    async fn no_usable_keys_errors_and_keeps_the_cache() {
        let (service, path) = service_over_jwks(
            "no_usable_keys_errors_and_keeps_the_cache",
            &format!(
                r#"{{"keys":[{{"kty":"RSA","use":"enc","alg":"RSA-OAEP","kid":"enc","n":"{RSA_N}","e":"{RSA_E}"}}]}}"#
            ),
        );
        cache_a_key(&service);

        let result = service.fetch_new_keys(None).await;
        assert!(
            matches!(result, Err(JWKServiceError::NoUsableKeys)),
            "{result:?}"
        );
        assert!(
            service.get_cached_key("cached").is_some(),
            "the previously working key survives a useless fetch"
        );

        std::fs::remove_file(&path).ok();
    }

    fn jwks_with(kids: &[&str]) -> String {
        let keys: Vec<String> = kids
            .iter()
            .map(|kid| {
                format!(r#"{{"kty":"RSA","use":"sig","kid":"{kid}","n":"{RSA_N}","e":"{RSA_E}"}}"#)
            })
            .collect();
        format!(r#"{{"keys":[{}]}}"#, keys.join(","))
    }

    /// Let the throttle lapse without waiting out [`MIN_REFRESH_INTERVAL`] in real time.
    fn expire_the_throttle(service: &JwkServiceImpl) {
        let long_ago = Instant::now()
            .checked_sub(MIN_REFRESH_INTERVAL + Duration::from_secs(1))
            .expect("a clock far enough from its origin to subtract from");
        *service.last_refresh.lock().expect("throttle lock") = Some(long_ago);
    }

    /// Revocation has to take effect. A key the endpoint has stopped listing must stop
    /// verifying tokens, or withdrawing a compromised key would not withdraw anything.
    #[tokio::test]
    async fn a_key_the_endpoint_stopped_serving_is_dropped() {
        let (service, path) = service_over_jwks(
            "a_key_the_endpoint_stopped_serving_is_dropped",
            &jwks_with(&["old", "keep"]),
        );

        service.fetch_new_keys(None).await.expect("initial fetch");
        assert!(service.get_cached_key("old").is_some());

        std::fs::write(&path, jwks_with(&["keep"])).expect("rewrite test jwks");
        expire_the_throttle(&service);
        service.fetch_new_keys(None).await.expect("second fetch");

        assert!(
            service.get_cached_key("old").is_none(),
            "a revoked key must not survive a refresh"
        );
        assert!(
            service.get_cached_key("keep").is_some(),
            "and the keys still served must"
        );

        std::fs::remove_file(&path).ok();
    }

    /// The counterpart on the refresh path: an id the endpoint dropped reports no key rather
    /// than looking like a key that did not change.
    #[tokio::test]
    async fn refreshing_a_revoked_key_reports_no_key() {
        let (service, path) = service_over_jwks(
            "refreshing_a_revoked_key_reports_no_key",
            &jwks_with(&["going", "staying"]),
        );

        service.fetch_new_keys(None).await.expect("initial fetch");
        std::fs::write(&path, jwks_with(&["staying"])).expect("rewrite test jwks");
        expire_the_throttle(&service);

        let refreshed = service.refresh_key("going").await.expect("refresh runs");
        assert!(refreshed.is_none(), "there is no replacement to offer");
        assert!(service.get_cached_key("going").is_none());

        std::fs::remove_file(&path).ok();
    }

    /// The throttle has to lapse, or the first fetch after start-up would be the last one and
    /// no rotation would ever be picked up.
    #[tokio::test]
    async fn the_throttle_lapses_after_the_interval() {
        let (service, path) = service_over_jwks(
            "the_throttle_lapses_after_the_interval",
            &jwks_with(&["sig"]),
        );

        service.fetch_new_keys(None).await.expect("initial fetch");
        assert!(service.throttled(), "a fresh fetch throttles the next one");

        expire_the_throttle(&service);
        assert!(!service.throttled(), "and stops throttling once it lapses");

        std::fs::remove_file(&path).ok();
    }

    /// A JWKS listing the same id twice is malformed. Last-one-wins is arbitrary but has to be
    /// deliberate: silently keeping the other one would make which key verifies depend on
    /// document order in a way nobody had decided.
    #[tokio::test]
    async fn a_duplicate_kid_keeps_the_last_key_listed() {
        let (service, path) = service_over_jwks(
            "a_duplicate_kid_keeps_the_last_key_listed",
            &format!(
                r#"{{"keys":[
                    {{"kty":"RSA","use":"sig","alg":"RS256","kid":"dup","n":"{RSA_N}","e":"{RSA_E}"}},
                    {{"kty":"RSA","use":"sig","alg":"PS256","kid":"dup","n":"{RSA_N}","e":"{RSA_E}"}}
                ]}}"#
            ),
        );

        service.fetch_new_keys(None).await.expect("fetch");

        let (_, algorithm) = service
            .get_cached_key("dup")
            .expect("one of them is cached");
        assert_eq!(algorithm, jsonwebtoken::Algorithm::PS256);

        std::fs::remove_file(&path).ok();
    }

    /// A JWKS file larger than the cap is refused without being read into memory.
    #[tokio::test]
    async fn an_oversized_jwks_file_is_refused() {
        let (service, path) = service_over_jwks(
            "an_oversized_jwks_file_is_refused",
            &format!(
                r#"{{"keys":[],"padding":"{}"}}"#,
                "x".repeat(JWKS_MAX_RESPONSE_BYTES)
            ),
        );

        let result = service.fetch_new_keys(None).await;
        assert!(
            matches!(result, Err(JWKServiceError::ResponseTooLarge)),
            "{result:?}"
        );

        std::fs::remove_file(&path).ok();
    }

    #[derive(Clone)]
    struct CountingState {
        requests: Arc<AtomicUsize>,
        body: Arc<String>,
        delay: Duration,
    }

    async fn counting_handler(State(state): State<CountingState>) -> String {
        state.requests.fetch_add(1, Ordering::SeqCst);
        if !state.delay.is_zero() {
            tokio::time::sleep(state.delay).await;
        }
        (*state.body).clone()
    }

    async fn spawn_counting_jwks_server(state: CountingState) -> SocketAddr {
        let app = Router::new()
            .route("/jwks", get(counting_handler))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind counting jwks server");
        let address = listener.local_addr().expect("counting jwks server address");

        lore_base::lore_spawn!(async move {
            axum::serve(listener, app)
                .await
                .expect("serve counting jwks server");
        });

        address
    }

    /// The documented reason the refresh mutex exists: concurrent misses have to collapse into
    /// one request. Without it a burst of unknown key ids is a burst of outbound requests, and
    /// the throttle alone does not prevent that — every one of them passes the check before any
    /// of them has recorded an attempt.
    #[tokio::test]
    async fn concurrent_misses_collapse_into_one_request() {
        let requests = Arc::new(AtomicUsize::new(0));
        let address = spawn_counting_jwks_server(CountingState {
            requests: requests.clone(),
            body: Arc::new(jwks_with(&["sig"])),
            delay: Duration::from_millis(150),
        })
        .await;
        let service = JwkServiceImpl::new(JWKServiceSettings {
            endpoint: format!("http://{address}/jwks"),
        });

        let attempts: Vec<_> = (0..8)
            .map(|i| {
                let service = service.clone();
                lore_base::lore_spawn!(async move {
                    service.fetch_new_keys(Some(&format!("absent-{i}"))).await
                })
            })
            .collect();
        for attempt in attempts {
            attempt.await.expect("task joins").expect("fetch succeeds");
        }

        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "eight concurrent misses must cost one request"
        );
    }

    /// A response that never declares its length, so the cap cannot be enforced from the
    /// header and the accumulating read has to do it. This is the case that matters: an
    /// endpoint that means harm simply omits `Content-Length` or understates it.
    async fn chunked_oversized_handler() -> axum::response::Response {
        let chunks = (0..(JWKS_MAX_RESPONSE_BYTES / 1024) + 2)
            .map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024]));

        axum::response::Response::new(axum::body::Body::from_stream(futures::stream::iter(chunks)))
    }

    async fn spawn_chunked_oversized_server() -> SocketAddr {
        let app = Router::new().route("/jwks", get(chunked_oversized_handler));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind chunked jwks server");
        let address = listener.local_addr().expect("chunked jwks server address");

        lore_base::lore_spawn!(async move {
            axum::serve(listener, app)
                .await
                .expect("serve chunked jwks server");
        });

        address
    }

    /// The cap holds even when the endpoint declares no length at all.
    #[tokio::test]
    async fn an_oversized_chunked_response_is_refused_while_reading() {
        let address = spawn_chunked_oversized_server().await;
        let service = JwkServiceImpl::new(JWKServiceSettings {
            endpoint: format!("http://{address}/jwks"),
        });

        let result = service.fetch_new_keys(None).await;
        assert!(
            matches!(result, Err(JWKServiceError::ResponseTooLarge)),
            "a body with no declared length must still be capped: {result:?}"
        );
    }

    /// An oversized HTTP response is refused, and the cached keys are not disturbed by it.
    #[tokio::test]
    async fn an_oversized_jwks_response_is_refused() {
        let requests = Arc::new(AtomicUsize::new(0));
        let address = spawn_counting_jwks_server(CountingState {
            requests: requests.clone(),
            body: Arc::new(format!(
                r#"{{"keys":[],"padding":"{}"}}"#,
                "x".repeat(JWKS_MAX_RESPONSE_BYTES)
            )),
            delay: Duration::ZERO,
        })
        .await;
        let service = JwkServiceImpl::new(JWKServiceSettings {
            endpoint: format!("http://{address}/jwks"),
        });
        cache_a_key(&service);

        let result = service.fetch_new_keys(None).await;
        assert!(
            matches!(result, Err(JWKServiceError::ResponseTooLarge)),
            "{result:?}"
        );
        assert!(
            service.get_cached_key("cached").is_some(),
            "an oversized response must not cost the cached keys"
        );
    }
}
