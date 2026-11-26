// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A `DynamoDB` and S3 client built on the HTTP stack Lore already ships.
//!
//! It covers the operations Lore issues and nothing more: `SigV4` header signing, the credential
//! chain, and the `DynamoDB` JSON and S3 XML protocols.
//!
//! The layering is deliberately flat. [`error`] defines what a failure is, [`item`] what a row is,
//! [`json`] and [`xml`] turn one into the other, and [`ClientCore`] is the single place a request
//! is signed, sent, classified and retried.
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::RwLock;
use time::UtcDateTime;
use tokio::time::Instant;
use tracing::debug;
use tracing::warn;
use url::Url;

use crate::aws::creds::CachedCredentials;
use crate::aws::creds::Credentials;
use crate::aws::error::AwsCallError;
use crate::aws::error::Retryable;
use crate::aws::error::ServiceError;
use crate::aws::error::ServiceErrorCode;
use crate::aws::error::TransportError;
use crate::aws::http::HttpClient;
use crate::aws::http::HttpRequest;
use crate::aws::http::HttpResponse;
use crate::aws::http::HttpStream;
use crate::aws::retry::ClockSkew;
use crate::aws::retry::RetryBudget;
use crate::aws::retry::deadline_after;
use crate::aws::sign::HexDigest;
use crate::aws::sign::SignScratch;
use crate::aws::sign::SigningContext;
use crate::aws::sign::SigningKeyCache;
use crate::aws::sign::payload_hash;
use crate::aws::sign::sign_request;

pub mod creds;
pub mod dynamodb;
pub mod error;
pub mod http;
pub mod imds;
pub mod item;
pub mod json;
pub mod observe;
pub mod retry;
pub mod s3;
pub mod serde;
pub mod sign;
pub mod sts;
pub mod xml;

/// How a service reports its errors, and where it puts its request id.
///
/// Static dispatch rather than a trait object: there are exactly two implementations, and the
/// difference between them is a JSON body and an XML one.
pub trait ServiceProtocol {
    /// This service's error codes.
    type Code: ServiceErrorCode;

    /// The service's own name in the signature's credential scope.
    const SERVICE: &'static str;

    /// Where the service puts the id that AWS support asks for.
    const REQUEST_ID_HEADER: &'static str;

    /// Whether the service accepts `UNSIGNED-PAYLOAD`, which a body sent over TLS is then signed
    /// as rather than hashed.
    const UNSIGNED_PAYLOAD_OVER_TLS: bool = false;

    /// Read an error out of a response the service did not answer successfully.
    ///
    /// Never fails: a body that cannot be parsed still has a status, and reporting an unknown code
    /// is more useful than reporting a parse failure.
    ///
    /// The request id is read whatever the client was configured to capture on success: it is the
    /// one thing AWS support asks for, and only a failure ever needs it.
    fn parse_error(response: &HttpResponse) -> ServiceError<Self::Code>;
}

thread_local! {
    /// The buffers a signature is built in, one set per worker thread.
    ///
    /// The buffers outlive the request, so signing reuses their capacity rather than allocating its
    /// intermediate strings each time, and a thread-local gives that without a lock every worker
    /// would contend on.
    ///
    /// Sound in an async context because [`ClientCore::dispatch`] borrows it across the synchronous
    /// `sign_request` call and nothing else: a task moves between worker threads at an `.await` and
    /// nowhere else, and a `RefMut` is `!Send`, so a borrow held across one fails to compile.
    static SIGN_SCRATCH: std::cell::RefCell<SignScratch> =
        const { std::cell::RefCell::new(SignScratch::new()) };
}

/// Everything a request needs beyond its own path and body.
pub struct ClientCore {
    http: HttpClient,
    credentials: Arc<CachedCredentials>,
    signing_keys: SigningKeyCache,
    skew: ClockSkew,
    region: Arc<str>,
    /// The service endpoint. Its path, when it has one, prefixes every request's.
    endpoint: Url,
    /// The whole-operation budget, retries included.
    timeout: Duration,
    /// Whether to keep a request id off a *successful* response.
    ///
    /// Only the slow-operation warning reads it, and `lore-server` leaves that threshold at
    /// `u64::MAX` unless configured, so capturing one per response would allocate for nothing.
    capture_request_id: bool,
}

impl std::fmt::Debug for ClientCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCore")
            .field("region", &self.region)
            .field("endpoint", &self.endpoint.as_str())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl ClientCore {
    pub fn new(
        http: HttpClient,
        credentials: Arc<CachedCredentials>,
        region: Arc<str>,
        endpoint: Url,
        timeout: Duration,
        capture_request_id: bool,
    ) -> Self {
        Self {
            http,
            credentials,
            signing_keys: SigningKeyCache::new(),
            skew: ClockSkew::new(),
            region,
            endpoint,
            timeout,
            capture_request_id,
        }
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub fn capture_request_id(&self) -> bool {
        self.capture_request_id
    }

    /// When an operation starting now runs out of its budget.
    pub fn deadline(&self) -> Instant {
        deadline_after(self.timeout)
    }

    /// Send the request `build` produces, retrying what is worth retrying inside one operation
    /// budget.
    ///
    /// `build` is called once per attempt: the transport takes ownership of the URL and header map,
    /// and a signature covers its own timestamp, so each attempt needs a request of its own. An
    /// operation that succeeds first time builds one. `build` must produce the same body each time,
    /// which is what lets the payload be hashed once.
    pub fn send<P: ServiceProtocol>(
        &self,
        build: impl FnMut() -> HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, AwsCallError<P::Code>>> {
        self.dispatch::<P, Buffered, _>(build)
    }

    /// [`Self::send`], leaving the response body for the caller to stream.
    pub fn send_streaming<P: ServiceProtocol>(
        &self,
        build: impl FnMut() -> HttpRequest,
    ) -> impl Future<Output = Result<HttpStream, AwsCallError<P::Code>>> {
        self.dispatch::<P, Streaming, _>(build)
    }

    /// The attempt loop, shared by the buffered and streaming paths.
    ///
    /// An error response always has its body in hand — the streaming path reads it before
    /// classifying — so both paths classify identically.
    ///
    /// Only a request that never got an answer is resent here. A throttle or a 5xx goes back to the
    /// caller, whose own backoff paces congestion and who knows whether the work is still wanted.
    /// Re-signing for a corrected clock or for refreshed credentials is different: each changes the
    /// request rather than waiting on the service, and each is taken at most once per operation.
    ///
    /// A resend after a transport failure can repeat a request the service already applied, because
    /// the failure says nothing about whether it was. Reads, and transactions, which `DynamoDB`
    /// deduplicates by their request token, repeat without changing the outcome. A write can be
    /// applied twice, and a conditional `PutItem` applied before the failure reports its own write
    /// as a failed condition when resent.
    ///
    /// A request the client's connection limit held back ends the operation, which goes back to
    /// the caller as shed for its own back-off to pace. It is reported as [`operation_failure`]
    /// describes. So does one the limit kept
    /// queued until too little of the budget was left and that then ran out of it, reported as
    /// [`TransportError::ConnectionLimitTimeout`].
    async fn dispatch<P, D, B>(&self, mut build: B) -> Result<D::Output, AwsCallError<P::Code>>
    where
        P: ServiceProtocol,
        D: Dispatch,
        B: FnMut() -> HttpRequest,
    {
        let mut budget = RetryBudget::new(self.timeout);
        let mut skew_corrected = false;
        let mut credentials_refreshed = false;

        // Credentials before the first build, so no request is held across an await and the
        // future does not carry one.
        let mut credentials = self.credentials_within::<P::Code>(&budget).await?;
        let mut request = build();

        let mut digest: HexDigest = [0u8; 64];
        let payload_hash = payload_hash(
            P::UNSIGNED_PAYLOAD_OVER_TLS,
            &request.url,
            &request.body,
            &mut digest,
        );

        loop {
            let Some(timeout) = budget.attempt_timeout() else {
                return Err(AwsCallError::Timeout);
            };

            // Destructured so the signer can hold the headers mutably and the URL immutably.
            let HttpRequest {
                method,
                url,
                mut headers,
                body,
                response_cap,
            } = request;

            let (timestamp, signed_offset) = self.skew.now_with_offset();
            let context = SigningContext {
                access_key_id: &credentials.access_key_id,
                secret_access_key: &credentials.secret_access_key,
                session_token: credentials.session_token.as_deref(),
                region: &self.region,
                service: P::SERVICE,
                timestamp,
            };

            SIGN_SCRATCH
                .with_borrow_mut(|scratch| {
                    sign_request(
                        method.as_str(),
                        &url,
                        &mut headers,
                        payload_hash,
                        &context,
                        &self.signing_keys,
                        scratch,
                    )
                })
                .map_err(|error| {
                    AwsCallError::Transport(TransportError::Build(error.to_string()))
                })?;

            let attempt = HttpRequest {
                method,
                url,
                headers,
                body,
                response_cap,
            };

            let failure = match D::send(&self.http, attempt, timeout, budget.deadline()).await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let ErrorWithDate { error, server_date } =
                        response.into_service_error::<P>().await;

                    // A rejected signature is often a drifting clock rather than a wrong secret.
                    // The service told us its own time in the response; adopt it, and sign once
                    // more if the correction now in force differs from the one this attempt was
                    // signed under, whether this rejection or a concurrent one put it there.
                    if error.code.retryable() == Retryable::ClockSkew {
                        self.skew.learn(&server_date, UtcDateTime::now());

                        if skew_corrected || self.skew.offset_seconds() == signed_offset {
                            return Err(AwsCallError::Service(Box::new(error)));
                        }

                        skew_corrected = true;
                        warn!(
                            offset_seconds = self.skew.offset_seconds(),
                            code = error.code.as_str(),
                            "This host's clock disagreed with AWS; re-signing against the \
                             service's time"
                        );
                        None
                    } else if error.code.is_expired_credentials() {
                        // The service says these credentials are expired while the cache still
                        // believed them good. Re-sending is pointless, but signing again with
                        // freshly fetched ones is not — so drop them and take one more attempt,
                        // exactly once, the same shape as the clock correction above.
                        if credentials_refreshed {
                            return Err(AwsCallError::Service(Box::new(error)));
                        }

                        credentials_refreshed = true;
                        self.credentials.invalidate(&credentials);
                        warn!(
                            code = error.code.as_str(),
                            "AWS rejected the cached credentials as expired; refreshing and \
                             signing again"
                        );
                        None
                    } else {
                        Some(AwsCallError::Service(Box::new(error)))
                    }
                }
                Err(transport) => Some(AwsCallError::Transport(transport)),
            };

            // `None` re-signs at once; a failure is resent only after the back-off, and only when
            // the request never got an answer.
            if let Some(failure) = failure {
                let resend = matches!(
                    &failure,
                    AwsCallError::Transport(transport) if transport.is_worth_resending()
                );

                if !resend {
                    return Err(operation_failure(failure, budget.attempts()));
                }

                if !budget.wait().await {
                    debug!(
                        attempts = budget.attempts(),
                        "Giving up on an AWS operation: {failure}"
                    );

                    return Err(if budget.is_spent() {
                        AwsCallError::Timeout
                    } else {
                        failure
                    });
                }
            }

            credentials = self.credentials_within::<P::Code>(&budget).await?;
            request = build();
        }
    }

    /// Credentials to sign with, waiting on a refresh no longer than what is left of `budget`.
    ///
    /// Freshness is judged against the service's clock, not this host's. Signing uses the corrected
    /// time, and the two have to agree: on a skewed host the raw clock can call credentials fresh
    /// that the service will reject as expired.
    ///
    /// Credentials that need no refresh are served without arming a timer.
    async fn credentials_within<C>(
        &self,
        budget: &RetryBudget,
    ) -> Result<Arc<Credentials>, AwsCallError<C>> {
        let now = self.skew.now();
        if let Some(credentials) = self.credentials.fresh_enough(now) {
            return Ok(credentials);
        }

        let Some(remaining) = budget.remaining() else {
            return Err(AwsCallError::Timeout);
        };

        match tokio::time::timeout(remaining, self.credentials.get_at(now)).await {
            Ok(Ok(credentials)) => Ok(credentials),
            Ok(Err(error)) => Err(AwsCallError::Credentials {
                transient: error.is_transient(),
                detail: error.to_string(),
            }),
            Err(_elapsed) => Err(AwsCallError::Timeout),
        }
    }

    /// A URL under this client's endpoint: `path` follows the endpoint's own path, when it has one.
    pub fn url(&self, path: &str, query: Option<&str>) -> Url {
        let mut url = self.endpoint.clone();

        match self.endpoint.path().trim_end_matches('/') {
            "" => url.set_path(path),
            prefix => url.set_path(&format!("{prefix}{path}")),
        }
        url.set_query(query);

        url
    }
}

/// What an operation that will not be resent fails with, after `attempts` attempts.
///
/// A request the connection limit held back was never sent. When an earlier attempt of the same
/// operation was, that one may have been applied, so the operation is reported as a timeout rather
/// than as never sent.
#[lore_macro::test_pub]
fn operation_failure<C>(failure: AwsCallError<C>, attempts: u32) -> AwsCallError<C> {
    match failure {
        AwsCallError::Transport(TransportError::ConnectionLimit) if attempts > 1 => {
            AwsCallError::Timeout
        }
        failure => failure,
    }
}

/// Which transport call an operation makes: read the body, or leave it streaming.
///
/// A marker rather than a closure, because a closure returning a future that borrows its argument
/// cannot name the lifetime that relates them.
pub trait Dispatch {
    type Output: HasStatus;

    /// Send one attempt of an operation whose budget runs out at `deadline`. `timeout` bounds its
    /// exchange, and with a connection limit its wait for a connection lasts until `deadline`, or
    /// until the limit's acquire timeout when that ends sooner.
    fn send(
        client: &HttpClient,
        request: HttpRequest,
        timeout: Duration,
        deadline: Instant,
    ) -> impl Future<Output = Result<Self::Output, TransportError>>;
}

/// Read the whole body within the attempt: every operation but `GetObject`.
pub struct Buffered;

impl Dispatch for Buffered {
    type Output = HttpResponse;

    fn send(
        client: &HttpClient,
        request: HttpRequest,
        timeout: Duration,
        deadline: Instant,
    ) -> impl Future<Output = Result<Self::Output, TransportError>> {
        client.send_by(request, timeout, deadline)
    }
}

/// Leave the body for the caller to read by the operation's deadline: `GetObject`, whose body is a
/// fragment payload.
pub struct Streaming;

impl Dispatch for Streaming {
    type Output = HttpStream;

    fn send(
        client: &HttpClient,
        request: HttpRequest,
        timeout: Duration,
        deadline: Instant,
    ) -> impl Future<Output = Result<Self::Output, TransportError>> {
        client.send_streaming(request, timeout, deadline)
    }
}

/// What the attempt loop needs of a response, whichever shape it has.
///
/// An error response's body is read here rather than left to the caller, so the streaming path
/// classifies from the same information as the buffered one.
pub trait HasStatus: Sized {
    fn status(&self) -> ::http::StatusCode;

    /// Consume a failed response into its error.
    fn into_service_error<P: ServiceProtocol>(self)
    -> impl Future<Output = ErrorWithDate<P::Code>>;
}

/// A service error plus the response's `Date`, which clock-skew correction needs.
pub struct ErrorWithDate<E> {
    error: ServiceError<E>,
    server_date: String,
}

impl HasStatus for HttpResponse {
    fn status(&self) -> ::http::StatusCode {
        self.status
    }

    async fn into_service_error<P: ServiceProtocol>(self) -> ErrorWithDate<P::Code> {
        let error = P::parse_error(&self);
        let server_date = server_date_for(&error, || self.header("date"));

        ErrorWithDate { error, server_date }
    }
}

/// The service's own clock, copied only for the errors that read it.
///
/// Clock-skew correction is the only reader, and throttling is the error this client sees most.
fn server_date_for<'a, E: ServiceErrorCode>(
    error: &ServiceError<E>,
    date: impl FnOnce() -> Option<&'a str>,
) -> String {
    if error.code.retryable() == Retryable::ClockSkew {
        date().unwrap_or_default().to_owned()
    } else {
        String::new()
    }
}

impl HasStatus for HttpStream {
    fn status(&self) -> ::http::StatusCode {
        HttpStream::status(self)
    }

    async fn into_service_error<P: ServiceProtocol>(mut self) -> ErrorWithDate<P::Code> {
        let status = HttpStream::status(&self);
        let headers = self.take_headers();

        // An error body is small and always worth reading; a failure to read it leaves the status,
        // which is what a bodyless HEAD error is anyway.
        let body = self.bytes().await.unwrap_or_default();

        let response = HttpResponse {
            status,
            headers,
            body,
        };
        let error = P::parse_error(&response);
        let server_date = server_date_for(&error, || response.header("date"));

        ErrorWithDate { error, server_date }
    }
}

/// The region `explicit`, the configured `*_region` setting, or the environment names, in the order
/// the AWS tooling reads them.
///
/// Reads nothing over the network.
pub fn resolve_region(explicit: Option<&str>) -> Option<String> {
    if let Some(region) = explicit.filter(|region| !region.is_empty()) {
        return Some(region.to_owned());
    }

    for variable in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(region) = std::env::var(variable)
            && !region.is_empty()
        {
            return Some(region);
        }
    }

    None
}

/// The region to sign for: [`resolve_region`], then the region of the EC2 instance this process
/// runs on.
///
/// The metadata service is asked until it answers, and its answer, or a host that does not answer
/// at the metadata address, serves every later resolution in the process.
pub async fn resolve_region_with_imds(explicit: Option<&str>) -> Option<String> {
    static INSTANCE_REGION: imds::InstanceRegion = imds::InstanceRegion::new();

    INSTANCE_REGION
        .resolve(explicit, || {
            HttpClient::for_metadata().map(imds::ImdsProvider::new)
        })
        .await
}

/// The default endpoint for a service in a region.
pub fn default_endpoint(service: &str, region: &str) -> String {
    format!("https://{service}.{region}.{}", dns_suffix(region))
}

/// The domain of the partition `region` belongs to: `amazonaws.com.cn` for the China regions,
/// `amazonaws.com` for every other.
fn dns_suffix(region: &str) -> &'static str {
    if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    }
}

/// The endpoint configured for `service`, in the order the AWS tooling resolves it: `explicit`,
/// then `AWS_ENDPOINT_URL_<SERVICE>`, then `AWS_ENDPOINT_URL`. `None` leaves the service's default.
///
/// `service` is the lowercase id the default endpoint is built from, so `dynamodb` reads
/// `AWS_ENDPOINT_URL_DYNAMODB`. `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS=true` skips both variables, as
/// it does for the AWS SDKs; an explicit setting still applies.
pub fn configured_endpoint(explicit: Option<&str>, service: &str) -> Option<String> {
    if let Some(endpoint) = explicit.filter(|endpoint| !endpoint.is_empty()) {
        return Some(endpoint.to_owned());
    }

    if std::env::var("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS")
        .is_ok_and(|ignore| ignore.eq_ignore_ascii_case("true"))
    {
        return None;
    }

    let service_variable = format!("AWS_ENDPOINT_URL_{}", service.to_ascii_uppercase());
    [service_variable.as_str(), "AWS_ENDPOINT_URL"]
        .into_iter()
        .find_map(|variable| {
            std::env::var(variable)
                .ok()
                .filter(|endpoint| !endpoint.is_empty())
        })
}

/// The chains built so far, each under the region it signs for.
type CredentialChains = RwLock<Vec<(Box<str>, Arc<CachedCredentials>)>>;

/// The credential chain this client uses, one per region per process.
///
/// Ordered so a deployment's own identity wins over a developer's leftover file.
///
/// Shared rather than built per client, because every service client in a process resolves the same
/// identity. A cache per client would refresh per client — a server holding an immutable, mutable
/// and lock store builds four clients, so four `AssumeRoleWithWebIdentity` calls per expiry — and
/// an `ExpiredToken` learned from the service by one would not reach the others, leaving them
/// signing with a token both sides know is dead.
///
/// The link-local providers share a client that bypasses any proxy; STS gets one of its own that
/// uses the proxy the environment configures.
///
/// Keyed by region because the STS endpoint is regional, and a deployment may point two stores at
/// two regions. A list rather than a map: there is one region, occasionally two, so a comparison
/// beats a hash. Reached only when no credentials are configured explicitly.
pub fn default_credentials_chain(region: &str) -> Result<Arc<CachedCredentials>, TransportError> {
    static CHAINS: LazyLock<CredentialChains> = LazyLock::new(|| RwLock::new(Vec::new()));

    fn held(
        chains: &[(Box<str>, Arc<CachedCredentials>)],
        region: &str,
    ) -> Option<Arc<CachedCredentials>> {
        chains
            .iter()
            .find(|(held, _)| &**held == region)
            .map(|(_, chain)| chain.clone())
    }

    if let Some(chain) = held(&CHAINS.read(), region) {
        return Ok(chain);
    }

    let metadata_http = HttpClient::for_metadata()?;
    let sts_http = HttpClient::for_sts()?;

    let chain = creds::CredentialsChain::new(vec![
        Box::new(creds::EnvProvider),
        Box::new(imds::ContainerProvider::new(metadata_http.clone())),
        Box::new(sts::WebIdentityProvider::new(sts_http, region.to_owned())),
        Box::new(imds::ImdsProvider::new(metadata_http)),
        Box::new(creds::ProfileProvider),
    ]);

    let mut chains = CHAINS.write();

    // Another builder may have reached here while this one was assembling its chain. Whichever
    // arrives second takes the first one's, so a region ends up with exactly one cache however many
    // clients ask for it.
    if let Some(chain) = held(&chains, region) {
        return Ok(chain);
    }

    let chain = Arc::new(CachedCredentials::new(Arc::new(chain)));
    chains.push((region.into(), chain.clone()));

    Ok(chain)
}
