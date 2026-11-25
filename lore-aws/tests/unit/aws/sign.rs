// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use http::HeaderMap;
use http::HeaderValue;
use lore_aws::aws::sign::*;
use lore_base::timestamp;
use url::Url;

use crate::aws::utc;

/// The `SignedHeaders` list, read back out of an authorization.
fn signed_headers(authorization: &str) -> &str {
    const MARKER: &str = "SignedHeaders=";

    let Some(start) = authorization.find(MARKER) else {
        return "";
    };
    let rest = &authorization[start + MARKER.len()..];

    rest.split_once(',').map_or(rest, |(list, _)| list)
}

/// The `Authorization` header a signed request carries.
fn authorization_of(headers: &HeaderMap) -> &str {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .expect("a signed request carries Authorization")
}

fn iam_context<'a>() -> SigningContext<'a> {
    SigningContext {
        access_key_id: "AKIDEXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        session_token: None,
        region: "us-east-1",
        service: "iam",
        timestamp: utc(2015, 8, 30, 12, 36, 0),
    }
}

/// The worked example from AWS's Signature Version 4 documentation, asserted stage by stage so a
/// failure says which stage is wrong.
///
/// <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>
#[test]
fn signs_the_documented_aws_example() {
    let url = Url::parse("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded; charset=utf-8"),
    );

    let context = iam_context();
    let keys = SigningKeyCache::new();
    let mut scratch = SignScratch::new();

    // The documented example predates `x-amz-content-sha256`, so compare the canonical request
    // against it with that header removed from the picture: build it directly.
    let canonical = canonical_headers(&headers, "iam.amazonaws.com").unwrap();
    let mut with_date: Vec<(&str, &str)> = canonical.to_vec();
    with_date.push(("x-amz-date", "20150830T123600Z"));
    with_date.sort_unstable_by_key(|(name, _)| *name);

    let mut canonical_request = String::new();
    write_canonical_request(
        &mut canonical_request,
        "GET",
        "/",
        Some("Action=ListUsers&Version=2010-05-08"),
        &with_date,
        EMPTY_PAYLOAD_SHA256,
    );

    assert_eq!(
        canonical_request,
        concat!(
            "GET\n",
            "/\n",
            "Action=ListUsers&Version=2010-05-08\n",
            "content-type:application/x-www-form-urlencoded; charset=utf-8\n",
            "host:iam.amazonaws.com\n",
            "x-amz-date:20150830T123600Z\n",
            "\n",
            "content-type;host;x-amz-date\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
    );

    let mut digest = [0u8; 64];
    assert_eq!(
        payload_sha256(canonical_request.as_bytes(), &mut digest),
        "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59",
        "the canonical request hash is the documented one"
    );

    let string_to_sign = format!(
        "{ALGORITHM}\n20150830T123600Z\n20150830/us-east-1/iam/aws4_request\n{}",
        payload_sha256(canonical_request.as_bytes(), &mut digest)
    );
    assert_eq!(
        string_to_sign,
        concat!(
            "AWS4-HMAC-SHA256\n",
            "20150830T123600Z\n",
            "20150830/us-east-1/iam/aws4_request\n",
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59",
        )
    );

    let signing_key = derive_signing_key(
        context.secret_access_key,
        "20150830",
        context.region,
        context.service,
    );
    let mut key_hex = [0u8; 64];
    assert_eq!(
        hex_into(&signing_key, &mut key_hex),
        "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9",
        "the derived signing key is the documented one"
    );

    let mut signature_hex = [0u8; 64];
    assert_eq!(
        hex_into(
            &hmac_sha256(&signing_key, string_to_sign.as_bytes()),
            &mut signature_hex
        ),
        "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7",
        "the signature is the documented one"
    );

    let mut request_headers = headers.clone();
    sign_request(
        "GET",
        &url,
        &mut request_headers,
        EMPTY_PAYLOAD_SHA256,
        &context,
        &keys,
        &mut scratch,
    )
    .unwrap();

    assert!(
        scratch.authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature="
        ),
        "unexpected Authorization: {}",
        scratch.authorization
    );
    assert_eq!(
        authorization_of(&request_headers),
        scratch.authorization,
        "the header the request carries is the one that was built"
    );
}

/// `sign_request` itself reproduces AWS's published Signature Version 4 example for an S3
/// `GET Object` with a `Range`, so the signature it puts on a request is checked end to end rather
/// than stage by stage.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html>
#[test]
fn sign_request_reproduces_the_published_s3_get_object_signature() {
    let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(http::header::RANGE, HeaderValue::from_static("bytes=0-9"));

    let context = SigningContext {
        access_key_id: "AKIAIOSFODNN7EXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        session_token: None,
        region: "us-east-1",
        service: "s3",
        timestamp: utc(2013, 5, 24, 0, 0, 0),
    };
    let mut scratch = SignScratch::new();

    sign_request(
        "GET",
        &url,
        &mut headers,
        EMPTY_PAYLOAD_SHA256,
        &context,
        &SigningKeyCache::new(),
        &mut scratch,
    )
    .unwrap();

    assert_eq!(
        scratch.canonical_request,
        concat!(
            "GET\n",
            "/test.txt\n",
            "\n",
            "host:examplebucket.s3.amazonaws.com\n",
            "range:bytes=0-9\n",
            "x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
            "x-amz-date:20130524T000000Z\n",
            "\n",
            "host;range;x-amz-content-sha256;x-amz-date\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        "the canonical request is the published one"
    );
    assert_eq!(
        headers.get("x-amz-date").unwrap(),
        "20130524T000000Z",
        "the request carries the timestamp it was signed for"
    );
    assert_eq!(
        headers.get("x-amz-content-sha256").unwrap(),
        EMPTY_PAYLOAD_SHA256
    );
    assert_eq!(
        authorization_of(&headers),
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
         SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
         Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
}

/// Neither the signing-key cache nor the signing buffers show what they hold in `Debug`: the key
/// signs as well as the secret, and the buffers hold the session token.
#[test]
fn signing_state_shows_no_secret_in_debug() {
    let url = Url::parse("https://dynamodb.us-east-1.amazonaws.com/").unwrap();
    let keys = SigningKeyCache::new();
    let mut scratch = SignScratch::new();
    let mut context = iam_context();
    context.session_token = Some("SESSION-TOKEN-a1b2c3");

    let mut headers = HeaderMap::new();
    sign_request(
        "POST",
        &url,
        &mut headers,
        EMPTY_PAYLOAD_SHA256,
        &context,
        &keys,
        &mut scratch,
    )
    .unwrap();

    let mut key_hex = [0u8; 64];
    let key = hex_into(
        &derive_signing_key(context.secret_access_key, "20150830", "us-east-1", "iam"),
        &mut key_hex,
    )
    .to_owned();
    let signature = authorization_of(&headers)
        .rsplit("Signature=")
        .next()
        .expect("the authorization ends with its signature")
        .to_owned();

    let rendered = format!("{keys:?} {scratch:?}");

    for secret in ["SESSION-TOKEN-a1b2c3", key.as_str(), signature.as_str()] {
        assert!(!rendered.contains(secret), "{rendered} shows {secret}");
    }
}

/// The headers that carry a credential are marked sensitive, so a request's `Debug` output shows
/// neither the session token nor the signature.
#[test]
fn credential_headers_are_marked_sensitive() {
    let url = Url::parse("https://dynamodb.us-east-1.amazonaws.com/").unwrap();
    let mut context = iam_context();
    context.session_token = Some("session-token-value");

    let mut headers = HeaderMap::new();
    sign_request(
        "POST",
        &url,
        &mut headers,
        EMPTY_PAYLOAD_SHA256,
        &context,
        &SigningKeyCache::new(),
        &mut SignScratch::new(),
    )
    .unwrap();

    for name in ["x-amz-security-token", "authorization"] {
        assert!(
            headers.get(name).is_some_and(HeaderValue::is_sensitive),
            "{name} must be marked sensitive"
        );
    }
    assert!(
        !format!("{headers:?}").contains("session-token-value"),
        "the token must not reach a request's Debug output"
    );
}

/// Signing renders its own timestamp, so it has to agree with an RFC 3339 rendering of the same time
/// with its separators taken out — for every field boundary that could be written wrong, not just a
/// convenient one.
#[test]
fn the_hand_written_timestamp_matches_rfc3339_without_separators() {
    for (year, month, day, hour, minute, second) in [
        (2015, 8, 30, 12, 36, 0),
        (2024, 1, 1, 0, 0, 0),
        (2024, 12, 31, 23, 59, 59),
        (2024, 2, 29, 9, 5, 7),
        (1999, 10, 9, 19, 10, 59),
        (2000, 11, 20, 1, 2, 3),
    ] {
        let at = utc(year, month, day, hour, minute, second);
        let mut buffer = [0u8; TIMESTAMP_LEN];
        let rendered = write_timestamp(&mut buffer, at);
        let expected: String = timestamp::rfc3339(at)
            .trim_end_matches("+00:00")
            .chars()
            .filter(|&c| c != '-' && c != ':')
            .chain(['Z'])
            .collect();

        assert_eq!(rendered, expected, "timestamp disagrees at {at}");
        assert_eq!(
            &rendered[..DATE_LEN],
            timestamp::date(at).replace('-', ""),
            "scope date disagrees at {at}"
        );
    }
}

/// The date header is cached on the scratch and reused while the second holds. If the cache were
/// keyed wrongly a request would be signed over one timestamp and sent carrying another, which
/// the service rejects — so the header a request ends up with is checked against the second it
/// was signed for, across a change of second and back again.
#[test]
fn the_cached_date_header_follows_the_timestamp() {
    let url = Url::parse("https://dynamodb.us-east-1.amazonaws.com/").unwrap();
    let keys = SigningKeyCache::new();
    let mut scratch = SignScratch::new();

    let mut sign_at = |second: u8| {
        let context = SigningContext {
            timestamp: utc(2015, 8, 30, 12, 36, second),
            ..iam_context()
        };
        let mut headers = HeaderMap::new();
        sign_request(
            "POST",
            &url,
            &mut headers,
            EMPTY_PAYLOAD_SHA256,
            &context,
            &keys,
            &mut scratch,
        )
        .unwrap();
        headers
            .get("x-amz-date")
            .and_then(|value| value.to_str().ok())
            .expect("signing sets the date header")
            .to_owned()
    };

    assert_eq!(sign_at(0), "20150830T123600Z");
    assert_eq!(
        sign_at(0),
        "20150830T123600Z",
        "a cache hit is the same second"
    );
    assert_eq!(sign_at(1), "20150830T123601Z", "a new second rebuilds it");
    assert_eq!(
        sign_at(0),
        "20150830T123600Z",
        "and going back rebuilds again"
    );
}

/// An empty body always hashes to the same digest, so that header is a shared constant. It still
/// has to be the *right* constant, and a request with a body must not get it.
#[test]
fn the_payload_hash_header_matches_the_digest_it_names() {
    assert_eq!(
        payload_hash_header(EMPTY_PAYLOAD_SHA256).to_str().unwrap(),
        EMPTY_PAYLOAD_SHA256
    );

    let mut digest = [0u8; 64];
    let body_hash = payload_sha256(b"a body", &mut digest);
    assert_eq!(payload_hash_header(body_hash).to_str().unwrap(), body_hash);
    assert_ne!(body_hash, EMPTY_PAYLOAD_SHA256);

    assert_eq!(
        payload_hash_header(UNSIGNED_PAYLOAD).to_str().unwrap(),
        UNSIGNED_PAYLOAD
    );
}

/// A body goes unsigned only over TLS and only to a service that accepts it; anywhere else its
/// SHA-256 is what binds it to the signature.
#[test]
fn a_body_is_unsigned_only_over_tls_to_a_service_that_accepts_it() {
    let tls = Url::parse("https://s3.eu-west-1.amazonaws.com/bucket/key").unwrap();
    let plain = Url::parse("http://127.0.0.1:9000/bucket/key").unwrap();
    let mut digest = [0u8; 64];
    let mut expected = [0u8; 64];
    let body_hash = payload_sha256(b"a body", &mut expected);

    assert_eq!(
        payload_hash(true, &tls, b"a body", &mut digest),
        UNSIGNED_PAYLOAD
    );
    assert_eq!(
        payload_hash(true, &plain, b"a body", &mut digest),
        body_hash
    );
    assert_eq!(payload_hash(false, &tls, b"a body", &mut digest), body_hash);
}

/// The scratch is the whole point of the signer's shape: it is reused across requests, so signing
/// twice must reuse the buffers rather than grow fresh ones.
#[test]
fn signing_twice_reuses_the_scratch_buffers() {
    let url = Url::parse("https://dynamodb.us-east-1.amazonaws.com/").unwrap();
    let keys = SigningKeyCache::new();
    let mut scratch = SignScratch::new();

    let mut sign = |headers: &mut HeaderMap| {
        sign_request(
            "POST",
            &url,
            headers,
            EMPTY_PAYLOAD_SHA256,
            &iam_context(),
            &keys,
            &mut scratch,
        )
        .unwrap();
        (
            scratch.canonical_request.capacity(),
            scratch.authorization.capacity(),
            scratch.authorization.clone(),
        )
    };

    let (canonical_capacity, authorization_capacity, first) = sign(&mut HeaderMap::new());
    assert!(canonical_capacity > 0 && authorization_capacity > 0);

    let (again_canonical, again_authorization, second) = sign(&mut HeaderMap::new());

    assert_eq!(
        (again_canonical, again_authorization),
        (canonical_capacity, authorization_capacity),
        "a second signature must not reallocate the buffers the first one grew"
    );
    assert_eq!(first, second, "the same request signs the same way");
}

/// Every `x-amz-*` header must be signed. `x-amz-meta-*` carries the fragment that describes an
/// object's bytes, so leaving it unsigned would let it be changed in flight. `user-agent` stands
/// for the headers outside that set and outside `content-type`, which stay unsigned.
#[test]
fn signs_every_x_amz_header_including_object_metadata() {
    let url = Url::parse("https://bucket.s3.us-east-1.amazonaws.com/abc").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-amz-meta-lore-fragment",
        HeaderValue::from_static("8:1:1"),
    );
    headers.insert("x-amz-target", HeaderValue::from_static("Irrelevant.Op"));
    headers.insert("user-agent", HeaderValue::from_static("lore"));

    let mut scratch = SignScratch::new();
    sign_request(
        "PUT",
        &url,
        &mut headers,
        EMPTY_PAYLOAD_SHA256,
        &iam_context(),
        &SigningKeyCache::new(),
        &mut scratch,
    )
    .unwrap();

    assert_eq!(
        signed_headers(&scratch.authorization),
        "host;x-amz-content-sha256;x-amz-date;x-amz-meta-lore-fragment;x-amz-target"
    );
    assert!(
        !signed_headers(&scratch.authorization).contains("user-agent"),
        "signing a hop-by-hop header would break intermediaries"
    );
}

/// `Range` decides which bytes come back, so it is signed like the rest of what this client sets.
#[test]
fn a_range_header_is_signed() {
    let url = Url::parse("https://bucket.s3.us-east-1.amazonaws.com/abc").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::RANGE,
        HeaderValue::from_static("bytes=0-1023"),
    );

    let mut scratch = SignScratch::new();
    sign_request(
        "GET",
        &url,
        &mut headers,
        EMPTY_PAYLOAD_SHA256,
        &iam_context(),
        &SigningKeyCache::new(),
        &mut scratch,
    )
    .unwrap();

    assert_eq!(
        signed_headers(&scratch.authorization),
        "host;range;x-amz-content-sha256;x-amz-date"
    );
    assert!(
        scratch.canonical_request.contains("range:bytes=0-1023\n"),
        "unexpected canonical request: {}",
        scratch.canonical_request
    );
}

/// Session credentials put their token on the request, and it is signed with everything else; a
/// request signed without one carries none.
#[test]
fn a_session_token_is_sent_and_signed() {
    let url = Url::parse("https://dynamodb.us-east-1.amazonaws.com/").unwrap();
    let mut scratch = SignScratch::new();

    let mut context = iam_context();
    context.session_token = Some("session-token-value");
    let mut headers = HeaderMap::new();
    sign_request(
        "POST",
        &url,
        &mut headers,
        EMPTY_PAYLOAD_SHA256,
        &context,
        &SigningKeyCache::new(),
        &mut scratch,
    )
    .unwrap();

    assert!(signed_headers(&scratch.authorization).contains("x-amz-security-token"));
    assert_eq!(
        headers.get("x-amz-security-token").unwrap(),
        "session-token-value"
    );

    let mut untokened = HeaderMap::new();
    sign_request(
        "POST",
        &url,
        &mut untokened,
        EMPTY_PAYLOAD_SHA256,
        &iam_context(),
        &SigningKeyCache::new(),
        &mut scratch,
    )
    .unwrap();

    assert!(!signed_headers(&scratch.authorization).contains("x-amz-security-token"));
    assert!(untokened.get("x-amz-security-token").is_none());
}

#[test]
fn a_valueless_query_flag_canonicalizes_with_an_equals() {
    let mut out = String::new();
    write_canonical_query(&mut out, "versions&prefix=abc");

    assert_eq!(out, "prefix=abc&versions=");
}

#[test]
fn query_parameters_sort_by_name_then_value() {
    let mut out = String::new();
    write_canonical_query(&mut out, "b=2&a=9&a=1");

    assert_eq!(out, "a=1&a=9&b=2");
}

#[test]
fn header_values_collapse_inner_whitespace() {
    let mut out = String::new();
    push_collapsed(&mut out, "one   two\tthree");

    assert_eq!(out, "one two three");
}

/// The buffer is reused across calls, so writing a second host must replace the first rather
/// than append to it.
#[test]
fn the_host_header_carries_a_non_default_port() {
    let localstack = Url::parse("http://localhost:4566/").unwrap();
    let https = Url::parse("https://dynamodb.us-east-1.amazonaws.com/").unwrap();

    let mut host = String::new();
    write_host(&mut host, &localstack).unwrap();
    assert_eq!(host, "localhost:4566");

    write_host(&mut host, &https).unwrap();
    assert_eq!(
        host, "dynamodb.us-east-1.amazonaws.com",
        "a default port must not appear, because hyper will not send it"
    );
}

#[test]
fn percent_encoding_keeps_only_the_unreserved_set() {
    let mut out = String::new();
    percent_encode_into(&mut out, "a/b c~d.e-f_g+h=i", false);
    assert_eq!(out, "a%2Fb%20c~d.e-f_g%2Bh%3Di");

    let mut path = String::new();
    percent_encode_into(&mut path, "a/b c", true);
    assert_eq!(path, "a/b%20c");
}

#[test]
fn the_key_cache_derives_once_per_date_and_credential() {
    let keys = SigningKeyCache::new();
    let context = iam_context();

    let first = keys.signing_key(&context, "20150830");
    let cached = keys.signing_key(&context, "20150830");
    assert_eq!(first, cached);

    let next_day = keys.signing_key(&context, "20150831");
    assert_ne!(first, next_day, "a new date must derive a new key");

    let mut rotated = iam_context();
    rotated.access_key_id = "AKIDROTATED";
    rotated.secret_access_key = "another-secret";
    let after_rotation = keys.signing_key(&rotated, "20150830");
    assert_ne!(
        first, after_rotation,
        "rotated credentials must not reuse the previous secret's key"
    );
}

#[test]
fn an_empty_path_signs_as_root() {
    let mut out = String::new();
    write_canonical_request(&mut out, "POST", "", None, &[("host", "h")], "hash");

    assert!(out.starts_with("POST\n/\n\n"), "unexpected: {out:?}");
}

/// A signed header set twice signs differently here than at AWS, and the rejection that follows
/// reads as a clock problem. Refused where it is built, so the diagnosis is local. One value for
/// that name is the ordinary case, and an unsigned header repeating is not this client's
/// business, so neither is refused.
#[test]
fn a_repeated_signed_header_is_refused() {
    let mut headers = HeaderMap::new();
    headers.append(
        "x-amz-meta-lore-fragment",
        HeaderValue::from_static("8:1:1"),
    );
    headers.append(
        "x-amz-meta-lore-fragment",
        HeaderValue::from_static("8:2:2"),
    );

    assert!(matches!(
        canonical_headers(&headers, "host"),
        Err(SignError::RepeatedHeader(name)) if name == "x-amz-meta-lore-fragment"
    ));

    let mut single = HeaderMap::new();
    single.insert(
        "x-amz-meta-lore-fragment",
        HeaderValue::from_static("8:1:1"),
    );
    single.append("user-agent", HeaderValue::from_static("lore"));
    single.append("user-agent", HeaderValue::from_static("lore-again"));

    assert!(canonical_headers(&single, "host").is_ok());
}

#[test]
fn a_url_without_a_host_is_refused() {
    let url = Url::parse("file:///tmp/nope").unwrap();

    assert!(matches!(
        write_host(&mut String::new(), &url),
        Err(SignError::NoHost)
    ));
}
