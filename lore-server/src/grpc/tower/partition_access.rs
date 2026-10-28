// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use http::HeaderMap;
use http::Request;
use http::Response;
use lore_revision::lore::RepositoryId;
use lore_transport::grpc::PARTITION_ID_KEY;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use tonic::Status;
use tonic::metadata::MetadataMap;
use tonic::server::NamedService;
use tower::Layer;
use tower::Service;

use crate::authnz::repository_authorizer::Grants;
use crate::authnz::repository_authorizer::PartitionGrants;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::authorization_timeout_status;
use crate::grpc::get_repository;
use crate::grpc::get_verified_token;
use crate::grpc::no_repository_access_status;

/// Refuses requests for a partition the caller cannot reach, before the
/// handler runs. Reaching a partition is the whole `read` check; handlers
/// that write or need a privileged action make their own check.
///
/// The validator works with the JWT interceptor. The interceptor
/// verifies the token and inserts it as extensions, this service reads them.
/// The validator takes the partition from the request metadata and asks the
/// configured [`RepositoryAuthorizer`] whether the caller may reach it.
/// Returns [`no_repository_access_status`] on denial.
///
/// The check lives here rather than in the interceptor because
/// tonic's `Interceptor::call` is synchronous and cannot await an online
/// authorizer. A tower `Service` can be async.
///
/// Fine-grained per-method permissions can be done in the handlers. To spare
/// the handler a second authorizer round trip, the layer asks the authorizer
/// to enumerate the caller's grants
/// ([`granted_actions`](RepositoryAuthorizer::granted_actions)): when it
/// can, reachability is answered from the enumeration and the enumeration is
/// exposed to the handler as a [`PartitionGrants`] extension. An authorizer
/// that cannot enumerate is asked `read` instead. Handlers make their checks through
/// `RepositoryAuthorizer::permits`, which consumes the exposed grants and
/// falls back to a per-action authorizer call when they are absent.
///
/// Checks that depend on the request body stay in handlers, since that's the
/// only place the body is decoded. All such calls still use the common
/// [`RepositoryAuthorizer`], so that all the authorization decisions are done
/// using the same logic.
///
/// The layer bounds its own authorization stage and nothing else. The inner
/// service is entered unbounded, because tonic decodes the request body
/// inside it: a bound spanning that decode expires on a client that is slow
/// to finish sending and attributes the stall to the server, under a status
/// that counts as a server error. Each handler behind this layer times out
/// the work it owns, which is the bound that keeps a request below the load
/// balancer's ceiling.
#[derive(Clone)]
pub struct PartitionAccessLayer {
    authorizer: Arc<dyn RepositoryAuthorizer>,
    /// Ceiling on the authorization stage alone — the one online call this
    /// layer may make. Sized for reaching the authorizer, not for a whole
    /// request, so a stalled online authorizer cannot park every
    /// partition-scoped RPC until the client gives up.
    authorization_timeout: Duration,
}

impl PartitionAccessLayer {
    pub fn new(authorizer: Arc<dyn RepositoryAuthorizer>, authorization_timeout: Duration) -> Self {
        Self {
            authorizer,
            authorization_timeout,
        }
    }
}

impl<S> Layer<S> for PartitionAccessLayer {
    type Service = PartitionAccessService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PartitionAccessService {
            inner,
            authorizer: self.authorizer.clone(),
            authorization_timeout: self.authorization_timeout,
        }
    }
}

#[derive(Clone)]
pub struct PartitionAccessService<S> {
    inner: S,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    authorization_timeout: Duration,
}

/// The authorization stage's verdict on one request.
enum Access {
    /// Reachable; the enumerated grants when the authorizer had them.
    Granted(Option<Grants>),
    Denied,
}

/// The partition the request names, [`None`] when doesn't name one.
fn named_partition(headers: &HeaderMap) -> Result<Option<RepositoryId>, Status> {
    if !headers.contains_key(PARTITION_ID_KEY) && !headers.contains_key(REPOSITORY_ID_KEY) {
        return Ok(None);
    }
    let metadata = MetadataMap::from_headers(headers.clone());
    get_repository(&metadata).map(Some)
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for PartitionAccessService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send,
    ReqBody: Send + 'static,
    ResBody: Default,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let authorizer = self.authorizer.clone();
        let authorization_timeout = self.authorization_timeout;

        Box::pin(async move {
            let mut request = request;
            let repository = match named_partition(request.headers()) {
                Ok(Some(repository)) => repository,
                Ok(None) => return inner.call(request).await,
                Err(status) => return Ok(status.into_http()),
            };

            // Authorization denials are flattened to `Denied` inside the
            // timed stage, so elapsing is the only thing the stage reports
            // besides a verdict. Only the extensions are borrowed into the
            // stage, not the request, so the future stays `Send` for any
            // body type.
            let access = {
                let extensions = request.extensions();
                tokio::time::timeout(authorization_timeout, async move {
                    let token = get_verified_token(extensions);
                    match authorizer.granted_access(token.as_ref(), repository).await {
                        Ok(grants) => Access::Granted(grants),
                        Err(_denied) => Access::Denied,
                    }
                })
                .await
            };

            match access {
                Ok(Access::Granted(grants)) => {
                    if let Some(grants) = grants {
                        // The enumeration also answers the handler's action
                        // checks, without another authorizer call.
                        request.extensions_mut().insert(PartitionGrants {
                            repository_id: repository,
                            grants,
                        });
                    }
                    // Entered unbounded: the decode of the request body
                    // happens in here, and time a client spends sending is
                    // not the server's to time out. The handler bounds the
                    // work it owns once it has the body.
                    inner.call(request).await
                }
                // Flattened to one uniform status, so an unauthorized caller
                // learns nothing from the reason.
                Ok(Access::Denied) => Ok(no_repository_access_status().into_http()),
                Err(_elapsed) => Ok(authorization_timeout_status().into_http()),
            }
        })
    }
}

// Required to mount the wrapped service on the router under the inner
// service's route.
impl<S: NamedService> NamedService for PartitionAccessService<S> {
    const NAME: &'static str = S::NAME;
}
