// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use axum::body::Body;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use lore_base::types::Context;
use lore_telemetry::tracing::fields::USER_ID;
use serde::Deserialize;
use tracing::Span;

use crate::auth::jwt::AuthorizationToken;
use crate::authnz::repository_authorizer::PartitionGrants;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::http::server::ServerState;

#[derive(Deserialize)]
pub struct Params {
    repository_id: String,
}

pub async fn jwt_axum_verify_authorization(
    State(state): State<ServerState>,
    Path(params): Path<Params>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(jwt_verifier) = state.jwt_verifier {
        if let Some(accesstoken) = extract_bearer_token(&request) {
            if let Ok(user_info) = jwt_verifier.verify_token(&accesstoken).await {
                let repository: lore_revision::lore::RepositoryId =
                    Context::from_str(params.repository_id.as_str())
                        .unwrap_or_default()
                        .into();
                let token = VerifiedToken {
                    raw: &accesstoken,
                    claims: &user_info,
                };
                if let Ok(grants) = state
                    .repository_authorizer
                    .granted_access(Some(&token), repository)
                    .await
                {
                    Span::current().record(USER_ID, user_info.identity());
                    if let Some(grants) = grants {
                        request.extensions_mut().insert(PartitionGrants {
                            repository_id: repository,
                            grants,
                        });
                    }
                    // Set `user_info` as a request extension so it can be used down the stack
                    request.extensions_mut().insert(RawToken(accesstoken));
                    request.extensions_mut().insert(Some(user_info));

                    return next.run(request).await;
                }
            }
            Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap()
        } else {
            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .unwrap()
        }
    } else {
        let no_user_info: Option<AuthorizationToken> = None;
        request.extensions_mut().insert(no_user_info);
        next.run(request).await
    }
}

fn extract_bearer_token(request: &Request) -> Option<String> {
    request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| {
            if header.starts_with("Bearer ") {
                Some(header.trim_start_matches("Bearer ").to_string())
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
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
    use lore_revision::fragment;
    use rand::random;

    use super::*;
    use crate::auth::jwk::JWKService;
    use crate::auth::jwk::JWKServiceError;
    use crate::auth::jwt::DEFAULT_IDENTITY_CLAIM;
    use crate::auth::jwt::JwtVerifier;
    use crate::auth::jwt::ResourcePermission;
    use crate::authnz::repository_authorizer::AuthClientAuthorizer;
    use crate::authnz::repository_authorizer::RepositoryAuthorizer;
    use crate::http::server::LoreHttpServerSettings;
    use crate::http::server::ServerHealth;
    use crate::http::server::create_router;
    use crate::store::test_store_create;

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
            identity_claim: DEFAULT_IDENTITY_CLAIM.to_string(),
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

                let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(
                    AuthClientAuthorizer::new("https://auth.invalid".to_string()),
                );
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
}
