// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Add;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode as HttpStatusCode;
use axum_test::TestServer;
use jsonwebtoken::Algorithm;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::EncodingKey;
use jsonwebtoken::Header;
use jsonwebtoken::encode;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_revision::fragment;
use lore_server::auth::jwk::JWKService;
use lore_server::auth::jwk::JWKServiceError;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::JwtVerifier;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::AuthClientAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::http::server::ServerHealth;
use lore_server::http::server::ServerState;
use lore_server::http::server::create_router;
use rand::random;
use serde_json::json;

use crate::http::test_utils::presign_config;
use crate::store::test_support::test_store_create;

const ALGORITHM: Algorithm = Algorithm::HS256;
const SIGNING_SECRET: &str = "axum-middleware-test-secret";
const TEST_AUDIENCE: &str = "lore-test";

mockall::mock! {
    TestJWKService {}

    #[async_trait::async_trait]
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

fn verifier() -> JwtVerifier {
    let mut jwk_service = MockTestJWKService::new();
    jwk_service
        .expect_get_key()
        .returning(|_| Ok((DecodingKey::from_secret(SIGNING_SECRET.as_ref()), ALGORITHM)));
    JwtVerifier {
        jwk_service: Arc::new(jwk_service),
        jwt_issuer: None,
        jwt_audience: Some(vec![TEST_AUDIENCE.to_string()]),
        jwt_typ: None,
    }
}

fn bearer(resources: Option<Vec<ResourcePermission>>) -> HeaderValue {
    let claims = AuthorizationToken {
        user_id: "test-user".to_string(),
        issuer: "test-issuer".to_string(),
        issued_at: 1,
        audience: vec![TEST_AUDIENCE.to_string()],
        expires: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .add(Duration::from_secs(60))
            .as_secs(),
        resources,
        ..Default::default()
    };
    let mut header = Header::new(ALGORITHM);
    header.kid = Some("test-kid".to_string());
    let token = encode(
        &header,
        &claims,
        &EncodingKey::from_secret(SIGNING_SECRET.as_ref()),
    )
    .unwrap();
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

/// The middleware asks the shared authorizer, so on the legacy tier an
/// access token's `resources` claim decides in place: a grant for the
/// path's repository passes, a grant for another one answers 403. The
/// authorizer's URL points nowhere, so the network never answered.
#[tokio::test]
async fn the_shared_authorizer_decides_the_repository_path() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();
            immutable_store
                .clone()
                .put(repository.into(), address, fragment, Some(payload), false)
                .await
                .expect("Failed to put data in immutable store");

            let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
                "https://auth.invalid".to_string(),
            ));
            let state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: Some(verifier()),
                repository_authorizer: authorizer,
                max_file_size: 100,
                presign_config: None,
            };
            let health = ServerHealth::new_without_availability(state.immutable_store.clone());
            let server = TestServer::new(create_router(
                state,
                health,
                &LoreHttpServerSettings::test_default(),
            ))
            .unwrap();
            let url = format!("/v1/repository/{repository}/content/{address}");
            let auth_header = HeaderName::from_static("authorization");

            let granted = vec![ResourcePermission {
                resource_id: format!("urc-{repository}"),
                permission: vec![],
            }];
            let response = server
                .get(&url)
                .add_header(auth_header.clone(), bearer(Some(granted)))
                .await;
            assert_eq!(response.status_code(), HttpStatusCode::OK);

            let elsewhere = vec![ResourcePermission {
                resource_id: "urc-00000000000000000000000000000000".to_string(),
                permission: vec![],
            }];
            let response = server
                .get(&url)
                .add_header(auth_header.clone(), bearer(Some(elsewhere)))
                .await;
            assert_eq!(response.status_code(), HttpStatusCode::FORBIDDEN);

            // No bearer at all keeps answering 401, not 403.
            let response = server.get(&url).await;
            assert_eq!(response.status_code(), HttpStatusCode::UNAUTHORIZED);
        })
        .await;
}

/// Any grant on the repository reads its content; `PUT` also needs `write`.
/// The legacy tier answers from the token's `resources` claim, so the
/// authorizer's URL is never reached.
#[tokio::test]
async fn any_grant_reads_and_put_needs_write() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();
            immutable_store
                .clone()
                .put(repository.into(), address, fragment, Some(payload), false)
                .await
                .expect("Failed to put data in immutable store");

            let state = ServerState {
                immutable_store,
                mutable_store,
                jwt_verifier: Some(verifier()),
                repository_authorizer: Arc::new(AuthClientAuthorizer::new(
                    "https://auth.invalid".to_string(),
                )),
                max_file_size: 100,
                presign_config: None,
            };
            let health = ServerHealth::new_without_availability(state.immutable_store.clone());
            let server = TestServer::new(create_router(
                state,
                health,
                &LoreHttpServerSettings::test_default(),
            ))
            .unwrap();
            let auth_header = HeaderName::from_static("authorization");
            let holding = |action: Action| {
                bearer(Some(vec![ResourcePermission {
                    resource_id: format!("urc-{repository}"),
                    permission: vec![action.to_string()],
                }]))
            };

            let get = format!("/v1/repository/{repository}/content/{address}");
            let put = format!("/v1/repository/{repository}/content");
            for (action, get_status, put_status) in [
                (Action::Read, HttpStatusCode::OK, HttpStatusCode::FORBIDDEN),
                (Action::Write, HttpStatusCode::OK, HttpStatusCode::OK),
            ] {
                let response = server
                    .get(&get)
                    .add_header(auth_header.clone(), holding(action))
                    .await;
                assert_eq!(response.status_code(), get_status, "GET with {action}");

                let response = server
                    .put(&put)
                    .add_header(auth_header.clone(), holding(action))
                    .bytes(bytes::Bytes::from_static(b"content"))
                    .await;
                assert_eq!(response.status_code(), put_status, "PUT with {action}");
            }
        })
        .await;
}

/// Vending a presigned URL needs `presign`, which `write` does not imply.
/// Without `[server.auth]` it stays open, since allow-all grants `presign`.
#[tokio::test]
async fn presign_needs_presign() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();
            immutable_store
                .clone()
                .put(repository.into(), address, fragment, Some(payload), false)
                .await
                .expect("Failed to put data in immutable store");
            let server = |jwt_verifier, repository_authorizer| {
                let state = ServerState {
                    immutable_store: immutable_store.clone(),
                    mutable_store: mutable_store.clone(),
                    jwt_verifier,
                    repository_authorizer,
                    max_file_size: 100,
                    presign_config: Some(presign_config()),
                };
                let health = ServerHealth::new_without_availability(immutable_store.clone());
                TestServer::new(create_router(
                    state,
                    health,
                    &LoreHttpServerSettings::test_default(),
                ))
                .unwrap()
            };
            let url = format!("/v1/repository/{repository}/content/{address}/presign");

            let legacy = server(
                Some(verifier()),
                Arc::new(AuthClientAuthorizer::new(
                    "https://auth.invalid".to_string(),
                )),
            );
            let auth_header = HeaderName::from_static("authorization");
            for (action, status) in [
                (Action::Write, HttpStatusCode::FORBIDDEN),
                (Action::Presign, HttpStatusCode::OK),
            ] {
                let token = bearer(Some(vec![ResourcePermission {
                    resource_id: format!("urc-{repository}"),
                    permission: vec![action.to_string()],
                }]));
                let response = legacy
                    .post(&url)
                    .add_header(auth_header.clone(), token)
                    .json(&json!({}))
                    .await;
                assert_eq!(response.status_code(), status, "presign with {action}");
            }

            let no_auth = server(None, Arc::new(AllowAllRepositoryAuthorizer));
            let response = no_auth.post(&url).json(&json!({})).await;
            assert_eq!(response.status_code(), HttpStatusCode::OK);
        })
        .await;
}
