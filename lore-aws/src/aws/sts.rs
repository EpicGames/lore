// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `AssumeRoleWithWebIdentity`, which is how EKS gives a pod credentials.
//!
//! The one STS call this client makes, and the reason it is the only one: it is *unsigned*
//! (<https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html>). The
//! projected service-account token is the proof of identity, so there is no chicken-and-egg between
//! signing and credential acquisition — which is exactly what `AssumeRole` role chaining would have.
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use url::Url;

use crate::aws::configured_endpoint;
use crate::aws::creds::Credentials;
use crate::aws::creds::CredentialsError;
use crate::aws::creds::ProvideCredentials;
use crate::aws::creds::parse_expiry;
use crate::aws::default_endpoint;
use crate::aws::error::Retryable;
use crate::aws::error::ServiceErrorCode;
use crate::aws::error::StsErrorCode;
use crate::aws::error::shed_by_status;
use crate::aws::http::HttpClient;
use crate::aws::http::HttpRequest;
use crate::aws::sign::percent_encode_into;
use crate::aws::xml::XmlError;
use crate::aws::xml::XmlEvent;
use crate::aws::xml::XmlReader;
use crate::aws::xml::parse_error_body;

/// STS's API version for the Query protocol.
const API_VERSION: &str = "2011-06-15";
/// A role assumption is a network round trip to a regional endpoint, not a link-local one.
const STS_TIMEOUT: Duration = Duration::from_secs(5);
/// Session name when the environment does not give one. Identifies the caller in `CloudTrail`.
const DEFAULT_SESSION_NAME: &str = "lore";

/// Credentials by exchanging a projected identity token for a role.
#[derive(Debug)]
pub struct WebIdentityProvider {
    http: HttpClient,
    region: String,
}

impl WebIdentityProvider {
    pub fn new(http: HttpClient, region: impl Into<String>) -> Self {
        Self {
            http,
            region: region.into(),
        }
    }

    /// The regional endpoint, so the call does not cross regions for no reason.
    ///
    /// An endpoint [`configured_endpoint`] finds in the environment overrides it. Without a region
    /// the global endpoint stands in, which is what a credential file that names no region leaves.
    #[lore_macro::test_pub]
    fn endpoint(&self) -> String {
        if let Some(endpoint) = configured_endpoint(None, "sts") {
            return endpoint;
        }

        if self.region.is_empty() {
            return "https://sts.amazonaws.com".to_owned();
        }

        default_endpoint("sts", &self.region)
    }
}

#[async_trait]
impl ProvideCredentials for WebIdentityProvider {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        let token_file = std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE")
            .ok()
            .filter(|path| !path.is_empty())
            .ok_or(CredentialsError::NotProvided("web-identity"))?;
        let role_arn = std::env::var("AWS_ROLE_ARN")
            .ok()
            .filter(|arn| !arn.is_empty())
            .ok_or_else(|| CredentialsError::Failed {
                provider: "web-identity",
                detail: "AWS_WEB_IDENTITY_TOKEN_FILE is set but AWS_ROLE_ARN is not".to_owned(),
            })?;
        let session_name = std::env::var("AWS_ROLE_SESSION_NAME")
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| DEFAULT_SESSION_NAME.to_owned());

        // Read on every refresh rather than cached: the projected token is rotated on disk by the
        // kubelet, and a cached copy would expire with no way back.
        let token = tokio::fs::read_to_string(&token_file)
            .await
            .map_err(|error| CredentialsError::Failed {
                provider: "web-identity",
                detail: format!("cannot read {token_file}: {error}"),
            })?;

        let url = Url::parse(&self.endpoint()).map_err(|error| CredentialsError::Failed {
            provider: "web-identity",
            detail: format!("STS endpoint is not a URL: {error}"),
        })?;

        let body = form_body(&role_arn, &session_name, token.trim());
        let mut request = HttpRequest::new(Method::POST, url).body(Bytes::from(body));
        request.headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let response = self
            .http
            .send(request, STS_TIMEOUT)
            .await
            .map_err(|error| CredentialsError::Transient {
                provider: "web-identity",
                detail: error.to_string(),
            })?;

        let text =
            std::str::from_utf8(&response.body).map_err(|error| CredentialsError::Failed {
                provider: "web-identity",
                detail: format!("STS response is not text: {error}"),
            })?;

        if !response.status.is_success() {
            return Err(assume_role_error(response.status, text));
        }

        parse_assume_role_response(text)
    }

    fn name(&self) -> &'static str {
        "web-identity"
    }
}

/// A non-success STS answer as a credentials failure.
///
/// Transient when STS shed the request, so the caller waits rather than reporting a deployment that
/// cannot sign. The status decides alongside the code, for the same reason it does on a service call:
/// an endpoint standing in for STS names its own exceptions, and `IDPCommunicationError` — STS failing
/// to reach the identity provider — is a 500 under a code no ladder would otherwise wait out.
#[lore_macro::test_pub]
fn assume_role_error(status: StatusCode, body: &str) -> CredentialsError {
    let parsed = parse_error_body(body);
    let detail = format!(
        "STS answered {} {}: {}",
        status.as_u16(),
        parsed.code.as_deref().unwrap_or("<no code>"),
        parsed.message.as_deref().unwrap_or("<no message>")
    );
    let code = StsErrorCode::from_wire(parsed.code.as_deref().unwrap_or(""), status);

    if code.retryable() == Retryable::Yes || shed_by_status(status) {
        CredentialsError::Transient {
            provider: "web-identity",
            detail,
        }
    } else {
        CredentialsError::Failed {
            provider: "web-identity",
            detail,
        }
    }
}

/// The Query-protocol form body.
///
/// Every value is percent-encoded: the token is a JWT, which carries `.` and may carry `-` and `_`,
/// and the ARN carries `:` and `/`. Interpolating them raw would corrupt the request.
#[lore_macro::test_pub]
fn form_body(role_arn: &str, session_name: &str, token: &str) -> String {
    let mut body = String::with_capacity(token.len() + role_arn.len() + 128);

    body.push_str("Action=AssumeRoleWithWebIdentity&Version=");
    body.push_str(API_VERSION);
    body.push_str("&RoleArn=");
    percent_encode_into(&mut body, role_arn, false);
    body.push_str("&RoleSessionName=");
    percent_encode_into(&mut body, session_name, false);
    body.push_str("&WebIdentityToken=");
    percent_encode_into(&mut body, token, false);

    body
}

/// A document that could not be walked to its end as a credentials failure.
fn unreadable_response(error: XmlError) -> CredentialsError {
    CredentialsError::Failed {
        provider: "web-identity",
        detail: format!("STS response is not readable XML: {error}"),
    }
}

/// Pull the credentials out of an `AssumeRoleWithWebIdentityResponse`.
///
/// The document is walked to its end, and a read that stops short is a failure rather than a short
/// answer. Abandoning the walk would let whatever had been read so far stand in for the whole
/// response: a body cut after `<SecretAccessKey>` parses into usable credentials with no expiry,
/// which the cache then holds as permanent and never refreshes.
#[lore_macro::test_pub]
fn parse_assume_role_response(body: &str) -> Result<Credentials, CredentialsError> {
    let mut reader = XmlReader::new(body);
    let mut access_key_id = None;
    let mut secret_access_key = None;
    let mut session_token = None;
    let mut expiration = None;
    let mut in_credentials = false;

    while let Some(event) = reader.next_event().map_err(unreadable_response)? {
        match event {
            XmlEvent::Open("Credentials") => in_credentials = true,
            XmlEvent::Close("Credentials") => in_credentials = false,
            // Only fields inside <Credentials> are read. The response also carries an
            // <AssumedRoleUser> with an <Arn>, and a nested read would confuse the two.
            XmlEvent::Open(name) if in_credentials => {
                let field = match name {
                    "AccessKeyId" => &mut access_key_id,
                    "SecretAccessKey" => &mut secret_access_key,
                    "SessionToken" => &mut session_token,
                    "Expiration" => &mut expiration,
                    _ => continue,
                };

                *field = Some(reader.text(name).map_err(unreadable_response)?.into_owned());
            }
            _ => {}
        }
    }

    let (Some(access_key_id), Some(secret_access_key), Some(expiration)) =
        (access_key_id, secret_access_key, expiration)
    else {
        return Err(CredentialsError::Failed {
            provider: "web-identity",
            detail: "STS response is missing a credentials field".to_owned(),
        });
    };

    Ok(Credentials::new(
        access_key_id,
        secret_access_key,
        session_token.map(Into::into),
        parse_expiry("web-identity", Some(&expiration))?,
    ))
}
