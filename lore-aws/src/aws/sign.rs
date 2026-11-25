// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `AWS4-HMAC-SHA256` request signing.
//!
//! Header signing only — no presigning, no chunked payload signing, no `SigV4a`. The algorithm is
//! specified at <https://docs.aws.amazon.com/general/latest/gr/signature-version-4.html>, and the
//! documented worked example is this module's test.
//!
//! Two properties are load-bearing for security:
//!
//! - **Every `x-amz-*` header is signed.** A header that is sent but unsigned can be changed in
//!   flight without invalidating the signature, and `PutObject` carries the fragment's own metadata
//!   in `x-amz-meta-lore-fragment`. Signing the whole `x-amz-*` set is also what `DynamoDB`
//!   requires of `x-amz-target`, so the rule is one rule rather than a per-operation list.
//! - **The body is bound to the signature wherever nothing else binds it.** Over plain HTTP its
//!   SHA-256 is signed; only over TLS, and only for a service that accepts it, is it sent as
//!   `UNSIGNED-PAYLOAD` (see [`payload_hash`]).
use std::fmt::Write as _;

use http::HeaderMap;
use http::HeaderValue;
use parking_lot::RwLock;
use ring::digest;
use ring::hmac;
use smallvec::SmallVec;
use time::UtcDateTime;
use url::Url;

/// The algorithm name, in the string to sign and in the `Authorization` header.
#[lore_macro::test_pub]
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// The scope terminator.
const TERMINATOR: &str = "aws4_request";
/// SHA-256 of nothing, for a request with no body.
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// `YYYYMMDDTHHMMSSZ`, and the `YYYYMMDD` prefix of it that the credential scope uses.
#[lore_macro::test_pub]
const TIMESTAMP_LEN: usize = 16;
#[lore_macro::test_pub]
const DATE_LEN: usize = 8;

/// A hex digest, rendered without allocating.
pub type HexDigest = [u8; 64];

/// Render `bytes` as lowercase hex into a fixed buffer, returning it as text.
#[lore_macro::test_pub]
fn hex_into<'a>(bytes: &[u8], out: &'a mut HexDigest) -> &'a str {
    hex::encode_to_slice(bytes, out).expect("a 32-byte digest renders as 64 hex characters");
    // `encode_to_slice` writes only ASCII hex digits.
    std::str::from_utf8(out).expect("hex is ASCII")
}

/// The payload hash a request sends in place of a digest of its body.
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// SHA-256 of `payload`, as hex text in a caller-owned buffer.
pub fn payload_sha256<'a>(payload: &[u8], out: &'a mut HexDigest) -> &'a str {
    let digest = digest::digest(&digest::SHA256, payload);
    hex_into(digest.as_ref(), out)
}

/// The payload hash a request carrying `body` to `url` is signed with.
///
/// [`UNSIGNED_PAYLOAD`] when the service accepts it, `unsigned_over_tls`, and `url` is `https`: TLS
/// already protects the body in transit, and a fragment payload hashed again for the signature
/// would cost the calling worker a quarter of a megabyte of SHA-256 on every put. Otherwise the
/// body's SHA-256, which binds it to the signature where nothing else does.
pub fn payload_hash<'a>(
    unsigned_over_tls: bool,
    url: &Url,
    body: &[u8],
    out: &'a mut HexDigest,
) -> &'a str {
    if unsigned_over_tls && url.scheme() == "https" {
        return UNSIGNED_PAYLOAD;
    }

    payload_sha256(body, out)
}

/// Who is signing, for what, and when.
pub struct SigningContext<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub session_token: Option<&'a str>,
    pub region: &'a str,
    pub service: &'static str,
    pub timestamp: UtcDateTime,
}

/// Why a request could not be signed.
#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("request URL has no host to sign")]
    NoHost,
    #[error("header {0} holds a value that is not text")]
    NonTextHeader(String),
    /// A signed header carrying more than one value.
    ///
    /// Canonicalization combines the values onto one line and names the header once. Nothing here
    /// sets a signed header twice — every one is an `insert`, and object metadata is deduplicated
    /// before it becomes headers — so the case is refused rather than implemented. Signing it the
    /// obvious way would produce a signature AWS computes differently, which arrives as
    /// `SignatureDoesNotMatch`: a code this client reads as a drifting clock, so it would spend its
    /// one re-sign on a clock that was never wrong and then fail with the wrong diagnosis.
    #[error("header {0} is signed and carries more than one value")]
    RepeatedHeader(String),
}

/// Derives and caches the signing key.
///
/// The key depends only on the secret, the date, the region and the service, so in steady state one
/// HMAC chain per day per service replaces four HMACs per request. The cache holds one entry
/// because a client signs for exactly one region and service, and the date advances rather than
/// alternating.
///
/// `Debug` shows nothing of the entry: the key signs as well as the secret it was derived from.
#[derive(Default)]
pub struct SigningKeyCache {
    current: RwLock<Option<CachedKey>>,
}

impl std::fmt::Debug for SigningKeyCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKeyCache").finish_non_exhaustive()
    }
}

struct CachedKey {
    date: String,
    region: String,
    access_key_id: String,
    key: [u8; 32],
}

impl SigningKeyCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The signing key for this date, region, service and credential, deriving it if needed.
    ///
    /// Keyed on the access key id as well as the date, so rotating credentials cannot be signed
    /// with the previous secret's key.
    #[lore_macro::test_pub]
    fn signing_key(&self, context: &SigningContext<'_>, date: &str) -> [u8; 32] {
        if let Some(cached) = self.current.read().as_ref()
            && cached.date == date
            && cached.region == context.region
            && cached.access_key_id == context.access_key_id
        {
            return cached.key;
        }

        let key = derive_signing_key(
            context.secret_access_key,
            date,
            context.region,
            context.service,
        );

        *self.current.write() = Some(CachedKey {
            date: date.to_owned(),
            region: context.region.to_owned(),
            access_key_id: context.access_key_id.to_owned(),
            key,
        });

        key
    }
}

/// `HMAC(HMAC(HMAC(HMAC("AWS4" + secret, date), region), service), "aws4_request")`.
pub fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let mut seed = Vec::with_capacity(4 + secret.len());
    seed.extend_from_slice(b"AWS4");
    seed.extend_from_slice(secret.as_bytes());

    let mut key = hmac_sha256(&seed, date.as_bytes());
    key = hmac_sha256(&key, region.as_bytes());
    key = hmac_sha256(&key, service.as_bytes());
    hmac_sha256(&key, TERMINATOR.as_bytes())
}

#[lore_macro::test_pub]
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Percent-encode `text` into `out`, leaving only the RFC3986 unreserved set intact.
///
/// `keep_slash` is what separates a path from everything else: path segments keep their separators,
/// query names and values do not.
pub fn percent_encode_into(out: &mut String, text: &str, keep_slash: bool) {
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b'/' if keep_slash => out.push('/'),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
}

/// Headers that are signed when present, beyond the `x-amz-*` set.
///
/// Both are headers *this client* sets and whose value changes what the service does:
/// `content-type` is what `DynamoDB` dispatches its protocol on, and `range` decides which bytes a
/// `GetObject` answers with. Nothing else is signed, because a header we do not set is one an
/// intermediary is entitled to add or rewrite, and signing those breaks the request rather than
/// protecting it.
const ALSO_SIGNED: [&str; 2] = ["content-type", "range"];

/// The headers a request signs: `host`, each of [`ALSO_SIGNED`] when set, and every `x-amz-*`.
///
/// Sorted by name, values trimmed and inner whitespace collapsed, which is what canonicalization
/// requires. `host` is passed separately because hyper adds it at send time, so it is never in the
/// map we are handed.
///
/// A `HeaderMap` yields one entry per value, so a header set twice would arrive here twice. That is
/// refused rather than combined: see [`SignError::RepeatedHeader`].
#[lore_macro::test_pub]
fn canonical_headers<'a>(
    headers: &'a HeaderMap,
    host: &'a str,
) -> Result<SmallVec<[(&'a str, &'a str); 12]>, SignError> {
    let mut collected: SmallVec<[(&str, &str); 12]> = SmallVec::new();
    collected.push(("host", host));

    for (name, value) in headers {
        let name = name.as_str();
        if !ALSO_SIGNED.contains(&name) && !name.starts_with("x-amz-") {
            continue;
        }

        let Ok(value) = value.to_str() else {
            return Err(SignError::NonTextHeader(name.to_owned()));
        };
        collected.push((name, value.trim()));
    }

    collected.sort_unstable_by_key(|(name, _)| *name);

    if let Some(pair) = collected.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(SignError::RepeatedHeader(pair[0].0.to_owned()));
    }

    Ok(collected)
}

/// The canonical request, written into `out`.
///
/// `path` and `query` are taken from the URL as they already stand, so the caller is responsible
/// for having encoded them canonically — see [`percent_encode_into`]. Every path and query this
/// client builds is assembled that way, so neither is re-encoded here.
#[lore_macro::test_pub]
fn write_canonical_request(
    out: &mut String,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &[(&str, &str)],
    payload_hash: &str,
) {
    out.push_str(method);
    out.push('\n');

    if path.is_empty() {
        out.push('/');
    } else {
        out.push_str(path);
    }
    out.push('\n');

    if let Some(query) = query.filter(|query| !query.is_empty()) {
        write_canonical_query(out, query);
    }
    out.push('\n');

    for (name, value) in headers {
        out.push_str(name);
        out.push(':');
        push_collapsed(out, value);
        out.push('\n');
    }
    out.push('\n');

    for (index, (name, _)) in headers.iter().enumerate() {
        if index > 0 {
            out.push(';');
        }
        out.push_str(name);
    }
    out.push('\n');

    out.push_str(payload_hash);
}

/// Sort the query's parameters and give every one an `=`, which canonicalization requires even for
/// a valueless flag such as S3's `?versions`.
#[lore_macro::test_pub]
fn write_canonical_query(out: &mut String, query: &str) {
    let mut parameters: SmallVec<[(&str, &str); 6]> = query
        .split('&')
        .filter(|parameter| !parameter.is_empty())
        .map(|parameter| match parameter.split_once('=') {
            Some((name, value)) => (name, value),
            None => (parameter, ""),
        })
        .collect();

    parameters.sort_unstable();

    for (index, (name, value)) in parameters.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(name);
        out.push('=');
        out.push_str(value);
    }
}

/// Append `value` with runs of whitespace collapsed to one space.
///
/// Leading whitespace is dropped and trailing whitespace never reaches `out`, as canonicalization
/// requires. The tracking is per value rather than per buffer, `out` already holding the rest of
/// the canonical request by the time a header value is appended.
#[lore_macro::test_pub]
fn push_collapsed(out: &mut String, value: &str) {
    let mut in_space = false;
    let mut written = false;

    for character in value.chars() {
        if character == ' ' || character == '\t' {
            in_space = true;
            continue;
        }
        if in_space && written {
            out.push(' ');
        }
        in_space = false;
        written = true;
        out.push(character);
    }
}

/// The buffers a signature is assembled in.
///
/// The intermediate strings are cleared per call rather than built afresh, so signing reuses their
/// capacity instead of allocating them for every request. On return they hold the last canonical
/// request and authorization.
///
/// Reused across attempts and across requests; see the thread-local in [`crate::aws`].
///
/// `Debug` shows only the host: the canonical request and the cached header carry the session
/// token, and the authorization carries a signature the service would accept.
#[lore_macro::test_pub]
#[derive(Default)]
pub struct SignScratch {
    host: String,
    canonical_request: String,
    string_to_sign: String,
    authorization: String,
    /// The `x-amz-date` header last built, with the timestamp it was built for.
    date_header: Option<(String, HeaderValue)>,
    /// The `x-amz-security-token` header last built, with the token it was built for.
    token_header: Option<(String, HeaderValue)>,
}

impl std::fmt::Debug for SignScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignScratch")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl SignScratch {
    /// `const` so the per-thread instance costs no lazy-initialization check on each access.
    pub const fn new() -> Self {
        Self {
            host: String::new(),
            canonical_request: String::new(),
            string_to_sign: String::new(),
            authorization: String::new(),
            date_header: None,
            token_header: None,
        }
    }

    /// The `x-amz-date` header for `timestamp`, reusing the last one when the second has not moved.
    ///
    /// A `HeaderValue` built from a string owns its bytes unshared, so handing one to a request
    /// costs an allocation to build and another the first time it is cloned. The value only changes
    /// once a second while a busy worker signs thousands of requests in that second, so it is built
    /// once and cloned after — and a clone of an already-shared value is a reference count.
    fn date_header(&mut self, timestamp: &str) -> HeaderValue {
        Self::cached(&mut self.date_header, timestamp, |value| {
            HeaderValue::from_str(value).expect("a formatted timestamp is header-safe")
        })
    }

    /// The `x-amz-security-token` header for `token`, reusing the last one while it is unchanged.
    ///
    /// Session credentials outlive many thousands of requests, so this is rebuilt only when they
    /// are refreshed. The value is marked sensitive, so a request's `Debug` output never shows it.
    fn token_header(&mut self, token: &str) -> Result<HeaderValue, SignError> {
        if let Some((cached, value)) = &self.token_header
            && cached == token
        {
            return Ok(value.clone());
        }
        let Ok(mut value) = HeaderValue::from_str(token) else {
            return Err(SignError::NonTextHeader("x-amz-security-token".to_owned()));
        };
        value.set_sensitive(true);
        self.token_header = Some((token.to_owned(), value.clone()));
        Ok(value)
    }

    /// Reuse `slot`'s value when it was built for `key`, otherwise rebuild and remember it.
    fn cached(
        slot: &mut Option<(String, HeaderValue)>,
        key: &str,
        build: impl FnOnce(&str) -> HeaderValue,
    ) -> HeaderValue {
        if let Some((cached, value)) = slot
            && cached == key
        {
            return value.clone();
        }
        let value = build(key);
        // The stored copy is what makes every later hit a reference count: cloning promotes the
        // bytes to shared storage once, here, rather than on each request.
        *slot = Some((key.to_owned(), value.clone()));
        value
    }
}

/// Sign a freshly built request, adding `x-amz-date`, `x-amz-security-token` when there is one, and
/// `Authorization`.
///
/// `payload_hash` is the hex SHA-256 of the body; the caller has the body, so it hashes once and
/// the value is reused across retries. `x-amz-content-sha256` is set here because S3 requires it
/// and `DynamoDB` accepts it, so every request carries the same proof of its body.
///
/// Every `x-amz-*` header already in `headers` is signed as it stands, a token header left by an
/// earlier signature included, so `headers` must belong to a freshly built request.
///
/// `scratch` holds the intermediate strings and, on return, the canonical request and
/// authorization. The token and authorization headers are marked sensitive, so a request's `Debug`
/// output never shows them.
pub fn sign_request(
    method: &str,
    url: &Url,
    headers: &mut HeaderMap,
    payload_hash: &str,
    context: &SigningContext<'_>,
    keys: &SigningKeyCache,
    scratch: &mut SignScratch,
) -> Result<(), SignError> {
    let mut timestamp_buffer = [0u8; TIMESTAMP_LEN];
    let timestamp = write_timestamp(&mut timestamp_buffer, context.timestamp);
    let date = &timestamp[..DATE_LEN];

    write_host(&mut scratch.host, url)?;

    let date_header = scratch.date_header(timestamp);
    headers.insert("x-amz-date", date_header);
    headers.insert("x-amz-content-sha256", payload_hash_header(payload_hash));
    if let Some(token) = context.session_token {
        let value = scratch.token_header(token)?;
        headers.insert("x-amz-security-token", value);
    }

    // Scoped so the borrow of `headers` ends before the `Authorization` header goes in. A
    // `SmallVec` has a `Drop`, which keeps its borrow alive to the end of its scope.
    {
        let canonical = canonical_headers(headers, &scratch.host)?;

        scratch.canonical_request.clear();
        write_canonical_request(
            &mut scratch.canonical_request,
            method,
            url.path(),
            url.query(),
            &canonical,
            payload_hash,
        );

        let mut canonical_hash = [0u8; 64];
        let canonical_hash =
            payload_sha256(scratch.canonical_request.as_bytes(), &mut canonical_hash);

        let string_to_sign = &mut scratch.string_to_sign;
        string_to_sign.clear();
        string_to_sign.push_str(ALGORITHM);
        string_to_sign.push('\n');
        string_to_sign.push_str(timestamp);
        string_to_sign.push('\n');
        write_scope(string_to_sign, date, context.region, context.service);
        string_to_sign.push('\n');
        string_to_sign.push_str(canonical_hash);

        let signing_key = keys.signing_key(context, date);
        let mut signature_hex = [0u8; 64];
        let signature = hex_into(
            &hmac_sha256(&signing_key, string_to_sign.as_bytes()),
            &mut signature_hex,
        );

        let authorization = &mut scratch.authorization;
        authorization.clear();
        authorization.push_str(ALGORITHM);
        authorization.push_str(" Credential=");
        authorization.push_str(context.access_key_id);
        authorization.push('/');
        write_scope(authorization, date, context.region, context.service);
        authorization.push_str(", SignedHeaders=");
        for (index, (name, _)) in canonical.iter().enumerate() {
            if index > 0 {
                authorization.push(';');
            }
            authorization.push_str(name);
        }
        authorization.push_str(", Signature=");
        authorization.push_str(signature);
    }

    let Ok(mut value) = HeaderValue::from_str(&scratch.authorization) else {
        return Err(SignError::NonTextHeader("authorization".to_owned()));
    };
    value.set_sensitive(true);
    headers.insert(http::header::AUTHORIZATION, value);

    Ok(())
}

/// The `SigV4` timestamp `YYYYMMDDTHHMMSSZ`, written into `buffer`.
///
/// The digits are written by hand rather than through `time`'s formatter, which renders into a
/// new `String`. The layout is fixed and the fields are plain integers, so writing them directly
/// allocates nothing.
///
/// The credential scope's date is the first [`DATE_LEN`] bytes of this, so one call answers both.
#[lore_macro::test_pub]
fn write_timestamp(buffer: &mut [u8; TIMESTAMP_LEN], at: UtcDateTime) -> &str {
    fn two(buffer: &mut [u8; TIMESTAMP_LEN], at: usize, value: u8) {
        buffer[at] = b'0' + value / 10;
        buffer[at + 1] = b'0' + value % 10;
    }

    // A year outside four digits cannot be rendered in this format at all; clamping keeps the
    // output well-formed rather than corrupting the fields after it.
    let year = at.year().clamp(0, 9999) as u32;
    buffer[0] = b'0' + (year / 1000) as u8;
    buffer[1] = b'0' + (year / 100 % 10) as u8;
    buffer[2] = b'0' + (year / 10 % 10) as u8;
    buffer[3] = b'0' + (year % 10) as u8;
    two(buffer, 4, u8::from(at.month()));
    two(buffer, 6, at.day());
    buffer[8] = b'T';
    two(buffer, 9, at.hour());
    two(buffer, 11, at.minute());
    two(buffer, 13, at.second());
    buffer[15] = b'Z';

    std::str::from_utf8(buffer).expect("every byte written is an ASCII digit or separator")
}

/// The `x-amz-content-sha256` header for a payload hash.
///
/// A request with no body — every GET, HEAD and DELETE, which is the whole read path — hashes to
/// the same well-known digest, and an unsigned payload is one fixed word, so a static value serves
/// both and costs nothing to build or to clone. A real body's digest is unique to it, so that one
/// is still built per request.
#[lore_macro::test_pub]
fn payload_hash_header(payload_hash: &str) -> HeaderValue {
    const EMPTY: HeaderValue = HeaderValue::from_static(EMPTY_PAYLOAD_SHA256);
    const UNSIGNED: HeaderValue = HeaderValue::from_static(UNSIGNED_PAYLOAD);

    match payload_hash {
        EMPTY_PAYLOAD_SHA256 => EMPTY,
        UNSIGNED_PAYLOAD => UNSIGNED,
        digest => HeaderValue::from_str(digest).expect("a hex digest is header-safe"),
    }
}

/// The credential scope, `date/region/service/aws4_request`, appended to `out`.
///
/// Appended twice per signature, into the string to sign and into the authorization, rather than
/// held in a buffer of its own.
fn write_scope(out: &mut String, date: &str, region: &str, service: &str) {
    out.push_str(date);
    out.push('/');
    out.push_str(region);
    out.push('/');
    out.push_str(service);
    out.push('/');
    out.push_str(TERMINATOR);
}

/// The `Host` header hyper will send, written into `out`: authority with the port only when it is
/// not the scheme's default, which is what `Url::port` already answers.
#[lore_macro::test_pub]
fn write_host(out: &mut String, url: &Url) -> Result<(), SignError> {
    let host = url.host_str().ok_or(SignError::NoHost)?;

    out.clear();
    out.push_str(host);
    if let Some(port) = url.port() {
        let _ = write!(out, ":{port}");
    }

    Ok(())
}
