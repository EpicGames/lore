// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_proto::lore::repository::v1::RepositoryGetRequest;
use lore_proto::lore::repository::v1::RepositoryGetResponse;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use crate::auth::jwt::JwtVerifier;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::repository::v1::repository_get::repository_get_implementation;

/// Handler that takes a `RepositoryGet` request forwarded on from peer's `RepositoryService`
/// and executes it, returning the result to the other server for forwarding on to its
/// client.
///
/// The internal endpoint runs no JWT interceptor, so this handler verifies the
/// forwarded end-user token itself and inserts the same extensions the public
/// interceptor would, letting the injected [`RepositoryAuthorizer`] answer the
/// access check. A token that is absent or fails verification does not get
/// inserted, and the authorizer then denies the checks. The caller sees
/// `RepositoryNotFound`.
#[tracing::instrument(name = "ForwardedRepository::v1::RepositoryGet::Handler", skip_all)]
pub async fn handler(
    request: Request<RepositoryGetRequest>,
    jwt_verifier: Option<JwtVerifier>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryGetResponse>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;
    let (_, mut extensions, req) = request.into_parts();

    if let Some(verifier) = &jwt_verifier
        && let Some(raw) = caller_context
            .authorization
            .as_deref()
            .and_then(|header| header.strip_prefix("Bearer "))
    {
        match verifier.verify_token(raw).await {
            Ok(claims) => {
                extensions.insert(RawToken(raw.to_string()));
                extensions.insert(claims);
            }
            Err(err) => {
                debug!(error = ?err, "Forwarded token failed verification");
            }
        }
    }

    repository_get_implementation(
        req,
        caller_context,
        authorizer,
        extensions,
        immutable_store,
        mutable_store,
    )
    .await
}

#[cfg(test)]
mod test {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_proto::lore::repository::v1::repository_get_request::Query;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use crate::authnz::repository_authorizer::VerifiedToken;
    use crate::store::test_store_create;

    async fn store_repository(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        id: RepositoryId,
        name: &str,
    ) {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            id,
        ));
        let metadata = lore_revision::repository::RepositoryMetadata {
            name: name.to_string(),
            description: "a description".into(),
            default_branch: Context::from(uuid::Uuid::now_v7()),
            default_branch_name: "main".into(),
            creator: "alice".into(),
            created: 12345,
        };
        let metadata_hash =
            lore_revision::repository::metadata_store(repository.clone(), metadata.clone())
                .await
                .expect("Failed to store repository metadata");
        lore_revision::repository::metadata_store_hash(repository.clone(), metadata_hash)
            .await
            .expect("Failed to store repository metadata hash");
        lore_revision::repository::store_name_to_id(repository, name, id)
            .await
            .expect("Failed to store repository name to id mapping");
    }

    fn make_forwarded_request(
        query: Query,
        authorization: Option<String>,
    ) -> Request<RepositoryGetRequest> {
        CallerContext {
            repository_id: RepositoryId::default(),
            user_id: "alice".into(),
            correlation_id: String::new(),
            authorization,
        }
        .to_forwarded_request(RepositoryGetRequest { query: Some(query) })
        .expect("CallerContext::to_forwarded_request failed in test")
    }

    #[tokio::test]
    async fn missing_user_id_returns_internal_error() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            // Deliberately omit on-behalf-of-user-id to test the missing-field error
            let request = Request::new(RepositoryGetRequest {
                query: Some(Query::Name("my-repo".into())),
            });

            let err = handler(
                request,
                None, /* no verifier */
                Arc::new(AllowAllRepositoryAuthorizer),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("missing user id should fail");

            assert_eq!(err.code(), tonic::Code::Internal);
            assert!(err.message().contains("on-behalf-of-user-id"));
        }))
        .await;
    }

    // Happy and unhappy paths verify that whatever the underlying
    // `repository_get_implementation` returns is forwarded on correctly.
    mod base_repository_get_handler {
        use super::*;

        #[tokio::test]
        async fn get_by_name_returns_full_repository_record() {
            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                store_repository(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    id,
                    "my-repo",
                )
                .await;

                let response = handler(
                    make_forwarded_request(Query::Name("my-repo".into()), None),
                    None, /* no verifier */
                    Arc::new(AllowAllRepositoryAuthorizer),
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect("Request failed");

                let repository = response
                    .into_inner()
                    .repository
                    .expect("response should include Repository");
                assert_eq!(repository.name, "my-repo");
                assert_eq!(repository.creator, "alice");
                assert_eq!(repository.id, bytes::Bytes::from(id));
            }))
            .await;
        }

        #[tokio::test]
        async fn get_unknown_id_returns_not_found() {
            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let err = handler(
                    make_forwarded_request(Query::Id(id.into()), None),
                    None, /* no verifier */
                    Arc::new(AllowAllRepositoryAuthorizer),
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect_err("unknown id should fail");

                assert_eq!(err.code(), tonic::Code::NotFound);
            }))
            .await;
        }

        /// A denied forwarded get answers `RepositoryNotFound`, matching the
        /// public handlers.
        #[tokio::test]
        async fn denied_get_answers_not_found_for_an_existing_repository() {
            struct DenyAllRepositoryAuthorizer;

            #[tonic::async_trait]
            impl crate::authnz::repository_authorizer::RepositoryAuthorizer for DenyAllRepositoryAuthorizer {
                async fn check_repository_access(
                    &self,
                    _token: Option<&VerifiedToken<'_>>,
                    _repository_id: lore_base::types::RepositoryId,
                    _action: Option<&str>,
                ) -> Result<(), Status> {
                    Err(Status::permission_denied("denied"))
                }
            }

            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                store_repository(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    id,
                    "my-repo",
                )
                .await;

                let err = handler(
                    make_forwarded_request(Query::Name("my-repo".into()), None),
                    None, /* no verifier */
                    Arc::new(DenyAllRepositoryAuthorizer),
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect_err("denied get must fail");

                assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");
            }))
            .await;
        }
    }

    /// The handler stands in for the interceptor on the internal endpoint:
    /// the forwarded `on-behalf-of-authorization` token must reach the
    /// authorizer verified, and an unverifiable one must reach it as no
    /// token at all.
    mod token_verification {
        use std::ops::Add;
        use std::sync::Mutex;
        use std::time::Duration;
        use std::time::SystemTime;
        use std::time::UNIX_EPOCH;

        use async_trait::async_trait;
        use jsonwebtoken::Algorithm;
        use jsonwebtoken::DecodingKey;
        use jsonwebtoken::EncodingKey;
        use jsonwebtoken::Header;
        use jsonwebtoken::encode;

        use super::*;
        use crate::auth::jwk::JWKService;
        use crate::auth::jwk::JWKServiceError;
        use crate::auth::jwt::AuthorizationToken;
        use crate::auth::jwt::DEFAULT_IDENTITY_CLAIM;

        const ALGORITHM: Algorithm = Algorithm::HS256;
        const SIGNING_SECRET: &str = "forwarded-get-test-secret";
        const TEST_AUDIENCE: &str = "lore-test";

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

        fn make_verifier() -> JwtVerifier {
            let mut service = MockTestJWKService::new();
            service
                .expect_get_key()
                .returning(|_| Ok((DecodingKey::from_secret(SIGNING_SECRET.as_ref()), ALGORITHM)));
            JwtVerifier {
                jwk_service: Arc::new(service),
                jwt_issuer: None,
                jwt_audience: Some(vec![TEST_AUDIENCE.to_string()]),
                identity_claim: DEFAULT_IDENTITY_CLAIM.to_string(),
            }
        }

        fn make_jwt() -> String {
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
                ..AuthorizationToken::default()
            };
            let key = EncodingKey::from_secret(SIGNING_SECRET.as_ref());
            let mut header = Header::new(ALGORITHM);
            header.kid = Some("test-kid".to_string());
            encode(&header, &claims, &key).unwrap()
        }

        /// Permits, recording the `(raw, user_id)` pair of the token it was
        /// handed, or `None` when it was handed no token.
        #[derive(Default)]
        struct RecordingPermitAuthorizer {
            seen: Mutex<Vec<Option<(String, String)>>>,
        }

        #[tonic::async_trait]
        impl RepositoryAuthorizer for RecordingPermitAuthorizer {
            async fn check_repository_access(
                &self,
                token: Option<&VerifiedToken<'_>>,
                _repository_id: lore_base::types::RepositoryId,
                _action: Option<&str>,
            ) -> Result<(), Status> {
                self.seen
                    .lock()
                    .unwrap()
                    .push(token.map(|token| (token.raw.to_string(), token.claims.user_id.clone())));
                Ok(())
            }
        }

        #[tokio::test]
        async fn forwarded_token_reaches_the_authorizer_verified() {
            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let authorizer = Arc::new(RecordingPermitAuthorizer::default());

            let jwt = make_jwt();
            let request = make_forwarded_request(
                Query::Name("my-repo".into()),
                Some(format!("Bearer {jwt}")),
            );

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                store_repository(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    id,
                    "my-repo",
                )
                .await;

                handler(
                    request,
                    Some(make_verifier()),
                    authorizer.clone(),
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect("Request failed");

                let seen = authorizer.seen.lock().unwrap();
                assert_eq!(
                    seen.as_slice(),
                    [Some((jwt, "test-user".to_string()))],
                    "the authorizer must receive the forwarded token, verified"
                );
            }))
            .await;
        }

        #[tokio::test]
        async fn unverifiable_forwarded_token_reaches_the_authorizer_as_no_token() {
            let id = random::<RepositoryId>();
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");
            let authorizer = Arc::new(RecordingPermitAuthorizer::default());

            let request = make_forwarded_request(
                Query::Name("my-repo".into()),
                Some("Bearer not-a-jwt".to_string()),
            );

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                store_repository(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    id,
                    "my-repo",
                )
                .await;

                handler(
                    request,
                    Some(make_verifier()),
                    authorizer.clone(),
                    immutable_store,
                    mutable_store,
                )
                .await
                .expect("the permitting authorizer decides, not the handler");

                let seen = authorizer.seen.lock().unwrap();
                assert_eq!(
                    seen.as_slice(),
                    [None],
                    "an unverifiable token must not reach the authorizer as verified"
                );
            }))
            .await;
        }
    }
}
