// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The six S3 operations Lore makes.
//!
//! Each addresses a bucket, one object by key — the payload of a fragment, named by its content
//! hash — or the versions under a key prefix
//! (<https://docs.aws.amazon.com/AmazonS3/latest/API/API_Operations_Amazon_Simple_Storage_Service.html>).
//! There is no body to write for any of them but `PutObject`, whose body is the caller's [`Bytes`]
//! handed straight to the transport, and no body to read for any of them but `GetObject`, which is
//! streamed, and `ListObjectVersions`, which is a short XML document.
//!
//! `ListObjectVersions` follows the listing's markers to its end, so a key with more versions than
//! one page holds is listed in full. Object metadata is an ordered pair list rather than a map:
//! Lore writes one metadata key, and a lookup over one or two entries is a comparison, not a hash.
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use parking_lot::RwLock;
use smallvec::SmallVec;
use tracing::warn;
use url::Host;
use url::Url;

use crate::aws::ClientCore;
use crate::aws::ServiceProtocol;
use crate::aws::error::AwsCallError;
use crate::aws::error::ErrorPayload;
use crate::aws::error::S3ErrorCode;
use crate::aws::error::ServiceError;
use crate::aws::error::ServiceErrorCode as _;
use crate::aws::error::TransportError;
use crate::aws::error::carries_request_id;
use crate::aws::http::HttpRequest;
use crate::aws::http::HttpResponse;
use crate::aws::http::HttpStream;
use crate::aws::item::ByteStr;
use crate::aws::sign::percent_encode_into;
use crate::aws::xml::XmlError;
use crate::aws::xml::XmlEvent;
use crate::aws::xml::XmlReader;

/// The prefix S3 gives every user-defined metadata header.
pub const META_PREFIX: &str = "x-amz-meta-";

/// The header carrying the CRC32 of a `PutObject` body, which S3 checks before it stores the
/// object.
const CHECKSUM_CRC32_HEADER: &str = "x-amz-checksum-crc32";

/// How many pages of versions one `ListObjectVersions` call will follow.
///
/// A thousand pages is a million versions, which no hash in this store can plausibly have; the cap
/// exists so a service that keeps saying "truncated" cannot turn one obliteration into an unbounded
/// loop. Hitting it is reported rather than silently ignored.
const MAX_LIST_PAGES: usize = 1_000;

/// The most entries one page of a version listing holds: S3's default and largest `max-keys`.
///
/// A page naming more is not S3's answer, and reading all of it would let the body choose how much
/// this process allocates.
#[lore_macro::test_pub]
const MAX_VERSIONS_PER_PAGE: usize = 1_000;

/// A convenient alias: every operation fails the same way.
pub type S3Result<T> = Result<T, AwsCallError<S3ErrorCode>>;

/// S3's error protocol: an XML body naming the code, or nothing at all on a `HEAD`.
pub struct S3Protocol;

impl ServiceProtocol for S3Protocol {
    type Code = S3ErrorCode;

    const SERVICE: &'static str = "s3";
    const REQUEST_ID_HEADER: &'static str = "x-amz-request-id";
    const UNSIGNED_PAYLOAD_OVER_TLS: bool = true;

    fn parse_error(response: &HttpResponse) -> ServiceError<Self::Code> {
        // A failed call always keeps its request id: it is what AWS support asks for, and an error
        // is rare enough that the allocation does not matter.
        let request_id = response
            .header(Self::REQUEST_ID_HEADER)
            .map(ByteStr::copy_from_str);

        // A `HEAD` error has no body, and a proxy's error page is not XML. Either way the status is
        // the whole answer, which is what `S3ErrorCode::from_wire` falls back to.
        let parsed =
            std::str::from_utf8(&response.body).map_or_default(crate::aws::xml::parse_error_body);

        // The body's fields arrive bounded, so they are taken as they are rather than copied again.
        ServiceError {
            status: response.status,
            code: S3ErrorCode::from_wire(parsed.code.as_deref().unwrap_or(""), response.status),
            message: parsed.message,
            request_id: request_id.or(parsed.request_id),
            payload: ErrorPayload::None,
        }
    }
}

/// A metadata key read off a response, reusing the last one when it repeats.
///
/// The name is the same string on every object — a caller writes one key and reads it back — so
/// copying it per response is an allocation per object read on the hottest path there is. Cloning a
/// cached [`ByteStr`] is a reference count instead.
///
/// One entry, keyed by comparison rather than by knowing which key: which name a deployment uses is
/// the store's business, not the client's. This is the inbound counterpart to
/// [`S3Client::meta_header`], which caches the outbound `HeaderName` the same way.
fn meta_name(name: &str) -> ByteStr {
    thread_local! {
        static CACHED: RefCell<Option<ByteStr>> = const { RefCell::new(None) };
    }

    CACHED.with_borrow_mut(|cached| {
        if let Some(held) = cached.as_ref()
            && held.as_str() == name
        {
            return held.clone();
        }

        let fresh = ByteStr::copy_from_str(name);
        // The stored copy is what makes later hits a reference count: cloning promotes the bytes to
        // shared storage once, here, rather than on every response.
        *cached = Some(fresh.clone());

        fresh
    })
}

/// An object's user-defined metadata, in insertion order.
///
/// Two entries inline: Lore writes one key, and the second slot leaves room for an object written
/// by something else to round-trip without allocating. Ordered rather than hashed because a lookup
/// over one or two entries is a comparison, not a hash and a probe.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ObjectMetadata(SmallVec<[(ByteStr, ByteStr); 2]>);

impl ObjectMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `name`, replacing an existing entry of the same name.
    ///
    /// Replacing rather than appending keeps the mapping single-valued, which is what S3 will do to
    /// it anyway — and it keeps ordering a function of the input, so two requests built the same
    /// way serialize the same way.
    pub fn insert(&mut self, name: ByteStr, value: ByteStr) {
        match self.0.iter_mut().find(|(held, _)| *held == name) {
            Some((_, held)) => *held = value,
            None => self.0.push((name, value)),
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(held, _)| held.as_str() == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// The entries with their storage intact, for a caller that can take a reference count where
    /// [`Self::iter`] would leave it only a copy.
    pub fn pairs(&self) -> impl Iterator<Item = (&ByteStr, &ByteStr)> {
        self.0.iter().map(|(name, value)| (name, value))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Take the `x-amz-meta-*` headers off a response, with the prefix stripped.
    ///
    /// A value that is not text is kept as an empty one rather than dropped. That is deliberate: an
    /// object whose metadata is unreadable is damaged, and it must not be mistaken for an object
    /// written before the metadata existed — those two cases lead to different recovery paths in
    /// the store.
    #[lore_macro::test_pub]
    fn from_headers(headers: &http::HeaderMap) -> Self {
        let mut metadata = Self::new();

        for (name, value) in headers {
            let Some(name) = name.as_str().strip_prefix(META_PREFIX) else {
                continue;
            };

            let value = if let Ok(text) = value.to_str() {
                ByteStr::copy_from_str(text)
            } else {
                warn!(
                    metadata_key = name,
                    "Object metadata value is not text; treating it as damaged rather than absent"
                );
                ByteStr::default()
            };

            metadata.insert(meta_name(name), value);
        }

        metadata
    }
}

impl<const N: usize> From<[(ByteStr, ByteStr); N]> for ObjectMetadata {
    fn from(entries: [(ByteStr, ByteStr); N]) -> Self {
        let mut metadata = Self::new();
        for (name, value) in entries {
            metadata.insert(name, value);
        }
        metadata
    }
}

/// What `HeadObject` answers.
#[derive(Debug, Default)]
pub struct HeadObjectOutput {
    pub metadata: ObjectMetadata,
    /// The stored object's length, as the service reports it.
    pub content_length: Option<u64>,
    pub request_id: Option<ByteStr>,
}

impl HeadObjectOutput {
    pub fn metadata(&self) -> &ObjectMetadata {
        &self.metadata
    }

    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }
}

/// What `GetObject` answers: the metadata, the length, and a body still on the wire.
#[derive(Debug)]
pub struct GetObjectOutput {
    pub metadata: ObjectMetadata,
    /// The length of *this response* — the range's length for a ranged read.
    pub content_length: Option<u64>,
    pub body: ByteStream,
    pub request_id: Option<ByteStr>,
}

impl GetObjectOutput {
    pub fn metadata(&self) -> &ObjectMetadata {
        &self.metadata
    }

    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }
}

/// What a write answers: nothing the callers read but its request id.
#[derive(Debug, Default)]
pub struct WriteOutput {
    pub request_id: Option<ByteStr>,
}

/// One version of one key, as `ListObjectVersions` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectVersion {
    pub key: ByteStr,
    /// The id to delete this version by, as the listing reports it.
    ///
    /// An object written while the bucket's versioning was off or suspended is the null version,
    /// which the listing reports as the literal `null`; deleting with `versionId=null` removes it,
    /// where deleting with no version id on a versioned bucket only adds a delete marker. Absent
    /// only when the listing carried an empty id.
    pub version_id: Option<ByteStr>,
    /// Whether this entry is a delete marker rather than stored bytes.
    ///
    /// A delete marker is a version with an id of its own, and deleting that id removes the marker.
    pub is_delete_marker: bool,
    pub is_latest: bool,
}

/// Every version of every key under a prefix, across as many pages as it took.
#[derive(Debug, Default)]
pub struct ListObjectVersionsOutput {
    pub versions: Vec<ObjectVersion>,
    /// Set only when the page cap or a service that stopped advancing its markers cut the listing
    /// short. A caller that must be exhaustive has to treat this as "not finished".
    pub truncated: bool,
    pub request_id: Option<ByteStr>,
}

impl ListObjectVersionsOutput {
    /// The versions of exactly one key, since a listing is by prefix.
    pub fn versions_of(&self, key: &str) -> impl Iterator<Item = &ObjectVersion> {
        self.versions
            .iter()
            .filter(move |version| version.key.as_str() == key)
    }
}

carries_request_id!(
    HeadObjectOutput,
    GetObjectOutput,
    WriteOutput,
    ListObjectVersionsOutput,
);

/// A `GetObject` body, read chunk by chunk.
///
/// Each chunk is a [`Bytes`] handed over from hyper, so the caller's copy into its own buffer is
/// the only one the payload takes.
#[derive(Debug)]
pub enum ByteStream {
    /// Still on the wire, which is every response from a real service. Boxed because it is an order
    /// of magnitude larger than the other variant, and this type sits on every `GetObject` output.
    Wire(Box<HttpStream>),
    /// Bytes already in hand, for a caller that has the payload without a socket — the in-memory
    /// store the unit tests run against.
    Buffered(Option<Bytes>),
    /// A body that stops partway, which is what a reset connection looks like to a reader. Only a
    /// real socket can produce one, and a socket cannot be made to drop on cue, so the readers
    /// above get this instead.
    #[cfg(feature = "test-util")]
    Truncated(Option<Bytes>),
    /// A body that arrives in pieces, as a chunked reply does. [`Self::Buffered`] hands its whole
    /// body over at once, so it cannot exercise a reader that bounds what it accumulates.
    #[cfg(feature = "test-util")]
    Chunked(std::vec::IntoIter<Bytes>),
}

impl ByteStream {
    pub fn from_bytes(bytes: Bytes) -> Self {
        Self::Buffered(Some(bytes))
    }

    /// A stream that hands over `head` and then fails, as a dropped connection does.
    #[cfg(feature = "test-util")]
    pub fn truncated(head: Bytes) -> Self {
        Self::Truncated(Some(head))
    }

    /// A stream that hands `body` over in pieces of at most `chunk` bytes.
    #[cfg(feature = "test-util")]
    pub fn chunked(body: Bytes, chunk: usize) -> Self {
        let pieces = (0..body.len())
            .step_by(chunk)
            .map(|start| body.slice(start..(start + chunk).min(body.len())))
            .collect::<Vec<_>>();

        Self::Chunked(pieces.into_iter())
    }

    /// The next chunk, or `None` at the end of the body.
    ///
    /// Shaped as `Option<Result<_>>` rather than `Result<Option<_>>` so a `while let Some(chunk)`
    /// loop reads the body to its end and meets a failure as one of its chunks.
    pub async fn next(&mut self) -> Option<Result<Bytes, TransportError>> {
        match self {
            Self::Wire(stream) => match stream.chunk().await {
                Ok(Some(chunk)) => Some(Ok(chunk)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            },
            Self::Buffered(bytes) => bytes.take().map(Ok),
            #[cfg(feature = "test-util")]
            Self::Truncated(head) => Some(match head.take() {
                Some(chunk) => Ok(chunk),
                None => Err(truncated_body()),
            }),
            #[cfg(feature = "test-util")]
            Self::Chunked(pieces) => pieces.next().map(Ok),
        }
    }

    /// The whole remaining body in one buffer.
    pub async fn collect(self) -> Result<Bytes, TransportError> {
        match self {
            Self::Wire(stream) => stream.bytes().await,
            Self::Buffered(bytes) => Ok(bytes.unwrap_or_default()),
            #[cfg(feature = "test-util")]
            Self::Truncated(_) => Err(truncated_body()),
            #[cfg(feature = "test-util")]
            Self::Chunked(pieces) => Ok(pieces.flatten().collect()),
        }
    }
}

#[cfg(feature = "test-util")]
fn truncated_body() -> TransportError {
    TransportError::Body("the connection dropped mid-body".to_owned())
}

/// How a bucket is addressed: in the host, or in the path.
///
/// Cached because a client serves one bucket for its lifetime, so the host string and its URL are
/// built once rather than per request. One entry, for the same reason
/// [`crate::aws::sign::SigningKeyCache`] holds one.
#[lore_macro::test_pub]
#[derive(Debug)]
struct Addressing {
    force_path_style: bool,
    cached: RwLock<Option<Arc<BucketBase>>>,
}

/// A bucket's resolved base: the origin to send to, and the path prefix its keys hang off.
#[lore_macro::test_pub]
#[derive(Debug)]
struct BucketBase {
    bucket: Box<str>,
    /// Scheme, host and port. Path and query are always replaced per request.
    url: Url,
    /// The endpoint's own path, without its trailing `/`, followed by `/{bucket}` when the bucket
    /// is in the path. `""` for a bucket in the host of an endpoint with no path.
    prefix: Box<str>,
}

impl Addressing {
    fn new(force_path_style: bool) -> Self {
        Self {
            force_path_style,
            cached: RwLock::new(None),
        }
    }

    #[lore_macro::test_pub]
    fn base(&self, endpoint: &Url, bucket: &str) -> Arc<BucketBase> {
        if let Some(cached) = self.cached.read().as_ref()
            && &*cached.bucket == bucket
        {
            return cached.clone();
        }

        let built = Arc::new(self.build(endpoint, bucket));
        *self.cached.write() = Some(built.clone());

        built
    }

    fn build(&self, endpoint: &Url, bucket: &str) -> BucketBase {
        let mut url = endpoint.clone();
        url.set_query(None);
        url.set_path("/");

        let mut prefix = String::from(endpoint.path().trim_end_matches('/'));

        // A bucket goes in the host only when it can: not when the caller asked for path style, not
        // against an IP endpoint (LocalStack, MinIO, a test stub — there is no name to prefix), and
        // not when the name is not a DNS label. `set_host` failing is the same answer arrived at
        // later, so it falls back rather than failing the request.
        let in_host = !self.force_path_style
            && matches!(endpoint.host(), Some(Host::Domain(_)))
            && is_dns_label(bucket)
            && {
                let host = format!("{bucket}.{}", endpoint.host_str().unwrap_or_default());
                url.set_host(Some(&host)).is_ok()
            };

        if !in_host {
            prefix.push('/');
            percent_encode_into(&mut prefix, bucket, false);
        }

        BucketBase {
            bucket: bucket.into(),
            url,
            prefix: prefix.into(),
        }
    }
}

/// Whether `bucket` can be the leading label of a host name.
///
/// Dots are excluded even though S3 allows them in a bucket name: a dotted bucket in the host does
/// not match the wildcard certificate S3 serves, so TLS fails. The path is where those belong.
#[lore_macro::test_pub]
fn is_dns_label(bucket: &str) -> bool {
    if !(3..=63).contains(&bucket.len()) {
        return false;
    }

    let usable = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = bucket.as_bytes();

    bytes.iter().all(|byte| usable(*byte) || *byte == b'-')
        && usable(bytes[0])
        && usable(bytes[bytes.len() - 1])
}

/// A client for one S3 endpoint.
#[lore_macro::test_pub]
#[derive(Debug, Clone)]
pub struct S3Client {
    core: Arc<ClientCore>,
    addressing: Arc<Addressing>,
}

impl S3Client {
    pub fn new(core: Arc<ClientCore>, force_path_style: bool) -> Self {
        Self {
            core,
            addressing: Arc::new(Addressing::new(force_path_style)),
        }
    }

    pub fn core(&self) -> &Arc<ClientCore> {
        &self.core
    }

    /// Whether a bucket exists and this identity can see it, which is all `ensure_bucket` needs.
    ///
    /// A 404 is the answer "no", not a failure. A 403 stays a failure: the bucket may well exist,
    /// and reporting it as absent would send a deployment looking for the wrong problem.
    pub async fn bucket_exists(&self, bucket: &str) -> S3Result<bool> {
        let send = self
            .core
            .send::<S3Protocol>(|| HttpRequest::new(Method::HEAD, self.url(bucket, None, None)));

        match send.await {
            Ok(_) => Ok(true),
            Err(AwsCallError::Service(error)) if error.code.is_not_found() => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Read an object's metadata without transferring its bytes.
    pub async fn head_object(&self, bucket: &str, key: &str) -> S3Result<HeadObjectOutput> {
        check_key(key)?;

        let response = self
            .core
            .send::<S3Protocol>(|| {
                HttpRequest::new(Method::HEAD, self.url(bucket, Some(key), None))
            })
            .await?;

        Ok(HeadObjectOutput {
            metadata: ObjectMetadata::from_headers(&response.headers),
            content_length: content_length(&response.headers),
            request_id: self.request_id(response.header(S3Protocol::REQUEST_ID_HEADER)),
        })
    }

    /// Read an object, leaving its body on the wire for the caller to stream.
    ///
    /// `range` is a Rust half-open range; HTTP's is inclusive on both ends, so the end is
    /// decremented. An empty range is refused rather than sent, because `bytes=5-4` is a request S3
    /// answers with a 416 and no useful diagnosis. A ranged read must be answered with `206 Partial
    /// Content`: a `200` carries the whole object, which an intermediary that ignored the range
    /// sends, and reading it as the range would hand the caller the wrong bytes.
    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<Range<usize>>,
    ) -> S3Result<GetObjectOutput> {
        check_key(key)?;

        // Rendered and validated once, outside the builder, which cannot fail. The value is at most
        // forty-eight bytes, so an attempt copying it is not worth avoiding.
        let range_header = match range {
            Some(range) if range.start >= range.end => {
                return Err(AwsCallError::Transport(TransportError::Build(format!(
                    "a GetObject range must not be empty, got {}..{}",
                    range.start, range.end
                ))));
            }
            Some(range) => {
                let mut buffer = [0u8; 48];
                let value = write_byte_range(&mut buffer, range.start, range.end - 1);

                Some(HeaderValue::from_str(value).map_err(|_invalid| {
                    AwsCallError::Transport(TransportError::Build(format!(
                        "byte range {value:?} is not a valid header value"
                    )))
                })?)
            }
            None => None,
        };

        let response = self
            .core
            .send_streaming::<S3Protocol>(|| {
                let mut request = HttpRequest::new(Method::GET, self.url(bucket, Some(key), None));

                if let Some(value) = range_header.clone() {
                    request.headers.insert(http::header::RANGE, value);
                }

                request
            })
            .await?;

        if range_header.is_some() && response.status() != http::StatusCode::PARTIAL_CONTENT {
            return Err(AwsCallError::Transport(TransportError::Protocol(format!(
                "a ranged GetObject was answered with {} rather than 206 Partial Content",
                response.status()
            ))));
        }

        Ok(GetObjectOutput {
            metadata: ObjectMetadata::from_headers(response.headers()),
            content_length: content_length(response.headers()),
            request_id: self.request_id(response.header(S3Protocol::REQUEST_ID_HEADER)),
            body: ByteStream::Wire(Box::new(response)),
        })
    }

    /// The `x-amz-meta-*` header for `name`, built once per name per thread.
    ///
    /// `HeaderName::from_bytes` copies, a name outside the standard set having nowhere to borrow
    /// from, so building the header inline costs two allocations per object. Cloning the cached one
    /// is a reference count instead: `HeaderName` holds its text in [`bytes::Bytes`].
    ///
    /// One entry, because a caller writes the same key on every object. A second name replaces the
    /// first and is served just as cheaply for as long as it is the one being written.
    fn meta_header(name: &str) -> Result<HeaderName, AwsCallError<S3ErrorCode>> {
        thread_local! {
            static CACHED: RefCell<Option<(Box<str>, HeaderName)>> = const { RefCell::new(None) };
        }

        CACHED.with_borrow_mut(|cached| {
            if let Some((cached_name, header)) = cached.as_ref()
                && &**cached_name == name
            {
                return Ok(header.clone());
            }

            let mut full = String::with_capacity(META_PREFIX.len() + name.len());
            full.push_str(META_PREFIX);
            full.push_str(name);

            let header = HeaderName::from_bytes(full.as_bytes()).map_err(|_invalid| {
                AwsCallError::Transport(TransportError::Build(format!(
                    "object metadata key {name:?} is not a valid header name"
                )))
            })?;

            *cached = Some((name.into(), header.clone()));

            Ok(header)
        })
    }

    /// Store an object, with the metadata that describes the bytes going with them.
    ///
    /// The metadata travels as `x-amz-meta-*` headers, every one of which is signed, so it cannot
    /// be changed in flight without invalidating the request. A name or value that is not
    /// header-safe is refused here rather than escaping into the request — there is no way to
    /// smuggle a header break through an object's metadata.
    ///
    /// The body's CRC32 goes with it, signed like the metadata, so S3 refuses a body damaged on
    /// the way with `BadDigest` rather than storing it. A body sent over TLS is signed unhashed,
    /// so this is the only check S3 makes of its bytes. The sum is taken on the calling task
    /// rather than the blocking pool: a body is at most one fragment, and summing it costs less than
    /// the hop.
    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: Bytes,
        metadata: Option<&ObjectMetadata>,
    ) -> S3Result<WriteOutput> {
        check_key(key)?;

        // Validated once, before the first attempt, because the builder below cannot fail. Both
        // halves hold their text in `Bytes`, so an attempt clones reference counts rather than
        // bytes.
        let mut headers: SmallVec<[(HeaderName, HeaderValue); 2]> = SmallVec::new();
        headers.push((
            HeaderName::from_static(CHECKSUM_CRC32_HEADER),
            crc32_header(&body)?,
        ));
        for (name, value) in metadata.into_iter().flat_map(ObjectMetadata::pairs) {
            let header = Self::meta_header(name.as_str())?;
            let value =
                HeaderValue::from_maybe_shared(value.as_bytes().clone()).map_err(|_invalid| {
                    AwsCallError::Transport(TransportError::Build(format!(
                        "object metadata value for {name:?} is not a valid header value"
                    )))
                })?;

            headers.push((header, value));
        }

        let response = self
            .core
            .send::<S3Protocol>(|| {
                let mut request = HttpRequest::new(Method::PUT, self.url(bucket, Some(key), None))
                    .body(body.clone());

                for (name, value) in &headers {
                    request.headers.insert(name.clone(), value.clone());
                }

                request
            })
            .await?;

        Ok(WriteOutput {
            request_id: self.request_id(response.header(S3Protocol::REQUEST_ID_HEADER)),
        })
    }

    /// Delete an object, or one version of it.
    ///
    /// With no version this writes a delete marker on a versioned bucket; with one it removes those
    /// bytes permanently, which is what obliteration needs.
    pub async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> S3Result<WriteOutput> {
        check_key(key)?;

        let query = version_id.map(|version| {
            let mut query = String::with_capacity("versionId=".len() + version.len());
            query.push_str("versionId=");
            percent_encode_into(&mut query, version, false);
            query
        });

        let response = self
            .core
            .send::<S3Protocol>(|| {
                HttpRequest::new(
                    Method::DELETE,
                    self.url(bucket, Some(key), query.as_deref()),
                )
            })
            .await?;

        Ok(WriteOutput {
            request_id: self.request_id(response.header(S3Protocol::REQUEST_ID_HEADER)),
        })
    }

    /// Every version under `prefix`, following the listing to its end.
    ///
    /// The service answers at most a thousand entries per page and says so; each further page is a
    /// separate request under its own budget.
    pub async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> S3Result<ListObjectVersionsOutput> {
        let mut output = ListObjectVersionsOutput::default();
        let mut key_marker: Option<String> = None;
        let mut version_marker: Option<String> = None;

        for page_number in 0..MAX_LIST_PAGES {
            let query = versions_query(prefix, key_marker.as_deref(), version_marker.as_deref());
            let response = self
                .core
                .send::<S3Protocol>(|| {
                    HttpRequest::new(Method::GET, self.url(bucket, None, Some(&query)))
                })
                .await?;

            output.request_id = self.request_id(response.header(S3Protocol::REQUEST_ID_HEADER));

            let mut page = parse_versions_page(&response.body).map_err(|error| {
                AwsCallError::Transport(TransportError::Protocol(error.to_string()))
            })?;

            output.versions.append(&mut page.versions);

            if !page.truncated {
                return Ok(output);
            }

            // A truncated page that does not move the cursor would be requested for ever. Both
            // shapes of that — no markers at all, and markers identical to the ones just sent —
            // stop here and report the listing as unfinished.
            if (page.next_key_marker.is_none() && page.next_version_id_marker.is_none())
                || (page.next_key_marker == key_marker
                    && page.next_version_id_marker == version_marker)
            {
                warn!(
                    bucket,
                    prefix,
                    page = page_number,
                    versions = output.versions.len(),
                    "S3 reported a truncated version listing without advancing its markers; \
                     the listing is incomplete"
                );
                output.truncated = true;

                return Ok(output);
            }

            key_marker = page.next_key_marker;
            version_marker = page.next_version_id_marker;
        }

        warn!(
            bucket,
            prefix,
            pages = MAX_LIST_PAGES,
            versions = output.versions.len(),
            "S3 version listing hit the page cap; the listing is incomplete"
        );
        output.truncated = true;

        Ok(output)
    }

    /// A URL for one request. `key` is `None` for the bucket-level operations.
    ///
    /// The path is percent-encoded here, to the unreserved set with `/` kept, which is exactly the
    /// canonical URI the signature is computed over — so what is signed and what is sent cannot
    /// drift apart. `Url::set_path` leaves an existing `%` alone, so this does not double-encode.
    /// It does resolve `.` and `..` segments, which is why every object operation passes its key
    /// through [`check_key`] first.
    #[lore_macro::test_pub]
    fn url(&self, bucket: &str, key: Option<&str>, query: Option<&str>) -> Url {
        let base = self.addressing.base(self.core.endpoint(), bucket);
        let mut url = base.url.clone();

        match key {
            Some(key) => {
                // Three bytes per encoded character is the worst case, but keys are hex, so
                // reserving the exact length plus the separator is right for every real call.
                let mut path = String::with_capacity(base.prefix.len() + 1 + key.len());
                path.push_str(&base.prefix);
                path.push('/');
                percent_encode_into(&mut path, key, true);
                url.set_path(&path);
            }
            // `/` for a bucket in the host, `/{bucket}` for one in the path.
            None if !base.prefix.is_empty() => url.set_path(&base.prefix),
            None => {}
        }

        url.set_query(query);

        url
    }

    /// The response's request id, when this client was built to capture one.
    ///
    /// Off unless a slow-operation threshold is configured: only that warning reads it, so
    /// capturing one per response would otherwise be an allocation for nothing.
    fn request_id(&self, header: Option<&str>) -> Option<ByteStr> {
        if !self.core.capture_request_id() {
            return None;
        }

        header.map(ByteStr::copy_from_str)
    }
}

/// Refuse a key the request URL would not carry as written.
///
/// A path segment of `.` or `..` is resolved away when the URL is built, so the request would
/// address a different key — or, with the bucket in the path, a different bucket. Nothing is sent
/// for such a key.
#[lore_macro::test_pub]
fn check_key(key: &str) -> S3Result<()> {
    if key
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(AwsCallError::Transport(TransportError::Build(format!(
            "object key {key:?} has a `.` or `..` segment, which the request URL would resolve away"
        ))));
    }

    Ok(())
}

/// `bytes=<first>-<last>`, written into `buffer`.
///
/// Two `usize`s are at most twenty digits each, so a 48-byte buffer cannot be overrun by any range
/// a caller can express.
fn write_byte_range(buffer: &mut [u8; 48], first: usize, last: usize) -> &str {
    use std::io::Write as _;

    let capacity = buffer.len();
    let written = {
        let mut cursor = &mut buffer[..];
        let _ = write!(cursor, "bytes={first}-{last}");
        capacity - cursor.len()
    };

    std::str::from_utf8(&buffer[..written]).unwrap_or_default()
}

/// The `x-amz-checksum-crc32` value for `body`: its CRC32, big-endian and base64-encoded.
fn crc32_header(body: &[u8]) -> S3Result<HeaderValue> {
    let unbuildable = |why: &str| {
        AwsCallError::Transport(TransportError::Build(format!(
            "the CRC32 header could not be built: {why}"
        )))
    };

    let mut encoded = [0u8; 8];
    let written = BASE64
        .encode_slice(crc32fast::hash(body).to_be_bytes(), &mut encoded)
        .map_err(|error| unbuildable(&error.to_string()))?;

    HeaderValue::from_bytes(&encoded[..written]).map_err(|error| unbuildable(&error.to_string()))
}

/// `Content-Length`, when the service sent one this client can read.
#[lore_macro::test_pub]
fn content_length(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// The query for one page of a version listing.
///
/// Canonicalization sorts the parameters and supplies the `=` for the valueless `versions` flag, so
/// the order here is the order S3 sees and nothing more.
#[lore_macro::test_pub]
fn versions_query(prefix: &str, key_marker: Option<&str>, version_marker: Option<&str>) -> String {
    let mut query = String::with_capacity(64 + prefix.len());
    query.push_str("versions&prefix=");
    percent_encode_into(&mut query, prefix, false);

    if let Some(marker) = key_marker {
        query.push_str("&key-marker=");
        percent_encode_into(&mut query, marker, false);
    }
    if let Some(marker) = version_marker {
        query.push_str("&version-id-marker=");
        percent_encode_into(&mut query, marker, false);
    }

    query
}

/// One page of `ListObjectVersions`.
#[lore_macro::test_pub]
#[derive(Debug, Default, PartialEq)]
struct VersionsPage {
    versions: Vec<ObjectVersion>,
    truncated: bool,
    next_key_marker: Option<String>,
    next_version_id_marker: Option<String>,
}

/// The root element of a `ListObjectVersions` answer.
const VERSIONS_ROOT: &str = "ListVersionsResult";

/// Read a page, taking the four fields that matter and stepping over everything else.
///
/// Deliberately tolerant of what it does not know — `Owner`, `StorageClass`, `CommonPrefixes`, a
/// namespace prefix on the root — and intolerant of a document that does not parse or is not a
/// version listing, which is a truncated or misdirected response rather than a service answer.
#[lore_macro::test_pub]
fn parse_versions_page(body: &[u8]) -> Result<VersionsPage, TransportError> {
    let body = std::str::from_utf8(body).map_err(|error| {
        TransportError::Protocol(format!("version listing is not UTF-8: {error}"))
    })?;

    read_versions_page(&mut XmlReader::new(body))
        .map_err(|error| TransportError::Protocol(format!("malformed version listing: {error}")))
}

/// The page under a `ListVersionsResult` root, up to and including the root's close.
fn read_versions_page(reader: &mut XmlReader<'_>) -> Result<VersionsPage, XmlError> {
    let root = loop {
        match reader.next_event()? {
            Some(XmlEvent::Open(name)) => break name,
            Some(XmlEvent::Text(_)) => {}
            Some(XmlEvent::Close(_)) | None => return Err(XmlError::NoRoot),
        }
    };

    if local_name(root) != VERSIONS_ROOT {
        return Err(XmlError::UnexpectedElement {
            expected: VERSIONS_ROOT.to_owned(),
            found: root.to_owned(),
        });
    }

    let mut page = VersionsPage::default();

    loop {
        match reader.next_event()? {
            Some(XmlEvent::Open(name)) => match name {
                "Version" | "DeleteMarker" => {
                    if page.versions.len() == MAX_VERSIONS_PER_PAGE {
                        return Err(XmlError::TooMany {
                            element: "Version".to_owned(),
                            limit: MAX_VERSIONS_PER_PAGE,
                        });
                    }
                    page.versions
                        .push(read_version(reader, name == "DeleteMarker")?);
                }
                "IsTruncated" => page.truncated = reader.text(name)?.trim() == "true",
                "NextKeyMarker" => page.next_key_marker = non_empty(&reader.text(name)?),
                "NextVersionIdMarker" => {
                    page.next_version_id_marker = non_empty(&reader.text(name)?);
                }
                // `Name`, `Prefix`, `MaxKeys`, `CommonPrefixes`, anything a future API version
                // adds.
                other => reader.skip_element(other)?,
            },
            Some(XmlEvent::Close(_)) => return Ok(page),
            Some(XmlEvent::Text(_)) => {}
            None => return Err(XmlError::ExpectedText(root.to_owned())),
        }
    }
}

/// An element name without its namespace prefix.
fn local_name(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, local)| local)
}

/// One `<Version>` or `<DeleteMarker>` element, up to and including its close.
fn read_version(
    reader: &mut XmlReader<'_>,
    is_delete_marker: bool,
) -> Result<ObjectVersion, XmlError> {
    let mut version = ObjectVersion {
        key: ByteStr::default(),
        version_id: None,
        is_delete_marker,
        is_latest: false,
    };

    loop {
        match reader.next_event()? {
            Some(XmlEvent::Open(field)) => match field {
                "Key" => version.key = ByteStr::from(reader.text(field)?),
                "VersionId" => {
                    let id = reader.text(field)?;
                    version.version_id = (!id.is_empty()).then(|| ByteStr::from(id));
                }
                "IsLatest" => version.is_latest = reader.text(field)?.trim() == "true",
                // `LastModified`, `ETag`, `Size`, `StorageClass`, and `Owner` with its own
                // children.
                other => reader.skip_element(other)?,
            },
            Some(XmlEvent::Close(_)) => return Ok(version),
            Some(XmlEvent::Text(_)) => {}
            None => {
                return Err(XmlError::ExpectedText("Version".to_owned()));
            }
        }
    }
}

/// A marker S3 sends as an empty element means "none", not "the empty key".
fn non_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}
