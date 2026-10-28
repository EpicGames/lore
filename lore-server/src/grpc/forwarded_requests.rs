// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

pub mod lock_service;
pub mod repository_service;
pub mod revision_service;

use std::str::FromStr;
use std::time::Duration;

use http::Uri;
use lore_base::lore_spawn_net;
use lore_base::types::RepositoryId;
use lore_error_set::prelude::*;
use lore_revision::errors::UnhandledError;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use lore_transport::user_agent;
use serde::Deserialize;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::transport::Channel;

use crate::grpc::extract_correlation_id;
use crate::grpc::forwarded_requests::lock_service::ForwardedLockServiceClient;
use crate::grpc::forwarded_requests::lock_service::GrpcForwardedLockServiceClient;
use crate::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
use crate::grpc::forwarded_requests::repository_service::GrpcForwardedRepositoryServiceClient;
use crate::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
use crate::grpc::forwarded_requests::revision_service::GrpcForwardedRevisionServiceClient;
use crate::grpc::get_is_service_account;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::settings::GrpcInternalClientSettings;
use crate::tls::load_client_tls;

pub type InternalClientError = UnhandledError;
pub type ForwardedRequestResult<T> = Result<Result<Response<T>, Status>, InternalClientError>;

const ON_BEHALF_OF_USER_ID_FIELD: &str = "on-behalf-of-user-id";
const ON_BEHALF_OF_AUTHORIZATION_FIELD: &str = "on-behalf-of-authorization";
const ON_BEHALF_OF_SERVICE_ACCOUNT_FIELD: &str = "on-behalf-of-service-account";

/// Classify what a forwarded call returned.
///
/// A `Status` decoded from a peer's response trailers carries no source error;
/// one tonic synthesizes locally from a connect, HTTP/2 or codec failure carries
/// the underlying error. Only the first is the peer's answer to the caller, so
/// the second becomes an [`InternalClientError`] and the origin substitutes its
/// own status instead of passing transport wording on to the client.
///
/// Two locally-produced statuses carry no source and are therefore treated as
/// the peer's answer: a `Status` boxed as an error by a middleware in the
/// channel stack, and tower's load-shed `Overloaded`. [`make_channel`] adds
/// neither, so revisit this if the channel gains a layer.
#[lore_macro::test_pub]
pub(crate) fn classify_forwarded_result<T>(
    result: Result<Response<T>, Status>,
) -> ForwardedRequestResult<T> {
    match result {
        Ok(response) => Ok(Ok(response)),
        Err(status) if std::error::Error::source(&status).is_none() => Ok(Err(status)),
        Err(status) => Err(InternalClientError::internal_with_context(
            status,
            "forwarded request did not reach the peer",
        )),
    }
}

/// Reconstructed information about the end user client who has performed a particular
/// RPC. Used in place of directly reading from metadata to avoid it being brought into
/// scope and incorrect information being read from it.
#[derive(Clone, Debug)]
pub struct CallerContext {
    pub repository_id: RepositoryId,
    pub user_id: String,
    pub is_service_account: bool,
    pub correlation_id: String,
    pub authorization: Option<String>,
}

impl CallerContext {
    /// Create the caller context from this original request. Call this function
    /// from the server that is the first one to receive an RPC from an end user client
    pub fn from_original_request<T>(request: &Request<T>) -> Result<Self, Status> {
        Ok(Self {
            repository_id: get_repository(request.metadata())?,
            user_id: get_user_id(request.extensions()),
            is_service_account: get_is_service_account(request.extensions()),
            correlation_id: extract_correlation_id(request).unwrap_or_default(),
            authorization: request
                .metadata()
                .get("authorization")
                .map(|v| {
                    v.to_str()
                        .map(|s| s.to_string())
                        .map_err(|_err| Status::internal("invalid `authorization` header"))
                })
                .transpose()?,
        })
    }

    /// Wraps `body` in a `Request` and stamps the caller's identity into the
    /// metadata so the receiving server can reconstruct this context via
    /// [`Self::from_forwarded_request`].
    pub fn to_forwarded_request<T>(&self, body: T) -> Result<Request<T>, Status> {
        let mut request = Request::new(body);
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(self.repository_id.data()),
        );
        request.metadata_mut().insert(
            ON_BEHALF_OF_USER_ID_FIELD,
            self.user_id
                .parse()
                .map_err(|_err| Status::internal("invalid user_id for forwarding"))?,
        );
        if self.is_service_account {
            request.metadata_mut().insert(
                ON_BEHALF_OF_SERVICE_ACCOUNT_FIELD,
                tonic::metadata::MetadataValue::from_static("true"),
            );
        }
        if !self.correlation_id.is_empty()
            && let Ok(value) = self.correlation_id.parse()
        {
            request.metadata_mut().insert(CORRELATION_ID_HEADER, value);
        }
        if let Some(auth) = &self.authorization
            && let Ok(value) = auth.parse()
        {
            request
                .metadata_mut()
                .insert(ON_BEHALF_OF_AUTHORIZATION_FIELD, value);
        }
        Ok(request)
    }

    /// Create the caller context from this forwarded request. Call this function
    /// from the server that has received this forwarded request from another Lore server
    pub fn from_forwarded_request<T>(request: &Request<T>) -> Result<Self, Status> {
        let user_id = request
            .metadata()
            .get(ON_BEHALF_OF_USER_ID_FIELD)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string())
            .ok_or_else(|| Status::internal("missing/invalid `on-behalf-of-user-id` field"))?;

        // Absent from a peer that predates the field: such a peer forwarded
        // every caller as a user.
        let is_service_account = request
            .metadata()
            .get(ON_BEHALF_OF_SERVICE_ACCOUNT_FIELD)
            .is_some_and(|v| v == "true");

        Ok(Self {
            repository_id: get_repository(request.metadata())?,
            user_id,
            is_service_account,
            correlation_id: extract_correlation_id(request).unwrap_or_default(),
            authorization: request
                .metadata()
                .get(ON_BEHALF_OF_AUTHORIZATION_FIELD)
                .map(|v| {
                    v.to_str().map(|s| s.to_string()).map_err(|_err| {
                        Status::internal("invalid `on-behalf-of-authorization` header")
                    })
                })
                .transpose()?,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ForwardedRequestsSettings {
    pub client: GrpcInternalClientSettings,
    #[serde(default)]
    pub enabled_rpcs: RpcFlags,
}

#[derive(Clone, Default, Debug, Deserialize)]
pub struct RpcFlags {
    #[serde(default)]
    pub revision_branch_create: bool,
    #[serde(default)]
    pub revision_branch_delete: bool,
    #[serde(default)]
    pub revision_branch_get: bool,
    #[serde(default)]
    pub revision_branch_list: bool,

    #[serde(default)]
    pub repository_create: bool,
    #[serde(default)]
    pub repository_get: bool,

    #[serde(default)]
    pub lock_query: bool,
}

pub trait ForwardedRequests: Send + Sync {
    fn rpc_flags(&self) -> &RpcFlags;
    fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient>;
    fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient>;
    fn forwarded_lock_service(&self) -> Box<dyn ForwardedLockServiceClient>;
}

async fn make_channel(settings: &GrpcInternalClientSettings) -> Result<Channel, UnhandledError> {
    let tls_config = if let Some(certs) = &settings.certs {
        let tls = load_client_tls(certs.clone())
            .forward::<UnhandledError>("loading client tls with certs")?;
        Some(tls)
    } else {
        None
    };

    let url = Uri::from_str(&settings.url).internal("parsing url")?;
    let mut endpoint = Channel::builder(url);
    if let Some(tls) = tls_config {
        endpoint = endpoint.tls_config(tls).internal("using TLS config")?;
    }

    let endpoint = endpoint
        .user_agent(user_agent())
        .internal("error setting user agent")?
        .connect_timeout(Duration::from_secs(settings.connect_timeout_seconds))
        .timeout(Duration::from_secs(settings.request_timeout_seconds))
        .tcp_keepalive(Some(Duration::from_secs(settings.tcp_keepalive_seconds)))
        .http2_keep_alive_interval(Duration::from_secs(
            settings.http2_keepalive_interval_seconds,
        ))
        .keep_alive_timeout(Duration::from_secs(
            settings.http2_keepalive_timeout_seconds,
        ))
        .keep_alive_while_idle(true);
    // Connect from net so the hyper/h2 driver tasks this spawns bind there rather
    // than to the core runtime the caller runs on.
    let channel = lore_spawn_net!(async move { endpoint.connect().await })
        .await
        .internal("connection task to endpoint")?
        .internal("connecting to endpoint")?;
    Ok(channel)
}

pub struct GrpcForwardedRequests {
    channel: Channel,
    flags: RpcFlags,
}

impl GrpcForwardedRequests {
    pub async fn new(settings: &ForwardedRequestsSettings) -> Result<Self, UnhandledError> {
        let channel = make_channel(&settings.client).await?;

        Ok(Self {
            channel,
            flags: settings.enabled_rpcs.clone(),
        })
    }
}

impl ForwardedRequests for GrpcForwardedRequests {
    fn rpc_flags(&self) -> &RpcFlags {
        &self.flags
    }

    fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient> {
        let client = GrpcForwardedRevisionServiceClient::new(self.channel.clone());
        Box::new(client)
    }

    fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient> {
        let client = GrpcForwardedRepositoryServiceClient::new(self.channel.clone());
        Box::new(client)
    }

    fn forwarded_lock_service(&self) -> Box<dyn ForwardedLockServiceClient> {
        let client = GrpcForwardedLockServiceClient::new(self.channel.clone());
        Box::new(client)
    }
}
