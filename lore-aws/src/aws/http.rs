// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The HTTP transport: one pooled client per service, dispatched on the net runtime.
//!
//! Lore splits its runtimes, and a pooled connection's driver task must live on net. hyper spawns
//! that driver itself, lazily, onto whatever runtime is current when the request future is first
//! polled — so the way to place it is to await the request from a net task, which is what
//! [`HttpClient::send`] does.
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use bytes::BytesMut;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use lore_base::lore_spawn_net;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use opentelemetry::metrics::UpDownCounter;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::task::AbortOnDropHandle;
use url::Url;

use crate::aws::error::TransportError;
use crate::aws::retry::deadline_after;
use crate::clients::HttpClientSettings;
use crate::clients::StoreKind;

/// The largest body any operation will read, buffered or streamed.
///
/// Four times the largest response any of these operations can legitimately produce —
/// `BatchGetItem` caps at 16 MiB — so it cannot reject a real answer, and it bounds what a
/// misdirected endpoint can make this process allocate.
///
/// Both paths enforce it on the read that crosses it, and neither keeps that read's bytes, so the
/// cap bounds what is held rather than reporting that too much already is. One transport read may
/// be in hand when it fires: hyper reads into a buffer of its own choosing, capped by its read
/// strategy, so what a response declares — a chunk length, a `Content-Length` — cannot enlarge
/// that.
const MAX_BUFFERED_BODY: u64 = 64 * 1024 * 1024;

/// The cap has to sit above every legitimate response, or it turns a working call into a failure.
/// `BatchGetItem`, the largest of them, caps at 16 MiB. Checked at build time rather than in a
/// test, because it is a property of the constant rather than of any behaviour.
const _: () = assert!(MAX_BUFFERED_BODY > 16 * 1024 * 1024);

/// How many headers a request has room for before its map regrows.
///
/// A signed request carries up to six of its own — content type, target, date, payload hash,
/// session token and authorization — an S3 write adds its checksum and object metadata, and the
/// transport adds `accept` and `user-agent`. A map made with room for eight holds twelve before it
/// regrows.
const REQUEST_HEADER_CAPACITY: usize = 8;

/// The `user-agent` every request carries, so the service's logs name Lore and its version.
const USER_AGENT: &str = concat!("lore/", env!("CARGO_PKG_VERSION"));

/// The failure a body past `cap` is reported as.
///
/// A protocol failure rather than a body one: the response was read fine, it is simply not a
/// response this client will hold.
fn body_over_cap(cap: u64) -> TransportError {
    TransportError::Protocol(format!("response body exceeds the {cap} byte limit"))
}

/// The failure a streamed body still arriving when the operation's budget runs out is reported as.
fn body_past_deadline() -> TransportError {
    TransportError::Body(
        "the response body did not arrive within the operation's budget".to_owned(),
    )
}

/// The failure a streamed response whose headers did not arrive within the attempt is reported as.
fn no_response_in_time() -> TransportError {
    TransportError::Io("no response within the attempt's timeout".to_owned())
}

/// A response whose body has been read in full.
///
/// `Debug` shows the body's length rather than its bytes: a credentials endpoint answers with the
/// secret in the body.
pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl HttpResponse {
    /// A header's value as text, or `None` when it is absent or not text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// A response whose body has not been read, for `GetObject`.
///
/// The status and headers are read from the response, which holds them.
pub struct HttpStream {
    response: reqwest::Response,
    /// How many more bytes this body may hand over before it is refused.
    remaining: u64,
    /// The cap `remaining` started from, for the failure that reports it.
    cap: u64,
    /// When the operation's budget runs out, which every read of the body must finish by.
    deadline: Instant,
    /// The permit for this response's connection, for a service client.
    ///
    /// The connection is in use until the body has been read or the stream is dropped, so the
    /// stream holds the permit until it is dropped.
    _connection: Option<ConnectionPermit>,
}

impl HttpStream {
    /// The response's status.
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    /// The response's headers.
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    /// A header's value as text, or `None` when it is absent or not text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.response.headers().get(name)?.to_str().ok()
    }

    /// The headers, moved out of the response and leaving it with none.
    pub(crate) fn take_headers(&mut self) -> HeaderMap {
        std::mem::take(self.response.headers_mut())
    }

    /// The next chunk of the body, or `None` at its end.
    ///
    /// Each chunk is a [`Bytes`] handed over from hyper, so moving it into a caller's buffer is the
    /// only copy the body takes.
    ///
    /// A body that grows past the client's cap — [`MAX_BUFFERED_BODY`] — is refused on the read
    /// that crosses it, and that read's bytes are dropped rather than returned, so a caller that
    /// keeps what it is given cannot be made to hold an unbounded body. `Content-Length` does not
    /// cover this: a chunked reply declares none, which is the case the cap exists for.
    ///
    /// A read not finished by the operation's deadline fails as a body failure, as a body that stops
    /// partway does.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, TransportError> {
        let chunk = tokio::time::timeout_at(self.deadline, self.response.chunk())
            .await
            .map_err(|_elapsed| body_past_deadline())?
            .map_err(|error| TransportError::Body(error.to_string()))?;

        let Some(chunk) = chunk else {
            return Ok(None);
        };

        let cap = self.cap;
        self.remaining = self
            .remaining
            .checked_sub(chunk.len() as u64)
            .ok_or_else(|| body_over_cap(cap))?;

        Ok(Some(chunk))
    }

    /// Read the rest of the body into one buffer.
    ///
    /// Bounded by what is left of this stream's allowance rather than by a fresh cap, so reading
    /// some of a body in chunks and the rest here cannot hand over more in total than either path
    /// alone.
    pub async fn bytes(mut self) -> Result<Bytes, TransportError> {
        let cap = self.cap;

        tokio::time::timeout_at(
            self.deadline,
            read_capped(&mut self.response, self.remaining),
        )
        .await
        .map_err(|_elapsed| body_past_deadline())?
        .map_err(|error| TransportError::Body(error.to_string()))?
        .ok_or_else(|| body_over_cap(cap))
    }
}

impl fmt::Debug for HttpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpStream")
            .field("status", &self.response.status())
            .finish_non_exhaustive()
    }
}

/// A request for one attempt.
///
/// The body is [`Bytes`], so the requests an operation builds for its attempts share one payload by
/// reference count rather than each copying it.
///
/// `Debug` shows the body's length rather than its bytes, because a body can carry a credential —
/// the STS request carries the web identity token. Headers that carry one are marked sensitive
/// where they are set, so their values do not show either.
pub struct HttpRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// The largest response body this request accepts. The client's own cap applies as well, so
    /// `u64::MAX` leaves that one alone.
    pub response_cap: u64,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url.as_str())
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("response_cap", &self.response_cap)
            .finish()
    }
}

impl HttpRequest {
    pub fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HeaderMap::with_capacity(REQUEST_HEADER_CAPACITY),
            body: Bytes::new(),
            response_cap: u64::MAX,
        }
    }

    pub fn response_cap(mut self, cap: u64) -> Self {
        self.response_cap = cap;
        self
    }

    pub fn header(mut self, name: &'static str, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    pub fn body(mut self, body: Bytes) -> Self {
        self.body = body;
        self
    }
}

/// A pooled HTTP client.
///
/// Cheap to clone: the pool lives behind reqwest's own `Arc`, so every clone shares the same
/// connections, and the same limit on them.
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    /// The largest body a response may carry: [`MAX_BUFFERED_BODY`] unless a test lowered it.
    body_cap: u64,
    /// The connections a service client has in use, and its limit on them. The credential clients
    /// count none.
    connections: Option<Arc<Connections>>,
}

impl HttpClient {
    /// Build a client from the settings the AWS store plugins already take.
    ///
    /// HTTP/1.1 only. No connect timeout, matching the deliberate choice recorded in `clients.rs` —
    /// the operation deadline bounds the whole call instead.
    ///
    /// Redirects are not followed. A signature covers the host and path it was computed over, so a
    /// followed redirect would arrive unsigned for its new target and be rejected; a service that
    /// answers with one — S3 telling us a bucket lives in another region, say — is reporting a
    /// misconfiguration that belongs to the caller, not something to chase silently.
    ///
    /// The environment's proxy is not used: the store's traffic goes to the service directly, as it
    /// does with the AWS SDK, rather than following a proxy set for other egress.
    ///
    /// When the settings set `max_connections`, a request takes a permit for its connection before
    /// it is sent and holds it until its body has been read, or until its stream is dropped, so no
    /// more than that many requests are in flight at once. A request waits for a permit for at most
    /// the settings' `connection_acquire_timeout_millis`, and never past its operation's deadline.
    ///
    /// The requests in flight are counted whether or not the settings limit them, and the limit is
    /// reported beside them, so how close a client runs to its limit can be watched.
    pub fn new(settings: &HttpClientSettings) -> Result<Self, TransportError> {
        Self::labelled(settings, &[])
    }

    /// [`Self::new`], for the client that sends a store's requests to `service`.
    ///
    /// Its requests in flight, its limit and the time its requests wait for a connection are
    /// labelled with `service`, and with `store` when one is named, so each can be traced to the
    /// store whose connections they count.
    pub(crate) fn for_service(
        settings: &HttpClientSettings,
        service: &'static str,
        store: Option<StoreKind>,
    ) -> Result<Self, TransportError> {
        match store {
            Some(store) => Self::labelled(
                settings,
                &[
                    KeyValue::new(STORE_LABEL_KEY, store.as_str()),
                    KeyValue::new(SERVICE_LABEL_KEY, service),
                ],
            ),
            None => Self::labelled(settings, &[KeyValue::new(SERVICE_LABEL_KEY, service)]),
        }
    }

    /// [`Self::new`], with what it records labelled `labels`.
    fn labelled(
        settings: &HttpClientSettings,
        labels: &[KeyValue],
    ) -> Result<Self, TransportError> {
        let connections = Connections::new(
            labels.to_vec(),
            settings.max_connections,
            settings
                .connection_acquire_timeout_millis
                .map(Duration::from_millis),
        )?;

        let inner = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(settings.pool_idle_timeout_seconds))
            .tcp_nodelay(settings.nodelay)
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(HeaderValue::from_static(USER_AGENT))
            .build()
            .map_err(|error| TransportError::Build(error.to_string()))?;

        if let Some(max) = settings.max_connections {
            report_max_connections(max, labels.to_vec());
        }

        Ok(Self {
            inner,
            body_cap: MAX_BUFFERED_BODY,
            connections: Some(connections),
        })
    }

    /// Build a client for the link-local credential endpoints: the container agent and IMDS.
    ///
    /// Separate from the service client so a metadata endpoint's connection behaviour — a nearby
    /// address, connections that idle out after seconds — cannot be configured by, or interfere
    /// with, the store's settings.
    ///
    /// No proxy is used, whatever the environment configures: these requests carry a
    /// credential-bearing token to an address this process vouched for, and the credentials come
    /// back in the answer, so nothing may stand between the two. Redirects are not followed for the
    /// same reason: a followed redirect would carry the token somewhere the address check never
    /// saw.
    pub fn for_metadata() -> Result<Self, TransportError> {
        Self::for_credentials(reqwest::Client::builder().no_proxy())
    }

    /// Build a client for STS, which the web-identity provider exchanges its identity token with.
    ///
    /// STS is a regional endpoint rather than a link-local one, so unlike [`Self::for_metadata`]
    /// this client uses the proxy the environment configures. Redirects are not followed: the
    /// request carries the identity token, and a followed redirect would deliver it to a host this
    /// process never chose.
    pub fn for_sts() -> Result<Self, TransportError> {
        Self::for_credentials(reqwest::Client::builder())
    }

    /// The settings both credential clients share: a short idle pool, HTTP/1.1, no redirects, and
    /// no limit on connections.
    fn for_credentials(builder: reqwest::ClientBuilder) -> Result<Self, TransportError> {
        let inner = builder
            .pool_idle_timeout(Duration::from_secs(5))
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(HeaderValue::from_static(USER_AGENT))
            .build()
            .map_err(|error| TransportError::Build(error.to_string()))?;

        Ok(Self {
            inner,
            body_cap: MAX_BUFFERED_BODY,
            connections: None,
        })
    }

    /// This client with its body cap lowered to `cap`, so a test reaches the capped path without
    /// moving 64 MiB.
    #[cfg(feature = "test-util")]
    pub fn with_body_cap(self, cap: u64) -> Self {
        Self {
            body_cap: cap,
            ..self
        }
    }

    /// Send `request`, reading the whole body, as an operation of its own: [`Self::send_by`] with
    /// `timeout` bounding the whole of it.
    #[inline]
    pub fn send(
        &self,
        request: HttpRequest,
        timeout: Duration,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> {
        self.send_by(request, timeout, deadline_after(timeout))
    }

    /// Send `request`, reading the whole body, as one attempt of an operation whose budget runs out
    /// at `deadline`.
    ///
    /// When the client limits connections, the request waits for a permit until `deadline`, or for
    /// the client's acquire timeout when that ends sooner, and holds it until the body has been
    /// read. The exchange then has `timeout` or what is left until `deadline`, whichever is less.
    /// Without a limit, `timeout` bounds the exchange. A request that finds no connection released
    /// in time fails as [`TransportError::ConnectionLimit`]. One whose wait cut `timeout` short —
    /// the whole of it fit before `deadline` when the wait began — and that is then not answered by
    /// `deadline` fails as [`TransportError::ConnectionLimitTimeout`].
    ///
    /// The exchange is spawned on the net runtime when this is called, and the future returned only
    /// waits for it, so it holds the task's handle rather than the request. The task with a limit
    /// is a different future from the one without, which neither waits nor holds a permit.
    ///
    /// Dropping the future returned aborts the task. A request still waiting for a connection is
    /// never sent, and releases its place. One already sent is cut off with its connection, so a
    /// write its caller gave up on may or may not have been applied: the outcome a dropped
    /// connection gives, which every caller already allows for, and which the caller that dropped
    /// it never learns either way.
    pub fn send_by(
        &self,
        request: HttpRequest,
        timeout: Duration,
        deadline: Instant,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> {
        let cap = self.body_cap.min(request.response_cap);
        let built = Self::build(request, Some(timeout));
        let client = self.inner.clone();

        // Awaited from a net task so hyper's connection driver is spawned there. Reading the body
        // there rather than in the caller keeps the whole exchange on one runtime.
        let dispatched = AbortOnDropHandle::new(match &self.connections {
            Some(connections) if connections.is_limited() => lore_spawn_net!(
                exchange_within_limit(connections.clone(), deadline, client, built, cap,)
            ),
            connections => lore_spawn_net!(exchange(
                client,
                built,
                cap,
                connections.clone().map(ConnectionPermit::new),
            )),
        });

        async move {
            match dispatched.await {
                Ok(Ok((_, _, None))) => Err(body_over_cap(cap)),
                Ok(Ok((status, headers, Some(body)))) => Ok(HttpResponse {
                    status,
                    headers,
                    body,
                }),
                Ok(Err(error)) => Err(error),
                // Only a panic in the dispatch task, or the net runtime shutting down: the task is
                // aborted only once nothing awaits it.
                Err(join_error) => Err(TransportError::Io(join_error.to_string())),
            }
        }
    }

    /// Send `request`, leaving the body to be read by the caller.
    ///
    /// `timeout` bounds the wait for the response's headers, cut to what is left until `deadline`
    /// when the request first waited for a connection. `deadline`, the operation's own, bounds
    /// every read of the body after that, so a body arriving slowly but steadily has what is left of
    /// the whole budget rather than the attempt's share of it.
    ///
    /// Spawned when called, limited, counted and aborted when the future returned is dropped, as
    /// [`Self::send_by`] is, except that the stream holds its permit, and its place in the count of
    /// requests in flight, until it is dropped. A request whose wait cut `timeout` short, and whose
    /// headers then do not arrive by `deadline`, fails as
    /// [`TransportError::ConnectionLimitTimeout`].
    pub fn send_streaming(
        &self,
        request: HttpRequest,
        timeout: Duration,
        deadline: Instant,
    ) -> impl Future<Output = Result<HttpStream, TransportError>> {
        let cap = self.body_cap.min(request.response_cap);
        let built = Self::build(request, None);
        let client = self.inner.clone();

        let dispatched =
            AbortOnDropHandle::new(match &self.connections {
                Some(connections) if connections.is_limited() => lore_spawn_net!(
                    open_within_limit(connections.clone(), deadline, timeout, client, built,)
                ),
                connections => lore_spawn_net!(open(
                    client,
                    built,
                    timeout,
                    connections.clone().map(ConnectionPermit::new),
                )),
            });

        async move {
            match dispatched.await {
                Ok(Ok((response, connection))) => Ok(HttpStream {
                    response,
                    remaining: cap,
                    cap,
                    deadline,
                    _connection: connection,
                }),
                Ok(Err(error)) => Err(error),
                // Only a panic in the dispatch task, or the net runtime shutting down: the task is
                // aborted only once nothing awaits it.
                Err(join_error) => Err(TransportError::Io(join_error.to_string())),
            }
        }
    }

    /// Turn our request into reqwest's, moving each part rather than copying it.
    ///
    /// Not `RequestBuilder`: its `headers()` inserts the entries one at a time into a second map it
    /// allocates itself. Assigning through `headers_mut` hands over the map already built, and
    /// `Request::new` starts with an empty one, so reqwest ends up owning exactly what was passed.
    ///
    /// `timeout` is reqwest's, which runs until the body has been read to its end.
    fn build(request: HttpRequest, timeout: Option<Duration>) -> reqwest::Request {
        let mut built = reqwest::Request::new(request.method, request.url);

        *built.headers_mut() = request.headers;
        *built.body_mut() = Some(reqwest::Body::from(request.body));
        *built.timeout_mut() = timeout;

        built
    }
}

/// The name, under [`HttpInstruments`], of the histogram of how long requests wait for a
/// connection.
const CONNECTION_WAIT_METRIC_NAME: &str = "connection_wait";

/// The name, under [`LoreHttpInstruments`], of the count of requests a client has in flight.
const REQUESTS_IN_FLIGHT_METRIC_NAME: &str = "requests_in_flight";

/// The name, under [`LoreHttpInstruments`], of the gauge of a client's `max_connections`.
const MAX_CONNECTIONS_METRIC_NAME: &str = "max_connections";

/// The label on a client's measurements that names the service it sends to.
const SERVICE_LABEL_KEY: &str = "service";

/// The label on a client's measurements that names the store it serves.
const STORE_LABEL_KEY: &str = "store";

/// The HTTP transport's instruments whose names dashboards and alerts already read, under
/// `urc.aws.http`.
struct HttpInstruments;

impl InstrumentProvider for HttpInstruments {
    fn namespace(&self) -> &'static str {
        "urc.aws.http"
    }
}

/// The HTTP transport's other instruments, under `lore.aws.http`.
struct LoreHttpInstruments;

impl InstrumentProvider for LoreHttpInstruments {
    fn namespace(&self) -> &'static str {
        "lore.aws.http"
    }
}

/// The connections one service client has in use, and the limit on them when its settings set one.
///
/// The client speaks HTTP/1.1, so each request in flight holds a connection of its own: a request
/// counts as in flight from when it is sent until its body has been read or its stream is dropped.
struct Connections {
    in_flight: UpDownCounter<i64>,
    /// The labels the count, and every wait for a connection, are recorded with.
    labels: Vec<KeyValue>,
    limit: Option<ConnectionLimit>,
}

/// The most connections a client may have in use at once, and how long its requests wait for one.
struct ConnectionLimit {
    permits: Semaphore,
    /// The longest a request waits for a permit, when it is not its operation's deadline.
    acquire_timeout: Option<Duration>,
    /// How long each request waited for a connection, in milliseconds, whether or not it got one.
    wait: Histogram<f64>,
}

impl fmt::Debug for Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("Connections");
        debug.field("labels", &self.labels);
        if let Some(limit) = &self.limit {
            debug
                .field("available", &limit.permits.available_permits())
                .field("acquire_timeout", &limit.acquire_timeout);
        }
        debug.finish_non_exhaustive()
    }
}

impl Connections {
    /// The connections of a client whose measurements carry `labels`, limited to `max` when one is
    /// given, each waited for no longer than `acquire_timeout` when one is given.
    ///
    /// A semaphore holds at most [`Semaphore::MAX_PERMITS`] permits, so the client is not built
    /// with a limit past it.
    fn new(
        labels: Vec<KeyValue>,
        max: Option<NonZeroUsize>,
        acquire_timeout: Option<Duration>,
    ) -> Result<Arc<Self>, TransportError> {
        let limit = match max {
            Some(max) if max.get() > Semaphore::MAX_PERMITS => {
                return Err(TransportError::Build(format!(
                    "max_connections must be at most {}, not {max}",
                    Semaphore::MAX_PERMITS
                )));
            }
            Some(max) => Some(ConnectionLimit {
                permits: Semaphore::new(max.get()),
                acquire_timeout,
                wait: HttpInstruments.latency_histogram_ms(CONNECTION_WAIT_METRIC_NAME),
            }),
            None => None,
        };

        Ok(Arc::new(Self {
            in_flight: LoreHttpInstruments
                .meter()
                .i64_up_down_counter(LoreHttpInstruments.scope_name(REQUESTS_IN_FLIGHT_METRIC_NAME))
                .build(),
            labels,
            limit,
        }))
    }

    /// Whether requests wait for a permit before they are sent.
    fn is_limited(&self) -> bool {
        self.limit.is_some()
    }

    /// A connection, with how long the request waited for it, or `None` when none was released
    /// within the acquire timeout or by `deadline`, whichever comes first. Without a limit, a
    /// connection at once.
    ///
    /// The time waited is recorded either way, as none when a connection was free. The semaphore
    /// is never closed, so the wait ends with a permit or when its time runs out.
    async fn connection_by(
        self: Arc<Self>,
        deadline: Instant,
    ) -> Option<(ConnectionPermit, Duration)> {
        let Some(limit) = &self.limit else {
            return Some((ConnectionPermit::new(self), Duration::ZERO));
        };

        // A released permit goes to the longest-queued request before it is free to take, so a
        // free one passes no request that queued.
        let free = match limit.permits.try_acquire() {
            Ok(permit) => {
                permit.forget();
                true
            }
            Err(_) => false,
        };
        if free {
            limit.wait.record(0.0, &self.labels);

            return Some((ConnectionPermit::new(self), Duration::ZERO));
        }

        let waiting = Instant::now();
        let held = match tokio::time::timeout_at(
            limit
                .acquire_timeout
                .and_then(|timeout| waiting.checked_add(timeout))
                .map_or(deadline, |end| deadline.min(end)),
            limit.permits.acquire(),
        )
        .await
        {
            Ok(Ok(permit)) => {
                permit.forget();
                true
            }
            Ok(Err(_)) | Err(_) => false,
        };
        let waited = waiting.elapsed();
        limit
            .wait
            .record(waited.as_secs_f64() * 1000.0, &self.labels);

        // Built only once a permit is held: dropping one releases a permit.
        if held {
            Some((ConnectionPermit::new(self), waited))
        } else {
            None
        }
    }
}

/// Report a client's limit of `max` connections as a gauge under `labels`, the labels its requests in
/// flight are counted with, so those can be read as a share of it.
///
/// The gauge lasts as long as the process, so it is reported only for a client that was built.
fn report_max_connections(max: NonZeroUsize, labels: Vec<KeyValue>) {
    let reported = max.get() as u64;
    LoreHttpInstruments
        .meter()
        .u64_observable_gauge(LoreHttpInstruments.scope_name(MAX_CONNECTIONS_METRIC_NAME))
        .with_callback(move |observer| observer.observe(reported, &labels))
        .build();
}

/// A connection in use, counted as in flight while held, and released to the client's limit when
/// dropped, when the client has one.
///
/// One pointer, where a semaphore's own owned permit is two: the connections it returns to also
/// hold the semaphore, and a permit is always one connection.
struct ConnectionPermit(Arc<Connections>);

impl ConnectionPermit {
    /// The permit for a request about to be sent, whose connection is already taken from the
    /// limit's semaphore when there is one.
    fn new(connections: Arc<Connections>) -> Self {
        connections.in_flight.add(1, &connections.labels);

        Self(connections)
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.in_flight.add(-1, &self.0.labels);
        if let Some(limit) = &self.0.limit {
            limit.permits.add_permits(1);
        }
    }
}

/// What an exchange hands back: the status, the headers, and the body, which is `None` when it is
/// past the cap.
type Exchanged = Result<(StatusCode, HeaderMap, Option<Bytes>), TransportError>;

/// What opening a stream hands back: the response with its body unread, and the permit for its
/// connection for a service client.
type Opened = Result<(reqwest::Response, Option<ConnectionPermit>), TransportError>;

/// Send `request` and read its body, giving up past `cap`. The request's own timeout bounds both.
/// `connection`, the request's permit, is held until the body has been read or the exchange is
/// dropped.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn exchange(
    client: reqwest::Client,
    request: reqwest::Request,
    cap: u64,
    connection: Option<ConnectionPermit>,
) -> impl Future<Output = Exchanged> {
    async move {
        let mut response = client
            .execute(request)
            .await
            .map_err(|error| classify(&error))?;
        let status = response.status();
        // Taken rather than cloned: nothing reads them through the response afterwards, and a
        // clone is an allocation on every response.
        let headers = std::mem::take(response.headers_mut());
        let body = read_capped(&mut response, cap)
            .await
            .map_err(|error| classify(&error))?;

        drop(connection);

        Ok((status, headers, body))
    }
}

/// [`exchange`], once a connection from `connections` is held, which it keeps until the body has
/// been read.
///
/// The wait for the connection lasts until `deadline`, the operation's, unless the limit's acquire
/// timeout ends it sooner, and the request's own timeout is then cut to what is left until it. A
/// request whose wait cut its timeout short, as [`shortened_by_wait`] decides, fails as
/// [`TransportError::ConnectionLimitTimeout`] when it runs out of what remained.
///
/// It repeats the steps of [`exchange`] rather than awaiting it, which would hold the request
/// twice, in this future and in that one. Not an `async fn`, which would hold a second copy of its
/// arguments.
#[allow(clippy::manual_async_fn)]
fn exchange_within_limit(
    connections: Arc<Connections>,
    deadline: Instant,
    client: reqwest::Client,
    mut request: reqwest::Request,
    cap: u64,
) -> impl Future<Output = Exchanged> {
    async move {
        // Decided inside the arm, so the wait's figures are not kept across the exchange.
        let (_connection, shortened) = match connections.connection_by(deadline).await {
            Some((connection, waited)) => {
                let left = deadline.saturating_duration_since(Instant::now());
                let shortened = match request.timeout_mut() {
                    Some(timeout) => {
                        let shortened = shortened_by_wait(*timeout, waited, left);
                        *timeout = (*timeout).min(left);
                        shortened
                    }
                    None => false,
                };
                (connection, shortened)
            }
            None => return Err(TransportError::ConnectionLimit),
        };

        let mut response = client
            .execute(request)
            .await
            .map_err(|error| classify_after_wait(&error, shortened))?;
        let status = response.status();
        let headers = std::mem::take(response.headers_mut());
        let body = read_capped(&mut response, cap)
            .await
            .map_err(|error| classify_after_wait(&error, shortened))?;

        Ok((status, headers, body))
    }
}

/// Send `request` and wait no longer than `timeout` for the response's headers, handing
/// `connection`, its permit, back with the response for the stream to hold.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn open(
    client: reqwest::Client,
    request: reqwest::Request,
    timeout: Duration,
    connection: Option<ConnectionPermit>,
) -> impl Future<Output = Opened> {
    async move {
        match tokio::time::timeout(timeout, client.execute(request)).await {
            Ok(Ok(response)) => Ok((response, connection)),
            Ok(Err(error)) => Err(classify(&error)),
            Err(_elapsed) => Err(no_response_in_time()),
        }
    }
}

/// [`open`], once a connection from `connections` is held, which it hands back with the response.
///
/// The wait for the connection lasts until `deadline`, the operation's, unless the limit's acquire
/// timeout ends it sooner, and the wait for the headers then has `timeout` or what is left until
/// `deadline`, whichever is less. A request whose wait cut `timeout` short, as
/// [`shortened_by_wait`] decides, fails as [`TransportError::ConnectionLimitTimeout`] when its
/// headers do not arrive in what remained.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn open_within_limit(
    connections: Arc<Connections>,
    deadline: Instant,
    timeout: Duration,
    client: reqwest::Client,
    request: reqwest::Request,
) -> impl Future<Output = Opened> {
    async move {
        // Decided inside the arm, so the wait's figures are not kept across the exchange.
        let (connection, shortened, response) = match connections.connection_by(deadline).await {
            Some((connection, waited)) => {
                let left = deadline.saturating_duration_since(Instant::now());
                (
                    connection,
                    shortened_by_wait(timeout, waited, left),
                    tokio::time::timeout(timeout.min(left), client.execute(request)),
                )
            }
            None => return Err(TransportError::ConnectionLimit),
        };

        match response.await {
            Ok(Ok(response)) => Ok((response, Some(connection))),
            Ok(Err(error)) => Err(classify(&error)),
            Err(_elapsed) if shortened => Err(TransportError::ConnectionLimitTimeout),
            Err(_elapsed) => Err(no_response_in_time()),
        }
    }
}

/// Read a body to its end, giving up past `cap`. `None` means the cap was passed.
///
/// A declared length past the cap is refused before anything is read. `Content-Length` alone does
/// not bound the read — a chunked reply declares none, which is the case the cap exists for — so
/// the cap is also enforced on each read as it arrives.
///
/// A body that arrives in one read is handed over as it stands, without a copy. A longer one is
/// gathered into a buffer sized from its declared length when it has one, so a declared body is
/// copied once into a buffer that never regrows.
async fn read_capped(
    response: &mut reqwest::Response,
    cap: u64,
) -> Result<Option<Bytes>, reqwest::Error> {
    let declared = response.content_length();
    if declared.is_some_and(|length| length > cap) {
        return Ok(None);
    }

    let over_cap = |held: usize, adding: usize| (held as u64 + adding as u64) > cap;

    let Some(first) = response.chunk().await? else {
        return Ok(Some(Bytes::new()));
    };
    if over_cap(0, first.len()) {
        return Ok(None);
    }

    let Some(second) = response.chunk().await? else {
        return Ok(Some(first));
    };
    if over_cap(first.len(), second.len()) {
        return Ok(None);
    }

    let gathered = first.len() + second.len();
    let capacity = declared
        .and_then(|length| usize::try_from(length).ok())
        .map_or(gathered, |length| length.max(gathered));

    let mut body = BytesMut::with_capacity(capacity);
    body.extend_from_slice(&first);
    body.extend_from_slice(&second);

    while let Some(chunk) = response.chunk().await? {
        if over_cap(body.len(), chunk.len()) {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Some(body.freeze()))
}

/// Sort a reqwest failure into what it means for a retry.
///
/// A timeout is `Io` rather than a distinct kind: at this level it is one attempt that did not
/// answer, and whether the operation as a whole is out of time is the retry loop's question.
fn classify(error: &reqwest::Error) -> TransportError {
    if error.is_body() || error.is_decode() {
        TransportError::Body(error.to_string())
    } else if error.is_builder() {
        TransportError::Build(error.to_string())
    } else {
        TransportError::Io(error.to_string())
    }
}

/// Whether waiting `waited` for a connection is what cut a request's `timeout` to the `left` until
/// its deadline: the whole timeout fit when the wait began, and no longer does.
///
/// A request whose timeout the deadline had already cut, as a late resend's is, or that found a
/// connection free, keeps any timeout as the service's.
#[lore_macro::test_pub]
fn shortened_by_wait(timeout: Duration, waited: Duration, left: Duration) -> bool {
    left < timeout && left.saturating_add(waited) > timeout
}

/// [`classify`], for a request whose timeout was `shortened` by its wait for a connection: running
/// out of that time is a [`TransportError::ConnectionLimitTimeout`].
fn classify_after_wait(error: &reqwest::Error, shortened: bool) -> TransportError {
    if shortened && error.is_timeout() {
        TransportError::ConnectionLimitTimeout
    } else {
        classify(error)
    }
}
