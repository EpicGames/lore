// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_proto::ObliterateRequest;
use lore_proto::ObliterateResponse;
use lore_revision::notification::NotificationSender;
use lore_storage::StoreObliterateStats;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::metadata::MetadataMap;
use tracing::info;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::JwtVerifier;
use crate::auth::jwt_interceptor::extract_bearer_token;
use crate::grpc::can_obliterate;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::hook_error_to_status;
use crate::grpc::warn_mapped_error_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

async fn authenticate_request(
    metadata: &MetadataMap,
    jwt_verifier: &JwtVerifier,
) -> Result<AuthorizationToken, Status> {
    let token = extract_bearer_token(metadata)
        .ok_or_else(|| Status::unauthenticated("authorization header required"))?;

    jwt_verifier
        .verify_token(&token)
        .await
        .map_err(|e| Status::unauthenticated(format!("invalid token ({e:?})")))
}

#[allow(clippy::todo)]
#[tracing::instrument(name = "Obliterate::handle", skip_all)]
pub async fn handler(
    mut request: Request<ObliterateRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    _mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    jwt_verifier: &Arc<Option<JwtVerifier>>,
) -> Result<Response<ObliterateResponse>, Status> {
    if let Some(verifier) = &**jwt_verifier {
        let authorization = authenticate_request(request.metadata(), verifier).await?;
        request.extensions_mut().insert(authorization);
    }

    let repository = get_repository(request.metadata())?;
    let extensions = request.extensions().clone();
    let user_id = get_user_id(&extensions);
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let address = Address::from(req.address.unwrap_or_default());

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(execution, async move {
            if jwt_verifier.is_some() && !can_obliterate(&extensions, repository) {
                warn!("Attempt to obliterate {address} in repository, but user does not have the correct permissions");
                return Err(Status::permission_denied("Permission denied"));
            }

            let hook_ctx = HookContext::builder()
                .correlation_id(correlation_id)
                .hook_point(HookPoint::Obliterate)
                .repository(repository)
                .user(user_id)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::Obliterate, &hook_ctx)
                .map_err(|error| {
                    let source_error = error.clone();
                    let response = hook_error_to_status(error);
                    warn_mapped_error_status(&source_error, &response);
                    response
                })?;

            info!("Handling obliterate request for address {address}");

            let stats = Arc::new(StoreObliterateStats::default());
            immutable_store
                .obliterate(repository, address, stats.clone())
                .await
                .map_err(|e| {
                    warn!("Failed to obliterate {address}: {e}");
                    if e.is_address_not_found() {
                        // Distinguish absent from internal failure so the client can map this
                        // back to `AddressNotFound` and treat it as idempotent success.
                        // Without this, every absent obliterate would surface as a generic
                        // Internal error.
                        Status::not_found(format!("Address not found: {address}"))
                    } else {
                        Status::internal(format!("Failed to obliterate {address}: {e}"))
                    }
                })?;

            info!("Successfully obliterated {address}, stats: {stats:?}");
            // TODO(jcohen): track metrics for stats

            notification
                .obliterate(repository, address)
                .await
                .map_err(|e| {
                    warn!("Failed to obliterate address: {address}: {e:?}");
                    Status::internal("Obliterate failed")
                })?;

            hook_dispatcher.spawn_post(HookPoint::Obliterate, hook_ctx);

            Ok(Response::new(ObliterateResponse {}))
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::ops::Add;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use async_trait::async_trait;
    use bytes::Bytes;
    use jsonwebtoken::Algorithm;
    use jsonwebtoken::DecodingKey;
    use jsonwebtoken::EncodingKey;
    use jsonwebtoken::Header;
    use jsonwebtoken::encode;
    use lore_base::types::Context;
    use lore_proto::ObliterateRequest;
    use lore_revision::lore::RepositoryId;
    use lore_storage::Fragment;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use mockall::predicate::eq;
    use rand::random;
    use tonic::Code;
    use tonic::Request;
    use tonic::metadata::MetadataValue;

    use super::*;
    use crate::auth::jwk::JWKService;
    use crate::auth::jwk::JWKServiceError;
    use crate::auth::jwt::ResourcePermission;
    use crate::hooks::HookDispatcher;
    use crate::notification::testing::MockNotificationSender;
    use crate::store::test_store_create;

    const ALGORITHM: Algorithm = Algorithm::HS256;
    const SIGNING_SECRET: &str = "obliterate-test-secret";

    mockall::mock! {
        TestJWKService {}

        #[async_trait]
        impl JWKService for TestJWKService {
            async fn get_key(
                &self,
                kid: &str,
            ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError>;

            fn get_cached_key(
                &self,
                kid: &str,
            ) -> Option<(DecodingKey, jsonwebtoken::Algorithm)>;

            async fn refresh_key(
                &self,
                kid: &str,
            ) -> Result<Option<(DecodingKey, jsonwebtoken::Algorithm)>, JWKServiceError>;
        }
    }

    const TEST_AUDIENCE: &str = "lore-test";

    fn make_verifier(jwk_service: MockTestJWKService) -> JwtVerifier {
        JwtVerifier {
            jwk_service: Arc::new(jwk_service),
            jwt_issuer: None,
            jwt_audience: Some(vec![TEST_AUDIENCE.to_string()]),
        }
    }

    fn make_jwt(resources: Option<Vec<ResourcePermission>>) -> String {
        let claims = AuthorizationToken {
            user_id: "test-user".to_string(),
            issuer: "test-issuer".to_string(),
            issued_at: 1,
            expires: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .add(Duration::from_secs(60))
                .as_secs(),
            audience: vec![TEST_AUDIENCE.to_string()],
            env: "test".to_string(),
            name: "test".to_string(),
            preferred_username: "test".to_string(),
            resources,
            groups: None,
            is_service_account: Some(false),
            idp: "test".to_string(),
        };
        let key = EncodingKey::from_secret(SIGNING_SECRET.as_ref());
        let mut header = Header::new(ALGORITHM);
        header.kid = Some("test-kid".to_string());
        encode(&header, &claims, &key).unwrap()
    }

    fn good_key_service() -> MockTestJWKService {
        let mut service = MockTestJWKService::new();
        service
            .expect_get_key()
            .returning(|_| Ok((DecodingKey::from_secret(SIGNING_SECRET.as_ref()), ALGORITHM)));
        service
    }

    fn bad_key_service() -> MockTestJWKService {
        let mut service = MockTestJWKService::new();
        service
            .expect_get_key()
            .returning(|_| Err(JWKServiceError::NotFound));
        service
    }

    fn make_request(
        repository: RepositoryId,
        auth_header: Option<String>,
    ) -> Request<ObliterateRequest> {
        let mut request = Request::new(ObliterateRequest { address: None });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        if let Some(token) = auth_header {
            let value: MetadataValue<tonic::metadata::Ascii> =
                format!("Bearer {token}").parse().unwrap();
            request.metadata_mut().insert("authorization", value);
        }
        request
    }

    #[tokio::test]
    async fn proceeds_without_auth_when_no_verifier_configured() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, _) = test_store_create().await.unwrap();
        let mut notification = MockNotificationSender::new();
        notification.expect_obliterate().never();
        let notification = Arc::new(notification);
        let hook_dispatcher = HookDispatcher::empty();

        let err = handler(
            make_request(repository, None),
            immutable_store,
            mutable_store,
            notification,
            &hook_dispatcher,
            &None.into(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn returns_unauthenticated_when_authorization_header_absent() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, _) = test_store_create().await.unwrap();
        let notification = Arc::new(MockNotificationSender::new());
        let hook_dispatcher = HookDispatcher::empty();
        // Key service not expected to be called — fail fast if it is
        let verifier = make_verifier(MockTestJWKService::new());

        let err = handler(
            make_request(repository, None),
            immutable_store,
            mutable_store,
            notification,
            &hook_dispatcher,
            &Some(verifier).into(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.code(), Code::Unauthenticated);
    }

    #[tokio::test]
    async fn returns_unauthenticated_for_unverifiable_token() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, _) = test_store_create().await.unwrap();
        let notification = Arc::new(MockNotificationSender::new());
        let hook_dispatcher = HookDispatcher::empty();
        let verifier = make_verifier(bad_key_service());

        let err = handler(
            make_request(repository, Some(make_jwt(None))),
            immutable_store,
            mutable_store,
            notification,
            &hook_dispatcher,
            &Some(verifier).into(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.code(), Code::Unauthenticated);
    }

    #[tokio::test]
    async fn returns_permission_denied_when_user_lacks_obliterate_permission() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, _) = test_store_create().await.unwrap();
        let notification = Arc::new(MockNotificationSender::new());
        let hook_dispatcher = HookDispatcher::empty();
        let verifier = make_verifier(good_key_service());
        // Token has resources for this repository but without the 'obliterate' permission
        let resources = vec![ResourcePermission {
            resource_id: format!("urc-{repository}"),
            permission: vec!["read".to_string(), "write".to_string()],
        }];

        let err = handler(
            make_request(repository, Some(make_jwt(Some(resources)))),
            immutable_store,
            mutable_store,
            notification,
            &hook_dispatcher,
            &Some(verifier).into(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.code(), Code::PermissionDenied);
    }

    #[tokio::test]
    async fn returns_not_found_when_authorized_and_address_absent() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, _) = test_store_create().await.unwrap();
        let mut notification = MockNotificationSender::new();
        // The obliterate notification must not fire: the store returns not_found before we get there
        notification.expect_obliterate().never();
        let notification = Arc::new(notification);
        let hook_dispatcher = HookDispatcher::empty();
        let verifier = make_verifier(good_key_service());
        let resources = vec![ResourcePermission {
            resource_id: format!("urc-{repository}"),
            permission: vec!["obliterate".to_string()],
        }];

        let err = handler(
            make_request(repository, Some(make_jwt(Some(resources)))),
            immutable_store,
            mutable_store,
            notification,
            &hook_dispatcher,
            &Some(verifier).into(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn succeeds_for_authorized_request_with_existing_address() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, _) = test_store_create().await.unwrap();

        // Write a fragment so the obliterate has something to act on
        let context: Context = rand::random();
        let payload = Bytes::from_static(b"test payload");
        let hash = lore_storage::hash::hash_slice(&payload);
        let address = Address { hash, context };
        immutable_store
            .clone()
            .put(
                repository,
                address,
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(payload),
                false,
            )
            .await
            .unwrap();

        // Confirm the fragment is readable before obliterating it
        immutable_store
            .clone()
            .get(repository, address, lore_storage::StoreMatch::MatchFull)
            .await
            .expect("address should be present before obliterate");

        let mut notification = MockNotificationSender::new();
        notification
            .expect_obliterate()
            .with(eq(repository), eq(address))
            .return_once(|_, _| Ok(()));
        let notification = Arc::new(notification);
        let hook_dispatcher = HookDispatcher::empty();
        let verifier = make_verifier(good_key_service());
        let resources = vec![ResourcePermission {
            resource_id: format!("urc-{repository}"),
            permission: vec!["obliterate".to_string()],
        }];

        let mut request = Request::new(ObliterateRequest {
            address: Some(address.into()),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        let value: MetadataValue<tonic::metadata::Ascii> =
            format!("Bearer {}", make_jwt(Some(resources)))
                .parse()
                .unwrap();
        request.metadata_mut().insert("authorization", value);

        handler(
            request,
            immutable_store.clone(),
            mutable_store,
            notification,
            &hook_dispatcher,
            &Some(verifier).into(),
        )
        .await
        .expect("handler should succeed");

        let get_err = immutable_store
            .get(repository, address, lore_storage::StoreMatch::MatchFull)
            .await
            .unwrap_err();
        assert!(
            get_err.is_payload_not_found(),
            "payload should be obliterated; got: {get_err:?}"
        );
    }
}
