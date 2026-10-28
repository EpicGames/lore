// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::convert::Infallible;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use http::HeaderValue;
use http::Request;
use http::Response;
use lore_base::types::Context as LoreContext;
use lore_revision::lore::RepositoryId;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::Grants;
use lore_server::authnz::repository_authorizer::PartitionGrants;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::no_repository_access_status;
use lore_server::grpc::tower::partition_access::*;
use lore_transport::grpc::PARTITION_ID_KEY;
use tonic::Code;
use tonic::Status;
use tonic::metadata::MetadataMap;
use tonic::metadata::MetadataValue;
use tower::Layer;
use tower::Service;

/// One recorded authorizer question: the token's subject, the
/// partition, and the action, or [`ENUMERATE`] for an enumeration.
type Asked = (Option<String>, RepositoryId, String);

const ENUMERATE: &str = "<enumerate>";

/// Records every question it is asked and answers with a fixed verdict:
/// `read` granted, or the partition denied.
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
        action: Action,
    ) -> Result<(), Status> {
        self.asked.lock().unwrap().push((
            token.map(|token| token.claims.user_id.clone()),
            repository_id,
            action.to_string(),
        ));
        if self.permit {
            Ok(())
        } else {
            Err(Status::permission_denied("no grant"))
        }
    }

    async fn granted_actions(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
    ) -> Result<Option<Grants>, Status> {
        self.asked.lock().unwrap().push((
            token.map(|token| token.claims.user_id.clone()),
            repository_id,
            ENUMERATE.to_string(),
        ));
        Ok(Some(if self.permit {
            Grants::Actions([Action::Read.to_string()].into())
        } else {
            Grants::Denied
        }))
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
        vec![(
            Some("the u".to_string()),
            repository(),
            ENUMERATE.to_string()
        )]
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
    assert_eq!(
        authorizer.asked(),
        vec![(None, repository(), ENUMERATE.to_string())]
    );
}

/// An authorizer that cannot enumerate is asked `read`, the reachability
/// question, and its answer decides; no grants are exposed to the handler.
#[tokio::test]
async fn a_non_enumerable_authorizer_is_asked_read() {
    struct PolicyOnly(bool);

    #[async_trait]
    impl RepositoryAuthorizer for PolicyOnly {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            action: Action,
        ) -> Result<(), Status> {
            assert_eq!(action, Action::Read);
            if self.0 {
                Ok(())
            } else {
                Err(Status::permission_denied("no grant"))
            }
        }
    }

    let inner = GrantsInner::default();
    let mut service =
        PartitionAccessLayer::new(Arc::new(PolicyOnly(true)), TEST_TIMEOUT).layer(inner.clone());
    service
        .call(request(Some(repository()), true))
        .await
        .unwrap();
    assert_eq!(*inner.0.lock().unwrap(), vec![None]);

    let inner = GrantsInner::default();
    let mut service =
        PartitionAccessLayer::new(Arc::new(PolicyOnly(false)), TEST_TIMEOUT).layer(inner.clone());
    let response = service
        .call(request(Some(repository()), true))
        .await
        .unwrap();
    assert_eq!(status_of(&response).code(), Code::PermissionDenied);
    assert!(inner.0.lock().unwrap().is_empty());
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
        _action: Action,
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

/// An anonymous caller (the no-auth server) is exposed whatever the
/// authorizer enumerates for it, like any other caller: allow-all's fixed
/// set holds no action that elevates anyone.
#[tokio::test]
async fn an_anonymous_caller_is_exposed_the_enumerated_grants() {
    let grants = Grants::Actions([Action::Read.to_string()].into());
    let authorizer = EnumeratingAuthorizer::new(grants.clone());
    let inner = GrantsInner::default();
    let mut service =
        PartitionAccessLayer::new(authorizer.clone(), TEST_TIMEOUT).layer(inner.clone());

    service
        .call(request(Some(repository()), false))
        .await
        .unwrap();

    assert_eq!(*inner.0.lock().unwrap(), vec![Some((repository(), grants))]);
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
            _action: Action,
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
    let mut service =
        PartitionAccessLayer::new(Arc::new(FailingAuthorizer), TEST_TIMEOUT).layer(inner.clone());

    let response = service
        .call(request(Some(repository()), true))
        .await
        .unwrap();

    assert_eq!(status_of(&response).code(), Code::PermissionDenied);
    assert!(inner.0.lock().unwrap().is_empty());
}

/// A stalled authorizer cannot park the request: the authorization stage
/// is bounded on its own, and elapsing answers with a server status
/// naming that stage rather than a denial.
#[tokio::test]
async fn a_stalled_authorizer_times_out_with_the_authorization_status() {
    struct StalledAuthorizer;

    #[async_trait]
    impl RepositoryAuthorizer for StalledAuthorizer {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            _action: Action,
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
    assert_eq!(status.message(), "Authorization timeout exceeded");
    assert!(inner.0.lock().unwrap().is_empty());
}

/// The authorization timeout bounds the authorization stage only. An
/// inner service that outlasts it still answers for itself, so neither
/// the receipt of a request body nor the handler's own work is charged
/// to the authorization budget.
#[tokio::test]
async fn the_authorization_timeout_does_not_bound_the_inner_service() {
    struct SlowAuthorizer(Duration);

    #[async_trait]
    impl RepositoryAuthorizer for SlowAuthorizer {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            _action: Action,
        ) -> Result<(), Status> {
            Ok(())
        }

        async fn granted_actions(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
        ) -> Result<Option<Grants>, Status> {
            tokio::time::sleep(self.0).await;
            Ok(Some(Grants::Actions([Action::Read.to_string()].into())))
        }
    }

    /// Answers only after a delay, standing in for a request still
    /// arriving and a handler still working. Marks its response so the
    /// test can tell it apart from one the layer synthesized.
    #[derive(Clone)]
    struct SlowInner(Duration);

    impl Service<Request<()>> for SlowInner {
        type Response = Response<()>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Response<()>, Infallible>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: Request<()>) -> Self::Future {
            let delay = self.0;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let mut response = Response::new(());
                response
                    .headers_mut()
                    .insert("x-inner-answered", HeaderValue::from_static("yes"));
                Ok(response)
            })
        }
    }

    let authorization_timeout = Duration::from_millis(100);
    let mut service = PartitionAccessLayer::new(
        Arc::new(SlowAuthorizer(Duration::from_millis(60))),
        authorization_timeout,
    )
    .layer(SlowInner(authorization_timeout * 3));

    let response = service
        .call(request(Some(repository()), true))
        .await
        .unwrap();

    assert_eq!(
        response
            .headers()
            .get("x-inner-answered")
            .map(HeaderValue::as_bytes),
        Some(b"yes".as_slice()),
    );
}

mod behind_the_interceptor {
    use jsonwebtoken::Algorithm;
    use jsonwebtoken::DecodingKey;
    use jsonwebtoken::EncodingKey;
    use jsonwebtoken::Header;
    use jsonwebtoken::encode;
    use lore_server::auth::jwk::JWKService;
    use lore_server::auth::jwk::JWKServiceError;
    use lore_server::auth::jwt::JwtVerifier;
    use lore_server::auth::jwt_interceptor::JWTInterceptor;
    use serde_json::json;
    use tonic::service::interceptor::InterceptedService;

    use super::*;

    const SIGNING_SECRET: &str = "the-secret";

    #[derive(Debug)]
    struct CachedJWKService;

    #[async_trait]
    impl JWKService for CachedJWKService {
        async fn get_key(&self, _kid: &str) -> Result<(DecodingKey, Algorithm), JWKServiceError> {
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
        let interceptor = JWTInterceptor::new(Some(&JwtVerifier {
            jwk_service: Arc::new(CachedJWKService),
            jwt_issuer: None,
            jwt_audience: Some(vec!["Lore".to_string()]),
            jwt_typ: None,
        }));
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
            vec![(
                Some("the bearer".to_string()),
                repository(),
                ENUMERATE.to_string()
            )]
        );
    }
}
