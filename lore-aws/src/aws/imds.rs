// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Credentials from a metadata endpoint: ECS/EKS Pod Identity, and EC2 `IMDSv2`.
//!
//! Both answer with the same JSON shape, so one parser serves both. IMDS is version 2 only — the
//! token must be fetched with a `PUT` before anything can be read
//! (<https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instance-metadata-v2-how-it-works.html>).
//! That is a security property, not a convenience: a server-side request forgery through a Lore
//! endpoint cannot reach the metadata service with a `GET` alone.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use lore_base::retry::retry;
use tokio::sync::OnceCell;
use url::Url;

use crate::aws::creds::Credentials;
use crate::aws::creds::CredentialsError;
use crate::aws::creds::ProvideCredentials;
use crate::aws::creds::parse_expiry;
use crate::aws::error::TransportError;
use crate::aws::error::shed_by_status;
use crate::aws::http::HttpClient;
use crate::aws::http::HttpRequest;
use crate::aws::resolve_region;

/// The EC2 metadata service.
const IMDS_BASE: &str = "http://169.254.169.254";
/// Where a relative container credentials URI resolves against.
const CONTAINER_BASE: &str = "http://169.254.170.2";

/// The ECS task metadata address, and the EKS Pod Identity agent's, in both families.
const ECS_ADDRESS: std::net::Ipv4Addr = std::net::Ipv4Addr::new(169, 254, 170, 2);
const EKS_POD_IDENTITY_ADDRESS: std::net::Ipv4Addr = std::net::Ipv4Addr::new(169, 254, 170, 23);
const EKS_POD_IDENTITY_ADDRESS_V6: std::net::Ipv6Addr =
    std::net::Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x23);

/// A metadata endpoint is link-local; if it does not answer promptly it is not there.
const METADATA_TIMEOUT: Duration = Duration::from_secs(2);
/// Waits between attempts at a region lookup the metadata service shed or did not answer, and the
/// first and longest wait, in milliseconds: about 100 ms, then 200 ms.
///
/// Two waits give three attempts. An attempt is a token request and a read, each bounded by the
/// metadata timeout, so a service that keeps failing costs a lookup up to about 12 seconds and then
/// gives no region.
const REGION_RETRY_LIMIT: usize = 2;
const REGION_RETRY_START_MILLIS: u64 = 100;
const REGION_RETRY_MAX_MILLIS: u64 = 200;
/// How long an `IMDSv2` token stays valid, in seconds. Long enough to cover the fetch it is taken
/// out for, short enough to be useless if it leaks.
const TOKEN_TTL_HEADER: HeaderValue = HeaderValue::from_static("60");

/// The region a metadata response names, or `None` for a body that is empty or not text.
fn parse_region(body: &[u8]) -> Option<String> {
    std::str::from_utf8(body)
        .ok()
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_owned)
}

/// The JSON both endpoints answer with.
///
/// IMDS adds a `Code` field, which is `Success` on the happy path; the container endpoint omits it.
/// Absent or `Success` both count as success, so one parser serves both.
#[lore_macro::test_pub]
pub(crate) fn parse_metadata_credentials(
    provider: &'static str,
    body: &[u8],
) -> Result<Credentials, CredentialsError> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Response {
        code: Option<String>,
        access_key_id: Option<String>,
        secret_access_key: Option<String>,
        token: Option<String>,
        expiration: Option<String>,
    }

    let response: Response =
        serde_json::from_slice(body).map_err(|error| CredentialsError::Failed {
            provider,
            detail: format!("unreadable credentials response: {error}"),
        })?;

    if let Some(code) = response.code.as_deref()
        && code != "Success"
    {
        return Err(CredentialsError::Failed {
            provider,
            detail: format!("credentials response reported {code}"),
        });
    }

    let (Some(access_key_id), Some(secret_access_key)) =
        (response.access_key_id, response.secret_access_key)
    else {
        return Err(CredentialsError::Failed {
            provider,
            detail: "credentials response carried no key".to_owned(),
        });
    };

    Ok(Credentials::new(
        access_key_id,
        secret_access_key,
        response.token.map(Into::into),
        parse_expiry(provider, response.expiration.as_deref())?,
    ))
}

/// A metadata endpoint that answered with something other than success, as a credentials failure.
///
/// Transient when the status says the request was shed, as a shed STS answer is, so a caller backs
/// off rather than reporting a deployment that cannot sign.
#[lore_macro::test_pub]
fn refused(provider: &'static str, status: StatusCode, detail: String) -> CredentialsError {
    if shed_by_status(status) {
        CredentialsError::Transient { provider, detail }
    } else {
        CredentialsError::Failed { provider, detail }
    }
}

/// A metadata endpoint that could not be reached, as a credentials failure: transient, because
/// asking again later can reach it.
fn not_reached(provider: &'static str, error: &TransportError) -> CredentialsError {
    CredentialsError::Transient {
        provider,
        detail: error.to_string(),
    }
}

/// Credentials from the ECS or EKS Pod Identity endpoint.
#[derive(Debug)]
pub struct ContainerProvider {
    http: HttpClient,
}

impl ContainerProvider {
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    /// The endpoint to read, from whichever variable is set.
    ///
    /// `FULL_URI` is used as given; `RELATIVE_URI` resolves against the ECS task-metadata address.
    #[lore_macro::test_pub]
    fn endpoint() -> Option<String> {
        if let Ok(full) = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI")
            && !full.is_empty()
        {
            return Some(full);
        }

        std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
            .ok()
            .filter(|relative| !relative.is_empty())
            .map(|relative| {
                if relative.starts_with('/') {
                    format!("{CONTAINER_BASE}{relative}")
                } else {
                    format!("{CONTAINER_BASE}/{relative}")
                }
            })
    }

    /// Whether a full container credentials URI may be used.
    ///
    /// Plain HTTP is allowed only to loopback and to the two container-agent link-local addresses; any
    /// other host must be HTTPS. This is the same restriction the AWS SDKs apply, and it is load
    /// bearing: this provider sends `AWS_CONTAINER_AUTHORIZATION_TOKEN` to whatever this variable
    /// names, so without the check one environment variable is enough to have the process hand that
    /// token — and with it, the role — to an arbitrary host in cleartext.
    #[lore_macro::test_pub]
    fn is_permitted(url: &Url) -> bool {
        if url.scheme() == "https" {
            return true;
        }

        match url.host() {
            // Not resolved: a name that resolves to loopback today can resolve elsewhere tomorrow, so
            // only the literal spelling is taken at face value.
            Some(url::Host::Domain(name)) => name == "localhost",
            Some(url::Host::Ipv4(address)) => {
                address.is_loopback()
                    || address == ECS_ADDRESS
                    || address == EKS_POD_IDENTITY_ADDRESS
            }
            Some(url::Host::Ipv6(address)) => {
                address.is_loopback() || address == EKS_POD_IDENTITY_ADDRESS_V6
            }
            None => false,
        }
    }

    /// The authorization the endpoint expects, from a file if one is named.
    async fn authorization() -> Option<String> {
        if let Ok(path) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")
            && !path.is_empty()
        {
            return tokio::fs::read_to_string(path)
                .await
                .ok()
                .map(|token| token.trim().to_owned());
        }

        std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
    }

    /// Read credentials from `url`, presenting `token` as the authorization when there is one.
    ///
    /// The token is marked sensitive, so no request's `Debug` output shows it.
    #[lore_macro::test_pub]
    async fn fetch(
        &self,
        url: Url,
        token: Option<String>,
    ) -> Result<Credentials, CredentialsError> {
        let mut request = HttpRequest::new(Method::GET, url);
        if let Some(token) = token {
            // The token reaches a header, so a value that cannot be one is a configuration
            // error rather than something to send anyway.
            let Ok(mut value) = HeaderValue::from_str(&token) else {
                return Err(CredentialsError::Failed {
                    provider: "container",
                    detail: "container authorization token is not header-safe".to_owned(),
                });
            };
            value.set_sensitive(true);
            request.headers.insert(http::header::AUTHORIZATION, value);
        }

        let response = self
            .http
            .send(request, METADATA_TIMEOUT)
            .await
            .map_err(|error| not_reached("container", &error))?;

        if !response.status.is_success() {
            return Err(refused(
                "container",
                response.status,
                format!("credentials endpoint answered {}", response.status),
            ));
        }

        parse_metadata_credentials("container", &response.body)
    }
}

#[async_trait]
impl ProvideCredentials for ContainerProvider {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        let endpoint = Self::endpoint().ok_or(CredentialsError::NotProvided("container"))?;
        let url = Url::parse(&endpoint).map_err(|error| CredentialsError::Failed {
            provider: "container",
            detail: format!("container credentials URI is not a URL: {error}"),
        })?;

        if !Self::is_permitted(&url) {
            return Err(CredentialsError::Failed {
                provider: "container",
                detail: format!(
                    "refusing to fetch credentials over plain HTTP from {}: only loopback and the \
                     container agent addresses are permitted without TLS",
                    url.host_str().unwrap_or("<no host>")
                ),
            });
        }

        self.fetch(url, Self::authorization().await).await
    }

    fn name(&self) -> &'static str {
        "container"
    }
}

/// Credentials from an EC2 instance profile, over `IMDSv2`.
#[derive(Debug)]
pub struct ImdsProvider {
    http: HttpClient,
    base: String,
}

impl ImdsProvider {
    pub fn new(http: HttpClient) -> Self {
        Self::at(
            http,
            std::env::var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
                .ok()
                .filter(|endpoint| !endpoint.is_empty())
                .unwrap_or_else(|| IMDS_BASE.to_owned()),
        )
    }

    /// A provider reading the metadata service at `base`: scheme, host and port, without a path.
    #[lore_macro::test_pub]
    fn at(http: HttpClient, base: String) -> Self {
        Self { http, base }
    }

    /// Whether the environment has switched IMDS off. Honoured because a deployment that sets it
    /// means it, and probing anyway wastes two seconds per credential fetch.
    #[lore_macro::test_pub]
    pub(crate) fn disabled() -> bool {
        std::env::var("AWS_EC2_METADATA_DISABLED")
            .is_ok_and(|value| value.eq_ignore_ascii_case("true"))
    }

    /// The `IMDSv2` session token, as the header every metadata read carries it in.
    ///
    /// Built from the response body without copying it, checked for header safety once rather than
    /// on each request it is attached to, and marked sensitive so no request's `Debug` output shows
    /// it.
    ///
    /// No answer at all is this host not being on EC2, so it declines rather than fails: a laptop
    /// would otherwise report a link-local timeout as the reason every credential resolution took
    /// the profile. An endpoint that does answer is a real one: a shed answer is transient, and any
    /// other refusal is a failure.
    #[lore_macro::test_pub]
    async fn token(&self) -> Result<HeaderValue, CredentialsError> {
        let url = self.url("/latest/api/token")?;
        let mut request = HttpRequest::new(Method::PUT, url);
        request
            .headers
            .insert("x-aws-ec2-metadata-token-ttl-seconds", TOKEN_TTL_HEADER);

        let response = self
            .http
            .send(request, METADATA_TIMEOUT)
            .await
            .map_err(|_unreachable| CredentialsError::NotProvided("imds"))?;

        if !response.status.is_success() {
            return Err(refused(
                "imds",
                response.status,
                format!("token request answered {}", response.status),
            ));
        }

        let mut token = HeaderValue::from_maybe_shared(response.body).map_err(|error| {
            CredentialsError::Failed {
                provider: "imds",
                detail: format!("token is not header-safe: {error}"),
            }
        })?;
        token.set_sensitive(true);

        Ok(token)
    }

    /// Read the metadata at `path`, presenting `token`.
    ///
    /// Holding a token means this host is on EC2, so a read that cannot reach the service is
    /// transient rather than a decline.
    #[lore_macro::test_pub]
    async fn get(&self, path: &str, token: &HeaderValue) -> Result<bytes::Bytes, CredentialsError> {
        let url = self.url(path)?;
        let mut request = HttpRequest::new(Method::GET, url);
        request
            .headers
            .insert("x-aws-ec2-metadata-token", token.clone());

        let response = self
            .http
            .send(request, METADATA_TIMEOUT)
            .await
            .map_err(|error| not_reached("imds", &error))?;

        if !response.status.is_success() {
            return Err(refused(
                "imds",
                response.status,
                format!("{path} answered {}", response.status),
            ));
        }

        Ok(response.body)
    }

    fn url(&self, path: &str) -> Result<Url, CredentialsError> {
        Url::parse(&format!("{}{path}", self.base)).map_err(|error| CredentialsError::Failed {
            provider: "imds",
            detail: format!("metadata endpoint is not a URL: {error}"),
        })
    }

    /// The region this instance is in, for region resolution rather than credentials.
    ///
    /// `None` when this host has no metadata service, or the service refuses the request or names
    /// no region: answers that asking again would not change. Once the service has answered at all,
    /// the host is on EC2, so a request it sheds or leaves unanswered is retried with a short
    /// back-off, and is the error when every attempt fails.
    ///
    /// `answered` records that the service has answered, and is read and set across calls, so a
    /// later lookup does not take a slow service for an absent one.
    pub async fn region(&self, answered: &AtomicBool) -> Result<Option<String>, CredentialsError> {
        if Self::disabled() {
            return Ok(None);
        }

        let mut backoff = retry(
            REGION_RETRY_START_MILLIS,
            REGION_RETRY_MAX_MILLIS,
            REGION_RETRY_LIMIT,
        );
        loop {
            let attempt = match self.token().await {
                Ok(token) => {
                    answered.store(true, Ordering::Relaxed);
                    self.get("/latest/meta-data/placement/region", &token)
                        .await
                        .map(|body| parse_region(&body))
                }
                // Nothing has ever answered at the metadata address: this host is not on EC2.
                Err(CredentialsError::NotProvided(_)) if !answered.load(Ordering::Relaxed) => {
                    return Ok(None);
                }
                // The service answered an earlier attempt, so this silence is the service being
                // slow rather than absent.
                Err(CredentialsError::NotProvided(provider)) => Err(CredentialsError::Transient {
                    provider,
                    detail: "token request went unanswered".to_owned(),
                }),
                Err(error) => {
                    if error.is_transient() {
                        answered.store(true, Ordering::Relaxed);
                    }
                    Err(error)
                }
            };

            match attempt {
                Ok(region) => return Ok(region),
                Err(error) if error.is_transient() => {
                    if !backoff.wait().await {
                        return Err(error);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "The instance metadata service refused the region lookup"
                    );
                    return Ok(None);
                }
            }
        }
    }
}

/// A region resolution that falls back to the instance metadata service, and keeps what the
/// service said.
///
/// Every client in a process runs on the same instance, so one answer serves every resolution, and
/// a host off EC2 waits on the metadata address once. A lookup the service kept shedding is not
/// kept, and resolutions that were waiting on it each run a lookup of their own in turn.
#[derive(Debug)]
pub struct InstanceRegion {
    region: OnceCell<Option<String>>,
    /// Whether the metadata service has answered any lookup, which makes this host one on EC2.
    answered: AtomicBool,
}

impl InstanceRegion {
    pub const fn new() -> Self {
        Self {
            region: OnceCell::const_new(),
            answered: AtomicBool::new(false),
        }
    }

    /// The region to sign for: [`resolve_region`] of `explicit`, then the region the metadata
    /// service names.
    ///
    /// `provider` builds the metadata client, and is called only by a resolution that asks.
    /// `AWS_EC2_METADATA_DISABLED` is honoured on every call, including after an answer was kept.
    pub async fn resolve(
        &self,
        explicit: Option<&str>,
        provider: impl FnOnce() -> Result<ImdsProvider, TransportError>,
    ) -> Option<String> {
        if let Some(region) = resolve_region(explicit) {
            return Some(region);
        }

        if ImdsProvider::disabled() {
            return None;
        }

        // Boxed because only a deployment that names no region reaches it, and the token and
        // metadata reads it holds would otherwise sit in every client build's future.
        let lookup = Box::pin(self.region.get_or_try_init(|| async {
            // A metadata client that cannot be built now cannot be built later either.
            let Ok(provider) = provider() else {
                return Ok(None);
            };

            provider.region(&self.answered).await
        }));

        match lookup.await {
            Ok(region) => region.clone(),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "The instance metadata service did not name the region after retrying; a later resolution asks again"
                );
                None
            }
        }
    }
}

impl Default for InstanceRegion {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProvideCredentials for ImdsProvider {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        if Self::disabled() {
            return Err(CredentialsError::NotProvided("imds"));
        }

        let token = self.token().await?;

        let roles = self
            .get("/latest/meta-data/iam/security-credentials/", &token)
            .await?;
        let roles = std::str::from_utf8(&roles).map_err(|error| CredentialsError::Failed {
            provider: "imds",
            detail: format!("role listing is not text: {error}"),
        })?;
        let role = roles
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or(CredentialsError::NotProvided("imds"))?;

        // The role name reaches a URL path, so it is encoded rather than interpolated raw.
        let mut path = String::from("/latest/meta-data/iam/security-credentials/");
        crate::aws::sign::percent_encode_into(&mut path, role, false);

        let body = self.get(&path, &token).await?;

        parse_metadata_credentials("imds", &body)
    }

    fn name(&self) -> &'static str {
        "imds"
    }
}
