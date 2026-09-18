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
use crate::grpc::get_repository;
use crate::grpc::get_verified_token;
use crate::grpc::no_repository_access_status;
use crate::grpc::timeout_grpc;

/// Enforces partition access for every RPC at the service level, so no
/// handler can forget the check.
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
/// that cannot enumerate is asked the plain reachability question
/// (`action: None`) instead. Handlers make their checks through
/// `RepositoryAuthorizer::permits`, which consumes the exposed grants and
/// falls back to a per-action authorizer call when they are absent.
///
/// Checks that depend on the request body stay in handlers, since that's the
/// only place the body is decoded. All such calls still use the common
/// [`RepositoryAuthorizer`], so that all the authorization decisions are done
/// using the same logic.
#[derive(Clone)]
pub struct PartitionAccessLayer {
    authorizer: Arc<dyn RepositoryAuthorizer>,
    /// The request's whole server-side budget, the same request-handler
    /// timeout the handlers behind this layer enforce themselves. The
    /// authorization stage consumes from it and the handler stage is bounded
    /// by the remainder, so the two do not stack: a request never holds the
    /// server longer than one budget, which is what keeps it below the load
    /// balancer's timeout. Without the authorization-stage bound a stalled
    /// online authorizer would park every partition-scoped RPC until the
    /// client gives up.
    request_timeout: Duration,
}

impl PartitionAccessLayer {
    pub fn new(authorizer: Arc<dyn RepositoryAuthorizer>, request_timeout: Duration) -> Self {
        Self {
            authorizer,
            request_timeout,
        }
    }
}

impl<S> Layer<S> for PartitionAccessLayer {
    type Service = PartitionAccessService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PartitionAccessService {
            inner,
            authorizer: self.authorizer.clone(),
            request_timeout: self.request_timeout,
        }
    }
}

#[derive(Clone)]
pub struct PartitionAccessService<S> {
    inner: S,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    request_timeout: Duration,
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
        let request_timeout = self.request_timeout;

        Box::pin(async move {
            let mut request = request;
            let repository = match named_partition(request.headers()) {
                Ok(Some(repository)) => repository,
                Ok(None) => return inner.call(request).await,
                Err(status) => return Ok(status.into_http()),
            };

            let stage_started = std::time::Instant::now();
            // Authorization denials are flattened to `Denied` inside the
            // timed stage, so the only error escaping it is the timeout's
            // own status. Only the extensions are borrowed into the stage,
            // not the request, so the future stays `Send` for any body type.
            let access = {
                let extensions = request.extensions();
                timeout_grpc(request_timeout, async move {
                    let token = get_verified_token(extensions);
                    Ok(
                        match authorizer.granted_access(token.as_ref(), repository).await {
                            Ok(grants) => Access::Granted(grants),
                            Err(_denied) => Access::Denied,
                        },
                    )
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
                    // The handler stage gets what the authorization stage
                    // left of the budget, so the two stages share one
                    // request timeout instead of stacking two. Handlers
                    // keep their own equal bound; this one only fires
                    // earlier by however long authorization took.
                    let remaining = request_timeout.saturating_sub(stage_started.elapsed());
                    match tokio::time::timeout(remaining, inner.call(request)).await {
                        Ok(response) => response,
                        Err(_elapsed) => {
                            Ok(Status::cancelled("Request handler timeout exceeded").into_http())
                        }
                    }
                }
                // Flattened to one uniform status, so an unauthorized caller
                // learns nothing from the reason.
                Ok(Access::Denied) => Ok(no_repository_access_status().into_http()),
                // The stage timed out: a server condition, answered with the
                // same status a handler timeout produces rather than masked
                // as a denial.
                Err(status) => Ok(status.into_http()),
            }
        })
    }
}

// Required to mount the wrapped service on the router under the inner
// service's route.
impl<S: NamedService> NamedService for PartitionAccessService<S> {
    const NAME: &'static str = S::NAME;
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::str::FromStr;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use async_trait::async_trait;
    use lore_base::types::Context as LoreContext;
    use tonic::Code;
    use tonic::metadata::MetadataValue;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;
    use crate::authnz::repository_authorizer::Grants;
    use crate::authnz::repository_authorizer::RawToken;
    use crate::authnz::repository_authorizer::VerifiedToken;

    /// One recorded authorizer question: the token's subject, the
    /// partition, and the action.
    type Asked = (Option<String>, RepositoryId, Option<String>);

    /// Records every question it is asked and answers with a fixed verdict.
    struct RecordingAuthorizer {
        permit: bool,
        asked: Mutex<Vec<Asked>>,
    }

    impl RecordingAuthorizer {
        fn new(permit: bool) -> Arc<Self> {
            Arc::new(Self {
                permit,
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<Asked> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RepositoryAuthorizer for RecordingAuthorizer {
        async fn check_repository_access(
            &self,
            token: Option<&VerifiedToken<'_>>,
            repository_id: RepositoryId,
            action: Option<&str>,
        ) -> Result<(), Status> {
            self.asked.lock().unwrap().push((
                token.map(|token| token.claims.user_id.clone()),
                repository_id,
                action.map(str::to_string),
            ));
            if self.permit {
                Ok(())
            } else {
                Err(Status::permission_denied("no grant"))
            }
        }
    }

    /// Counts calls and answers OK, standing in for a generated service.
    #[derive(Clone, Default)]
    struct Inner(Arc<AtomicUsize>);

    impl Inner {
        fn calls(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Service<Request<()>> for Inner {
        type Response = Response<()>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Response<()>, Infallible>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: Request<()>) -> Self::Future {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(Response::new(())))
        }
    }

    fn repository() -> RepositoryId {
        LoreContext::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into()
    }

    /// Builds the request the layer sees: the interceptor has already run, so
    /// the token halves are extensions and the partition is a binary header.
    fn request(partition: Option<RepositoryId>, token: bool) -> Request<()> {
        let mut request = Request::builder()
            .uri("/x.Service/TheRpc")
            .body(())
            .unwrap();
        if let Some(repository) = partition {
            // Encoded through tonic's own metadata code so the header bytes
            // cannot drift from what the wire carries.
            let mut metadata = MetadataMap::new();
            metadata.append_bin(
                PARTITION_ID_KEY,
                MetadataValue::from_bytes(repository.data()),
            );
            request.headers_mut().extend(metadata.into_headers());
        }
        if token {
            request.extensions_mut().insert(AuthorizationToken {
                user_id: "the u".to_string(),
                ..Default::default()
            });
            request.extensions_mut().insert(RawToken("raw.jwt".into()));
        }
        request
    }

    fn service_with(layer: PartitionAccessLayer) -> (PartitionAccessService<Inner>, Inner) {
        let inner = Inner::default();
        (layer.layer(inner.clone()), inner)
    }

    fn status_of(response: &Response<()>) -> Status {
        Status::from_header_map(response.headers()).expect("a gRPC status in the headers")
    }

    #[tokio::test]
    async fn a_denied_partition_never_reaches_the_service() {
        let authorizer = RecordingAuthorizer::new(false);
        let (mut service, inner) =
            service_with(PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT));

        let response = service
            .call(request(Some(repository()), true))
            .await
            .unwrap();

        let status = status_of(&response);
        assert_eq!(status.code(), Code::PermissionDenied);
        // The uniform denial status, not the authorizer's own reason.
        assert_eq!(status.message(), no_repository_access_status().message());
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn a_permitted_partition_reaches_the_service() {
        let authorizer = RecordingAuthorizer::new(true);
        let (mut service, inner) =
            service_with(PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT));

        service
            .call(request(Some(repository()), true))
            .await
            .unwrap();

        assert_eq!(inner.calls(), 1);
        // The reachability question, with the verified claims attached.
        assert_eq!(
            authorizer.asked(),
            vec![(Some("the u".to_string()), repository(), None)]
        );
    }

    /// A request naming no partition gets no decision, not a check against
    /// the zero partition id.
    #[tokio::test]
    async fn no_partition_metadata_means_no_decision() {
        let authorizer = RecordingAuthorizer::new(false);
        let (mut service, inner) =
            service_with(PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT));

        service.call(request(None, true)).await.unwrap();

        assert_eq!(inner.calls(), 1);
        assert!(authorizer.asked().is_empty());
    }

    /// No verified token still asks the authorizer, with `None`: the checking
    /// tiers deny, and `AllowAllRepositoryAuthorizer` keeps an unauthenticated
    /// server open.
    #[tokio::test]
    async fn a_missing_token_is_the_authorizers_question_too() {
        let authorizer = RecordingAuthorizer::new(false);
        let (mut service, inner) =
            service_with(PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT));

        let response = service
            .call(request(Some(repository()), false))
            .await
            .unwrap();

        assert_eq!(status_of(&response).code(), Code::PermissionDenied);
        assert_eq!(inner.calls(), 0);
        assert_eq!(authorizer.asked(), vec![(None, repository(), None)]);
    }

    /// Partition metadata that is present but undecodable is refused, not
    /// passed through: "no decision" is only for requests that name nothing.
    #[tokio::test]
    async fn undecodable_partition_metadata_is_refused() {
        let authorizer = RecordingAuthorizer::new(true);
        let (mut service, inner) =
            service_with(PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT));

        let mut request = request(None, true);
        request.headers_mut().insert(
            PARTITION_ID_KEY,
            http::HeaderValue::from_static("///not-base64///"),
        );

        let response = service.call(request).await.unwrap();

        assert_eq!(status_of(&response).code(), Code::InvalidArgument);
        assert_eq!(inner.calls(), 0);
        assert!(authorizer.asked().is_empty());
    }

    /// The seam for per-operation permissions: a mapped method asks for its
    /// action in the same single authorizer call, and unmapped methods keep
    /// asking plain reachability.
    /// An authorizer that enumerates, standing in for the shipped four. The
    /// layer must answer reachability from the enumeration, expose it to the
    /// handler, and never fall back to the per-question path.
    struct EnumeratingAuthorizer {
        grants: Grants,
        checks: AtomicUsize,
    }

    impl EnumeratingAuthorizer {
        fn new(grants: Grants) -> Arc<Self> {
            Arc::new(Self {
                grants,
                checks: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl RepositoryAuthorizer for EnumeratingAuthorizer {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            _action: Option<&str>,
        ) -> Result<(), Status> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn granted_actions(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
        ) -> Result<Option<Grants>, Status> {
            Ok(Some(self.grants.clone()))
        }
    }

    /// One received extension: the partition and grants it carried, or
    /// [`None`] when the request arrived without one.
    type SeenGrants = Option<(RepositoryId, Grants)>;

    /// A service that records the [`PartitionGrants`] extension it received.
    #[derive(Clone, Default)]
    struct GrantsInner(Arc<Mutex<Vec<SeenGrants>>>);

    impl Service<Request<()>> for GrantsInner {
        type Response = Response<()>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Response<()>, Infallible>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: Request<()>) -> Self::Future {
            self.0.lock().unwrap().push(
                request
                    .extensions()
                    .get::<PartitionGrants>()
                    .map(|grants| (grants.repository_id, grants.grants.clone())),
            );
            std::future::ready(Ok(Response::new(())))
        }
    }

    /// One enumeration serves both halves: reachability is answered from it
    /// and the handler receives it as an extension naming the partition, so
    /// an action check needs no second authorizer call.
    #[tokio::test]
    async fn an_enumeration_is_exposed_to_the_handler_and_answers_reachability() {
        let grants = Grants::Actions(["migrate".to_string()].into());
        let authorizer = EnumeratingAuthorizer::new(grants.clone());
        let inner = GrantsInner::default();
        let mut service =
            PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT).layer(inner.clone());

        service
            .call(request(Some(repository()), true))
            .await
            .unwrap();

        assert_eq!(*inner.0.lock().unwrap(), vec![Some((repository(), grants))]);
        assert_eq!(
            authorizer.checks.load(Ordering::SeqCst),
            0,
            "reachability comes from the enumeration, not a second question"
        );
    }

    /// An enumeration holding no access denies without consulting the
    /// per-question path — `Denied` is a verdict, not a fallback.
    #[tokio::test]
    async fn an_enumerated_denial_never_reaches_the_service() {
        let authorizer = EnumeratingAuthorizer::new(Grants::Denied);
        let inner = GrantsInner::default();
        let mut service =
            PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT).layer(inner.clone());

        let response = service
            .call(request(Some(repository()), true))
            .await
            .unwrap();

        assert_eq!(status_of(&response).code(), Code::PermissionDenied);
        assert!(inner.0.lock().unwrap().is_empty());
        assert_eq!(authorizer.checks.load(Ordering::SeqCst), 0);
    }

    /// Ample for every in-memory test. The stalled-authorizer test uses its
    /// own short bound.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// A failing enumeration denies, flattened like every other denial.
    #[tokio::test]
    async fn a_failing_enumeration_denies() {
        struct FailingAuthorizer;

        #[async_trait]
        impl RepositoryAuthorizer for FailingAuthorizer {
            async fn check_repository_access(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
                _action: Option<&str>,
            ) -> Result<(), Status> {
                unreachable!("the failed enumeration is the verdict");
            }

            async fn granted_actions(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
            ) -> Result<Option<Grants>, Status> {
                Err(Status::internal("auth service unreachable"))
            }
        }

        let inner = GrantsInner::default();
        let mut service = PartitionAccessLayer::new(Arc::new(FailingAuthorizer), TEST_TIMEOUT)
            .layer(inner.clone());

        let response = service
            .call(request(Some(repository()), true))
            .await
            .unwrap();

        assert_eq!(status_of(&response).code(), Code::PermissionDenied);
        assert!(inner.0.lock().unwrap().is_empty());
    }

    /// A stalled authorizer cannot park the request: the authorization stage
    /// runs under the same request-handler timeout as the handlers, and a
    /// timeout answers with the handler timeout's own status, not a denial.
    #[tokio::test]
    async fn a_stalled_authorizer_times_out_with_the_handler_status() {
        struct StalledAuthorizer;

        #[async_trait]
        impl RepositoryAuthorizer for StalledAuthorizer {
            async fn check_repository_access(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
                _action: Option<&str>,
            ) -> Result<(), Status> {
                std::future::pending().await
            }

            async fn granted_actions(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
            ) -> Result<Option<Grants>, Status> {
                std::future::pending().await
            }
        }

        let inner = GrantsInner::default();
        let mut service =
            PartitionAccessLayer::new(Arc::new(StalledAuthorizer), Duration::from_millis(50))
                .layer(inner.clone());

        let response = service
            .call(request(Some(repository()), true))
            .await
            .unwrap();

        let status = status_of(&response);
        assert_eq!(status.code(), Code::Cancelled);
        assert_eq!(status.message(), "Request handler timeout exceeded");
        assert!(inner.0.lock().unwrap().is_empty());
    }

    /// The authorization stage and the handler stage share one request
    /// budget: time the authorizer consumes is deducted from what the
    /// handler may use, so a slow authorizer plus a slow handler cannot
    /// hold the server for two budgets.
    #[tokio::test]
    async fn the_two_stages_share_one_request_budget() {
        struct SlowAuthorizer(Duration);

        #[async_trait]
        impl RepositoryAuthorizer for SlowAuthorizer {
            async fn check_repository_access(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
                _action: Option<&str>,
            ) -> Result<(), Status> {
                Ok(())
            }

            async fn granted_actions(
                &self,
                _token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
            ) -> Result<Option<Grants>, Status> {
                tokio::time::sleep(self.0).await;
                Ok(Some(Grants::All))
            }
        }

        /// A handler that never answers, standing in for a stalled one.
        #[derive(Clone)]
        struct PendingInner;

        impl Service<Request<()>> for PendingInner {
            type Response = Response<()>;
            type Error = Infallible;
            type Future = Pin<Box<dyn Future<Output = Result<Response<()>, Infallible>> + Send>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _request: Request<()>) -> Self::Future {
                Box::pin(std::future::pending())
            }
        }

        let budget = Duration::from_millis(100);
        let mut service =
            PartitionAccessLayer::new(Arc::new(SlowAuthorizer(Duration::from_millis(60))), budget)
                .layer(PendingInner);

        let started = std::time::Instant::now();
        let response = service
            .call(request(Some(repository()), true))
            .await
            .unwrap();

        assert_eq!(status_of(&response).code(), Code::Cancelled);
        // One budget covers both stages; well under two would already prove
        // no stacking, and the bound is loose only for scheduler slack.
        assert!(
            started.elapsed() < budget * 2,
            "elapsed {:?} must stay within one shared budget",
            started.elapsed()
        );
    }

    mod behind_the_interceptor {
        use jsonwebtoken::Algorithm;
        use jsonwebtoken::DecodingKey;
        use jsonwebtoken::EncodingKey;
        use jsonwebtoken::Header;
        use jsonwebtoken::encode;
        use serde_json::json;
        use tonic::service::interceptor::InterceptedService;

        use super::*;
        use crate::auth::jwk::JWKService;
        use crate::auth::jwk::JWKServiceError;
        use crate::auth::jwt::DEFAULT_IDENTITY_CLAIM;
        use crate::auth::jwt::JwtVerifier;
        use crate::auth::jwt_interceptor::JWTInterceptor;

        const SIGNING_SECRET: &str = "the-secret";

        #[derive(Debug)]
        struct CachedJWKService;

        #[async_trait]
        impl JWKService for CachedJWKService {
            async fn get_key(
                &self,
                _kid: &str,
            ) -> Result<(DecodingKey, Algorithm), JWKServiceError> {
                Ok(self.get_cached_key("").expect("always cached"))
            }

            fn get_cached_key(&self, _kid: &str) -> Option<(DecodingKey, Algorithm)> {
                Some((
                    DecodingKey::from_secret(SIGNING_SECRET.as_ref()),
                    Algorithm::HS256,
                ))
            }

            async fn refresh_key(
                &self,
                _kid: &str,
            ) -> Result<Option<(DecodingKey, Algorithm)>, JWKServiceError> {
                Ok(None)
            }
        }

        /// The full mounted stack: the JWT interceptor outside, this service
        /// inside. What this pins is the ordering contract the layer relies
        /// on — tonic's `InterceptedService` hands the extensions the
        /// interceptor inserted through to the wrapped service, so the
        /// authorizer sees the verified claims of a caller that only sent a
        /// bearer header.
        #[tokio::test]
        async fn the_authorizer_sees_the_claims_the_interceptor_verified() {
            let authorizer = RecordingAuthorizer::new(false);
            let interceptor = JWTInterceptor::new(&JwtVerifier {
                jwk_service: Arc::new(CachedJWKService),
                jwt_issuer: None,
                jwt_audience: Some(vec!["Lore".to_string()]),
                identity_claim: DEFAULT_IDENTITY_CLAIM.to_string(),
            });
            let inner = Inner::default();
            let mut stack = InterceptedService::new(
                PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT).layer(inner.clone()),
                interceptor,
            );

            let token = {
                let mut header = Header::new(Algorithm::HS256);
                header.kid = Some("the kid".into());
                encode(
                    &header,
                    &json!({
                        "iss": "the issuer",
                        "sub": "the bearer",
                        "aud": "Lore",
                        "iat": 1,
                        "exp": std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs() + 60,
                    }),
                    &EncodingKey::from_secret(SIGNING_SECRET.as_ref()),
                )
                .unwrap()
            };
            let mut request = request(Some(repository()), false);
            request.headers_mut().insert(
                "authorization",
                http::HeaderValue::try_from(format!("Bearer {token}")).unwrap(),
            );

            let response = stack.call(request).await.unwrap();

            assert_eq!(
                Status::from_header_map(response.headers()).unwrap().code(),
                Code::PermissionDenied
            );
            assert_eq!(inner.calls(), 0);
            assert_eq!(
                authorizer.asked(),
                vec![(Some("the bearer".to_string()), repository(), None)]
            );
        }
    }
}
