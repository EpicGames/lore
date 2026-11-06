// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::jwk::Jwk;
use lore_server::auth::jwk::*;
use serde_json::json;
use tokio::net::TcpListener;

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
        endpoint: Some(format!("http://{address}/jwks")),
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
    let temp_dir = lore_base::test_util::TempDir::new("jwk-test-file-url-");
    let jwks_path = temp_dir.child("jwks.json");

    std::fs::write(
        &jwks_path,
        r#"{"keys":[{"kty":"EC","crv":"P-256","x":"MKBCTNIcKUSDii11ySs3526iDZ8AiTo7Tu6KPAqv7D4","y":"4Etl6SRW2YiLUrN5vfvVHuhp7x8PxltmWWlbbM4IFGI","alg":"ES256","kid":"test-key-1"}]}"#,
    )
    .unwrap();

    let endpoint = reqwest::Url::from_file_path(&jwks_path)
        .unwrap()
        .to_string();
    let settings = JWKServiceSettings {
        endpoint: Some(endpoint),
    };
    let service = JwkServiceImpl::new(settings);

    let result = service.fetch_new_keys(None).await;
    assert!(result.is_ok(), "{result:?}");

    let (_, algorithm) = service
        .get_key("test-key-1")
        .await
        .expect("key should be cached after loading from file");
    assert_eq!(algorithm, jsonwebtoken::Algorithm::ES256);
}

#[tokio::test]
async fn file_url_missing_file_returns_error() {
    let settings = JWKServiceSettings {
        endpoint: Some("file:///tmp/jwk_test_file_that_does_not_exist.json".to_string()),
    };
    let service = JwkServiceImpl::new(settings);

    let result = service.fetch_new_keys(None).await;

    assert!(result.is_err());
}

fn service() -> JwkServiceImpl {
    JwkServiceImpl::new(JWKServiceSettings {
        endpoint: Some("http://127.0.0.1:1/jwks".to_string()),
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
    let jwk: Jwk = serde_json::from_str(r#"{"kty":"oct","use":"sig","kid":"k","k":"c2VjcmV0"}"#)
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

/// A service whose endpoint is a `file://` URL, which exercises the whole
/// parse-and-publish path without standing up a server.
/// Returns the temp directory alongside the service: it owns the jwks file,
/// so the caller has to hold it for as long as the service is used.
fn service_over_jwks(
    name: &str,
    jwks: &str,
) -> (
    JwkServiceImpl,
    std::path::PathBuf,
    lore_base::test_util::TempDir,
) {
    let dir = lore_base::test_util::TempDir::new(&format!("jwk-test-{name}-"));
    let path = dir.child("jwks.json");
    std::fs::write(&path, jwks).expect("write test jwks");
    let endpoint = reqwest::Url::from_file_path(&path)
        .expect("jwks path as a file url")
        .to_string();

    (
        JwkServiceImpl::new(JWKServiceSettings {
            endpoint: Some(endpoint),
        }),
        path,
        dir,
    )
}

/// One unusable key must not cost the usable ones. A JWKS carrying an encryption key
/// beside a signing key is ordinary, and at start-up failing the fetch over it takes
/// down every key the server has.
#[tokio::test]
async fn unusable_key_does_not_discard_the_rest() {
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
    let (service, path, _dir) = service_over_jwks(
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
        endpoint: Some(format!("http://{address}/jwks")),
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
        endpoint: Some(format!("http://{address}/jwks")),
    });

    let result = service.fetch_new_keys(None).await;
    assert!(
        matches!(result, Err(JWKServiceError::ResponseTooLarge)),
        "a body with no declared length must still be capped: {result:?}"
    );
}

/// A provider stub serving both the discovery document and the key set, counting
/// requests to each. `issuer_override` lets a test serve a document that vouches
/// for someone else. `oversized` pads the document past the response cap.
struct DiscoveryProvider {
    base: String,
    discovery_requests: Arc<AtomicUsize>,
    jwks_requests: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct DiscoveryState {
    base: String,
    issuer_override: Option<String>,
    jwks_uri_override: Option<String>,
    oversized: bool,
    redirect: bool,
    redirect_jwks: bool,
    jwks: &'static str,
    discovery_requests: Arc<AtomicUsize>,
    jwks_requests: Arc<AtomicUsize>,
}

async fn discovery_handler(State(state): State<DiscoveryState>) -> axum::response::Response {
    use axum::response::IntoResponse;

    state.discovery_requests.fetch_add(1, Ordering::SeqCst);
    if state.redirect {
        // Points at a route that serves a perfectly valid document, so a client
        // that followed redirects would succeed — refusing is the test.
        return axum::response::Redirect::temporary(&format!("{}/moved-discovery", state.base))
            .into_response();
    }
    if state.oversized {
        return format!(
            r#"{{"issuer":"{}","jwks_uri":"{}/jwks","padding":"{}"}}"#,
            state.base,
            state.base,
            "x".repeat(JWKS_MAX_RESPONSE_BYTES)
        )
        .into_response();
    }
    let issuer = state.issuer_override.as_ref().unwrap_or(&state.base);
    let default_jwks_uri = format!("{}/jwks", state.base);
    let jwks_uri = state
        .jwks_uri_override
        .as_ref()
        .unwrap_or(&default_jwks_uri);
    format!(r#"{{"issuer":"{issuer}","jwks_uri":"{jwks_uri}"}}"#).into_response()
}

/// The target of the redirecting discovery route: a valid document for this
/// provider, so only the refusal to follow explains a failed fetch.
async fn moved_discovery_handler(State(state): State<DiscoveryState>) -> String {
    let default_jwks_uri = format!("{}/jwks", state.base);
    format!(
        r#"{{"issuer":"{}","jwks_uri":"{default_jwks_uri}"}}"#,
        state.base
    )
}

const STUB_JWKS: &str =
    r#"{"keys":[{"kty":"oct","use":"sig","kid":"sig","alg":"HS256","k":"dGhlLXNlY3JldA"}]}"#;

/// One HS256 key, so a discovery test can verify a real token end to end.
/// `dGhlLXNlY3JldA` is `the-secret`.
async fn discovered_jwks_handler(State(state): State<DiscoveryState>) -> axum::response::Response {
    use axum::response::IntoResponse;

    state.jwks_requests.fetch_add(1, Ordering::SeqCst);
    if state.redirect_jwks {
        // Like the discovery redirect: the target serves perfectly valid keys, so
        // only the client's refusal to follow explains a failed fetch.
        return axum::response::Redirect::temporary(&format!("{}/moved-jwks", state.base))
            .into_response();
    }
    state.jwks.to_string().into_response()
}

async fn moved_jwks_handler() -> String {
    STUB_JWKS.to_string()
}

async fn spawn_discovery_provider(
    issuer_override: Option<String>,
    oversized: bool,
) -> DiscoveryProvider {
    spawn_discovery_provider_serving(issuer_override, None, oversized, false, false).await
}

/// A provider serving `jwks` in place of [`STUB_JWKS`].
async fn spawn_discovery_provider_with_jwks(jwks: &'static str) -> DiscoveryProvider {
    spawn_provider(None, None, false, false, false, jwks).await
}

async fn spawn_discovery_provider_serving(
    issuer_override: Option<String>,
    jwks_uri_override: Option<String>,
    oversized: bool,
    redirect: bool,
    redirect_jwks: bool,
) -> DiscoveryProvider {
    spawn_provider(
        issuer_override,
        jwks_uri_override,
        oversized,
        redirect,
        redirect_jwks,
        STUB_JWKS,
    )
    .await
}

async fn spawn_provider(
    issuer_override: Option<String>,
    jwks_uri_override: Option<String>,
    oversized: bool,
    redirect: bool,
    redirect_jwks: bool,
    jwks: &'static str,
) -> DiscoveryProvider {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind discovery provider");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("discovery provider address")
    );
    let state = DiscoveryState {
        base: base.clone(),
        issuer_override,
        jwks_uri_override,
        oversized,
        redirect,
        redirect_jwks,
        jwks,
        discovery_requests: Arc::new(AtomicUsize::new(0)),
        jwks_requests: Arc::new(AtomicUsize::new(0)),
    };
    let provider = DiscoveryProvider {
        base,
        discovery_requests: state.discovery_requests.clone(),
        jwks_requests: state.jwks_requests.clone(),
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery_handler))
        .route("/moved-discovery", get(moved_discovery_handler))
        .route("/jwks", get(discovered_jwks_handler))
        .route("/moved-jwks", get(moved_jwks_handler))
        .with_state(state);

    lore_base::lore_spawn!(async move {
        axum::serve(listener, app)
            .await
            .expect("serve discovery provider");
    });

    provider
}

/// A token from `issuer` for `alice`, valid for a minute, signed with HS256 under the key
/// id the stub key sets use.
fn signed_token(issuer: &str, secret: &[u8]) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("sig".to_string());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss": issuer, "sub": "alice", "aud": "Lore", "iat": now, "exp": now + 60});
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret),
    )
    .expect("encode test token")
}

fn discovering_service(provider: &DiscoveryProvider) -> JwkServiceImpl {
    JwkServiceImpl::with_issuers(
        JWKServiceSettings { endpoint: None },
        Some(std::slice::from_ref(&provider.base)),
    )
    .expect("an issuer URL resolves discovery")
}

/// If there is no explicit endpoint, keys resolve through the
/// issuer's discovery document, and a real token verifies against them.
#[tokio::test]
async fn keys_resolve_through_discovery_and_a_token_verifies() {
    let provider = spawn_discovery_provider(None, false).await;
    let service = discovering_service(&provider);

    service.fetch_new_keys(None).await.expect("discovery fetch");
    assert!(service.get_cached_key("sig").is_some());

    let verifier = lore_server::auth::jwt::JwtVerifier {
        jwk_service: Arc::new(service),
        jwt_issuer: Some(vec![provider.base.clone()]),
        jwt_audience: Some(vec!["Lore".to_string()]),
        jwt_typ: None,
        exchange_issuer: None,
    };
    let token = signed_token(&provider.base, b"the-secret");

    let verified = verifier
        .verify_token(&token)
        .await
        .expect("a token signed by the discovered key verifies");
    assert_eq!(verified.user_id, "alice");
}

/// An explicit endpoint wins over discovery and skips the fetch entirely.
#[tokio::test]
async fn an_explicit_endpoint_skips_discovery() {
    let provider = spawn_discovery_provider(None, false).await;
    let service = JwkServiceImpl::with_issuers(
        JWKServiceSettings {
            endpoint: Some(format!("{}/jwks", provider.base)),
        },
        Some(std::slice::from_ref(&provider.base)),
    )
    .expect("an explicit endpoint always resolves");

    service.fetch_new_keys(None).await.expect("fetch");

    assert!(service.get_cached_key("sig").is_some());
    assert_eq!(
        provider.discovery_requests.load(Ordering::SeqCst),
        0,
        "the discovery document must never be fetched"
    );
}

/// The issuer check: a document vouching for someone else is refused, and the error
/// names both values so the operator can see which side is misconfigured.
#[tokio::test]
async fn a_discovery_document_naming_another_issuer_is_refused() {
    let provider =
        spawn_discovery_provider(Some("https://impostor.example.com".to_string()), false).await;
    let service = discovering_service(&provider);

    let error = service
        .fetch_new_keys(None)
        .await
        .expect_err("a mismatched issuer must fail the fetch");

    let JWKServiceError::DiscoveryIssuerMismatch { expected, actual } = &error else {
        panic!("expected DiscoveryIssuerMismatch, got {error:?}");
    };
    assert_eq!(*expected, provider.base);
    assert_eq!(actual, "https://impostor.example.com");
    let message = error.to_string();
    assert!(message.contains(&provider.base) && message.contains("impostor.example.com"));
}

/// The downgrade matrix. An `https` issuer's keys are only ever fetched over
/// `https`. a plain-`http` issuer (local testing) has already waived transport
/// security. The other schemes are not allowed, `file://` included.
#[test]
fn discovered_jwks_uri_scheme_rules() {
    let https_issuer = "https://auth.example.com";
    let http_issuer = "http://127.0.0.1:8080";

    assert!(discovered_jwks_uri_scheme_permitted(
        https_issuer,
        "https://keys.example.com/jwks"
    ));
    assert!(
        !discovered_jwks_uri_scheme_permitted(https_issuer, "http://keys.example.com/jwks"),
        "an https issuer must never downgrade key retrieval to http"
    );
    assert!(discovered_jwks_uri_scheme_permitted(
        http_issuer,
        "http://127.0.0.1:8080/jwks"
    ));
    assert!(discovered_jwks_uri_scheme_permitted(
        http_issuer,
        "https://keys.example.com/jwks"
    ));
    for issuer in [https_issuer, http_issuer] {
        assert!(
            !discovered_jwks_uri_scheme_permitted(issuer, "file:///etc/passwd"),
            "a discovered URL must never reach a non-http scheme"
        );
        assert!(!discovered_jwks_uri_scheme_permitted(issuer, "not a url"));
    }
}

/// The refusal end to end: a discovery document steering key retrieval at a
/// non-https target is rejected and nothing is fetched from it.
#[tokio::test]
async fn a_discovered_jwks_uri_with_a_forbidden_scheme_is_refused() {
    let provider = spawn_discovery_provider_serving(
        None,
        Some("file:///etc/passwd".to_string()),
        false,
        false,
        false,
    )
    .await;
    let service = discovering_service(&provider);

    let error = service
        .fetch_new_keys(None)
        .await
        .expect_err("a file:// jwks_uri must be refused");

    assert!(
        matches!(error, JWKServiceError::JwksUriNotHttps { .. }),
        "{error:?}"
    );
    assert_eq!(
        provider.jwks_requests.load(Ordering::SeqCst),
        0,
        "nothing may be fetched from the refused URL"
    );
}

/// A discovered JWKS endpoint that answers a redirect is refused too — the scheme
/// check on the discovered `jwks_uri` would ensure nothing if a redirect could
/// then steer the key fetch elsewhere.
#[tokio::test]
async fn a_redirecting_discovered_jwks_endpoint_is_refused() {
    let provider = spawn_discovery_provider_serving(None, None, false, false, true).await;
    let service = discovering_service(&provider);

    let result = service.fetch_new_keys(None).await;

    assert!(
        result.is_err(),
        "the redirect must fail the fetch: {result:?}"
    );
    assert!(
        service.get_cached_key("sig").is_none(),
        "no key may be cached through a redirected fetch"
    );
}

/// The operator-authored endpoint keeps its historical behaviour: redirects are
/// followed. Compatibility for existing deployments whose endpoint sits behind one.
#[tokio::test]
async fn an_explicit_endpoint_may_still_redirect() {
    let provider = spawn_discovery_provider_serving(None, None, false, false, true).await;
    let service = JwkServiceImpl::new(JWKServiceSettings {
        endpoint: Some(format!("{}/jwks", provider.base)),
    });

    service
        .fetch_new_keys(None)
        .await
        .expect("an explicit endpoint follows the redirect as it always has");
    assert!(service.get_cached_key("sig").is_some());
}

/// A redirect from the discovery endpoint is refused, not followed. The redirect
/// here targets a route serving a perfectly valid document, so a client that
/// followed it would succeed.
#[tokio::test]
async fn a_redirecting_discovery_endpoint_is_refused() {
    let provider = spawn_discovery_provider_serving(None, None, false, true, false).await;
    let service = discovering_service(&provider);

    let result = service.fetch_new_keys(None).await;

    assert!(
        result.is_err(),
        "a redirect must fail the fetch: {result:?}"
    );
    assert_eq!(
        provider.jwks_requests.load(Ordering::SeqCst),
        0,
        "no keys may be fetched through a redirected discovery"
    );
}

/// The discovery response is capped like the JWKS response.
#[tokio::test]
async fn an_oversized_discovery_document_is_refused() {
    let provider = spawn_discovery_provider(None, true).await;
    let service = discovering_service(&provider);

    let result = service.fetch_new_keys(None).await;
    assert!(
        matches!(result, Err(JWKServiceError::ResponseTooLarge)),
        "{result:?}"
    );
}

/// The document is cached per issuer: key refreshes must not re-run discovery.
#[tokio::test]
async fn discovery_runs_once_per_issuer() {
    let provider = spawn_discovery_provider(None, false).await;
    let service = discovering_service(&provider);

    service.fetch_new_keys(None).await.expect("first fetch");
    expire_the_throttle(&service);
    service.fetch_new_keys(None).await.expect("second fetch");

    assert_eq!(provider.jwks_requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider.discovery_requests.load(Ordering::SeqCst),
        1,
        "the discovery document is resolved once, not per key refresh"
    );
}

/// with two issuers configured, discovery runs against the first.
#[tokio::test]
async fn discovery_uses_the_first_of_several_issuers() {
    let provider = spawn_discovery_provider(None, false).await;
    let service = JwkServiceImpl::with_issuers(
        JWKServiceSettings { endpoint: None },
        Some(&[provider.base.clone(), "https://old.example.com".to_string()]),
    )
    .expect("the first issuer is a URL");

    service.fetch_new_keys(None).await.expect("fetch");
    assert!(service.get_cached_key("sig").is_some());
}

/// No endpoint and nothing to discover against is a configuration error at
/// construction — a startup failure, not a request-time surprise.
#[test]
fn no_endpoint_and_no_issuer_url_fails_construction() {
    let keyword_issuer = JwkServiceImpl::with_issuers(
        JWKServiceSettings { endpoint: None },
        Some(&["LEGACY_AUTH_KEYWORD".to_string()]),
    );
    assert!(matches!(
        keyword_issuer,
        Err(JWKServiceError::EndpointUnresolvable)
    ));

    let no_issuers = JwkServiceImpl::with_issuers(JWKServiceSettings { endpoint: None }, None);
    assert!(matches!(
        no_issuers,
        Err(JWKServiceError::EndpointUnresolvable)
    ));
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
        endpoint: Some(format!("http://{address}/jwks")),
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

/// The verifier the server builds from `[server.auth]`, with a token exchange issuer apart
/// from the login issuer. Both providers serve one HS256 key under the same key id, so only
/// choosing the keys by `iss` keeps one provider's key from verifying the other's tokens.
mod separate_exchange_issuer {
    use lore_server::auth::jwt::JwtVerifier;
    use lore_server::auth::jwt::JwtVerifierError;
    use lore_server::settings::AuthSettings;

    use super::*;

    /// `other-secret`, under the key id [`STUB_JWKS`] uses.
    const OTHER_JWKS: &str =
        r#"{"keys":[{"kty":"oct","use":"sig","kid":"sig","alg":"HS256","k":"b3RoZXItc2VjcmV0"}]}"#;

    fn auth(login: &str, exchange: &str, jwk_endpoint: Option<&str>) -> AuthSettings {
        let jwk = jwk_endpoint
            .map(|endpoint| format!("[server.auth.jwk]\nendpoint = \"{endpoint}\""))
            .unwrap_or_default();
        crate::settings::settings_with_auth_keys(&format!(
            r#"
                jwt_issuer = "{login}"
                jwt_audience = ["Lore"]
                resource_claim = "resources"

                [server.auth.oidc]
                client_id = "lore-cli"
                resource_template = "https://lore.example.com/partitions/{{id}}"
                token_exchange_issuer = "{exchange}"

                {jwk}
            "#
        ))
        .expect("[server.auth] parses")
        .server
        .auth
        .expect("[server.auth] is present")
    }

    fn is_signature_failure(error: &JwtVerifierError) -> bool {
        matches!(error, JwtVerifierError::ValidationFailed(inner)
            if *inner.kind() == jsonwebtoken::errors::ErrorKind::InvalidSignature)
    }

    #[tokio::test]
    async fn each_issuer_verifies_only_its_own_tokens() {
        let login = spawn_discovery_provider(None, false).await;
        let exchange = spawn_discovery_provider_with_jwks(OTHER_JWKS).await;
        let verifier = JwtVerifier::from_settings(&auth(&login.base, &exchange.base, None))
            .await
            .expect("both key sets are fetched");

        verifier
            .verify_token(&signed_token(&login.base, b"the-secret"))
            .await
            .expect("the login issuer's token verifies");
        verifier
            .verify_token(&signed_token(&exchange.base, b"other-secret"))
            .await
            .expect("the exchange issuer's token verifies");
        for forged in [
            signed_token(&exchange.base, b"the-secret"),
            signed_token(&login.base, b"other-secret"),
        ] {
            let error = verifier
                .verify_token(&forged)
                .await
                .expect_err("a token signed in the other issuer's name is refused");
            assert!(is_signature_failure(&error), "{error:?}");
        }
    }

    /// `[server.auth.jwk].endpoint` names the login issuer's keys, which must never verify a
    /// token in the exchange issuer's name.
    #[tokio::test]
    async fn the_jwk_endpoint_never_serves_the_exchange_issuer() {
        let login = spawn_discovery_provider(None, false).await;
        let exchange = spawn_discovery_provider_with_jwks(OTHER_JWKS).await;
        let endpoint = format!("{}/jwks", login.base);
        let verifier =
            JwtVerifier::from_settings(&auth(&login.base, &exchange.base, Some(&endpoint)))
                .await
                .expect("both key sets are fetched");

        let error = verifier
            .verify_token(&signed_token(&exchange.base, b"the-secret"))
            .await
            .expect_err("the login issuer's key doesn't verify the exchange issuer's tokens");
        assert!(is_signature_failure(&error), "{error:?}");
        verifier
            .verify_token(&signed_token(&exchange.base, b"other-secret"))
            .await
            .expect("the exchange issuer's token verifies");
    }

    #[tokio::test]
    async fn an_unreachable_exchange_issuer_fails_construction() {
        let login = spawn_discovery_provider(None, false).await;
        let exchange = spawn_discovery_provider_with_jwks(OTHER_JWKS).await;
        let unreachable = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            format!("http://{}", listener.local_addr().expect("address"))
        };

        JwtVerifier::from_settings(&auth(&login.base, &exchange.base, None))
            .await
            .expect("with the exchange issuer reachable, construction succeeds");
        let Err(error) = JwtVerifier::from_settings(&auth(&login.base, &unreachable, None)).await
        else {
            panic!("an unreachable exchange issuer fails construction");
        };
        assert!(
            matches!(
                error.downcast_ref::<JWKServiceError>(),
                Some(JWKServiceError::InternalError)
            ),
            "{error:#}"
        );
    }
}
