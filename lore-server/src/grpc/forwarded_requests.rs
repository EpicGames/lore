// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

pub mod repository_service;
pub mod revision_service;

use std::str::FromStr;

use http::Uri;
use lore_base::lore_spawn_net;
use lore_base::types::RepositoryId;
use lore_error_set::prelude::*;
use lore_revision::errors::UnhandledError;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use lore_transport::grpc::user_agent;
use serde::Deserialize;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::transport::Channel;

use crate::grpc::extract_correlation_id;
use crate::grpc::forwarded_requests::repository_service::ForwardedRepositoryServiceClient;
use crate::grpc::forwarded_requests::repository_service::GrpcForwardedRepositoryServiceClient;
use crate::grpc::forwarded_requests::revision_service::ForwardedRevisionServiceClient;
use crate::grpc::forwarded_requests::revision_service::GrpcForwardedRevisionServiceClient;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::settings::GrpcInternalClientSettings;
use crate::tls::load_client_tls;

pub type InternalClientError = UnhandledError;
pub type ForwardedRequestResult<T> = Result<Result<Response<T>, Status>, InternalClientError>;

const ON_BEHALF_OF_USER_ID_FIELD: &str = "on-behalf-of-user-id";
const ON_BEHALF_OF_AUTHORIZATION_FIELD: &str = "on-behalf-of-authorization";

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

        Ok(Self {
            repository_id: get_repository(request.metadata())?,
            user_id,
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
}

pub trait ForwardedRequests: Send + Sync {
    fn rpc_flags(&self) -> &RpcFlags;
    fn forwarded_revision_service(&self) -> Box<dyn ForwardedRevisionServiceClient>;
    fn forwarded_repository_service(&self) -> Box<dyn ForwardedRepositoryServiceClient>;
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
        .internal("error setting user agent")?;
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
}

#[cfg(test)]
mod test {
    use lore_revision::lore::RepositoryId;
    use lore_transport::grpc::CORRELATION_ID_HEADER;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;

    fn insert_repository<T>(request: &mut Request<T>, repository: RepositoryId) {
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
    }

    mod from_original_request {
        use super::*;

        #[test]
        fn extracts_all_fields() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);
            request.extensions_mut().insert(AuthorizationToken {
                user_id: "alice".into(),
                ..AuthorizationToken::default()
            });
            request
                .metadata_mut()
                .insert(CORRELATION_ID_HEADER, "corr-123".parse().unwrap());

            let ctx = CallerContext::from_original_request(&request).unwrap();
            assert_eq!(ctx.repository_id, repository);
            assert_eq!(ctx.user_id, "alice");
            assert_eq!(ctx.correlation_id, "corr-123");
            assert_eq!(ctx.authorization, None);
        }

        #[test]
        fn authorization_is_extracted_when_present() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);
            request
                .metadata_mut()
                .insert("authorization", "Bearer token-abc".parse().unwrap());

            let ctx = CallerContext::from_original_request(&request).unwrap();
            assert_eq!(ctx.authorization.as_deref(), Some("Bearer token-abc"));
        }

        #[test]
        fn authorization_is_none_when_absent() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);

            let ctx = CallerContext::from_original_request(&request).unwrap();
            assert_eq!(ctx.authorization, None);
        }
    }

    mod from_forwarded_request {
        use super::*;

        #[test]
        fn missing_user_id_returns_internal_error() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);

            let err = CallerContext::from_forwarded_request(&request).unwrap_err();
            assert_eq!(err.code(), tonic::Code::Internal);
            assert_eq!(
                err.message(),
                "missing/invalid `on-behalf-of-user-id` field"
            );
        }

        #[test]
        fn extracts_all_fields() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);
            request
                .metadata_mut()
                .insert("on-behalf-of-user-id", "alice".parse().unwrap());
            request
                .metadata_mut()
                .insert(CORRELATION_ID_HEADER, "corr-456".parse().unwrap());

            let ctx = CallerContext::from_forwarded_request(&request).unwrap();
            assert_eq!(ctx.repository_id, repository);
            assert_eq!(ctx.user_id, "alice");
            assert_eq!(ctx.correlation_id, "corr-456");
            assert_eq!(ctx.authorization, None);
        }

        #[test]
        fn authorization_is_extracted_when_present() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);
            request
                .metadata_mut()
                .insert("on-behalf-of-user-id", "alice".parse().unwrap());
            request
                .metadata_mut()
                .insert("on-behalf-of-authorization", "Bearer tok".parse().unwrap());

            let ctx = CallerContext::from_forwarded_request(&request).unwrap();
            assert_eq!(ctx.authorization.as_deref(), Some("Bearer tok"));
        }

        #[test]
        fn authorization_is_none_when_absent() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);
            request
                .metadata_mut()
                .insert("on-behalf-of-user-id", "alice".parse().unwrap());

            let ctx = CallerContext::from_forwarded_request(&request).unwrap();
            assert_eq!(ctx.authorization, None);
        }
    }

    mod classify_forwarded_result {
        use super::*;

        /// Stands in for a transport failure: tonic reports one as a `Status`
        /// carrying the underlying error as its source.
        fn locally_synthesized_status() -> Status {
            Status::from_error(Box::new(std::io::Error::other("tcp connect error")))
        }

        #[test]
        fn success_is_passed_through() {
            let response = classify_forwarded_result(Ok(Response::new(42)))
                .expect("a response is not a client error")
                .expect("a response is not a peer error");
            assert_eq!(response.into_inner(), 42);
        }

        #[test]
        fn sourceless_status_is_the_peer_answer_for_the_client() {
            let result: ForwardedRequestResult<()> =
                classify_forwarded_result(Err(Status::not_found("no such repository")));

            let status = result
                .expect("a peer answer is not a client error")
                .expect_err("the peer answered with an error");
            assert_eq!(status.code(), tonic::Code::NotFound);
            assert_eq!(status.message(), "no such repository");
        }

        #[test]
        fn sourced_status_becomes_an_internal_client_error() {
            let result: ForwardedRequestResult<()> =
                classify_forwarded_result(Err(locally_synthesized_status()));

            let err = result.expect_err("a transport failure is a client error");
            assert!(
                err.to_string().contains("did not reach the peer"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn transport_detail_is_kept_for_the_log() {
            let result: ForwardedRequestResult<()> =
                classify_forwarded_result(Err(locally_synthesized_status()));

            let err = result.expect_err("a transport failure is a client error");
            let detail = format!("{err:?}");
            assert!(
                detail.contains("tcp connect error"),
                "the failing status should stay in the error chain: {detail}"
            );
        }
    }

    mod to_forwarded_request {
        use super::*;

        fn make_context(repository: RepositoryId, authorization: Option<&str>) -> CallerContext {
            CallerContext {
                repository_id: repository,
                user_id: "alice".into(),
                correlation_id: "corr-789".into(),
                authorization: authorization.map(|s| s.to_string()),
            }
        }

        #[test]
        fn authorization_is_stamped_when_some() {
            let repository = random::<RepositoryId>();
            let ctx = make_context(repository, Some("Bearer secret"));

            let request = ctx.to_forwarded_request(()).unwrap();
            let value = request
                .metadata()
                .get("on-behalf-of-authorization")
                .and_then(|v| v.to_str().ok());
            assert_eq!(value, Some("Bearer secret"));
        }

        #[test]
        fn authorization_is_absent_when_none() {
            let repository = random::<RepositoryId>();
            let ctx = make_context(repository, None);

            let request = ctx.to_forwarded_request(()).unwrap();
            assert!(
                request
                    .metadata()
                    .get("on-behalf-of-authorization")
                    .is_none(),
                "on-behalf-of-authorization should not be set when authorization is None"
            );
        }

        #[test]
        fn round_trip_preserves_authorization() {
            let repository = random::<RepositoryId>();
            let ctx = make_context(repository, Some("Bearer round-trip"));

            let forwarded = ctx.to_forwarded_request(()).unwrap();
            let recovered = CallerContext::from_forwarded_request(&forwarded).unwrap();
            assert_eq!(
                recovered.authorization.as_deref(),
                Some("Bearer round-trip")
            );
        }

        #[test]
        fn round_trip_preserves_none_authorization() {
            let repository = random::<RepositoryId>();
            let ctx = make_context(repository, None);

            let forwarded = ctx.to_forwarded_request(()).unwrap();
            let recovered = CallerContext::from_forwarded_request(&forwarded).unwrap();
            assert_eq!(recovered.authorization, None);
        }
    }
}
