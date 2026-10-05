// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_revision::lore::RepositoryId;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::storage::connect::*;
use lore_server::protocol::storage::messages::LoreResponse;
use lore_server::protocol::storage::messages::Message;
use lore_server::protocol::storage::messages::MessageHandleError;
use rand::random;
use zerocopy::IntoBytes;

#[test]
fn test_parse() {
    let repository = random::<RepositoryId>();
    let auth_token: String = "my_auth_token".to_string();

    let message = Connect {
        repository,
        auth_token: Some(auth_token.clone()),
    };

    let mut message_bytes = bytes::BytesMut::new();
    message_bytes.extend_from_slice(repository.as_bytes());
    message_bytes.extend_from_slice(auth_token.as_bytes());

    assert_eq!(Connect::parse(message_bytes.freeze()), Ok(message));
}

#[tokio::test]
async fn test_handle() {
    let repository = random::<RepositoryId>();

    let message = Connect {
        repository,
        auth_token: None,
    };

    let context = Arc::new(AttributeMap::default());

    assert_eq!(
        LoreResponse::Connect(ConnectResponse::default()),
        message
            .handle_auth(
                context.clone(),
                Arc::new(None),
                Arc::new(AllowAllRepositoryAuthorizer),
            )
            .await
            .unwrap()
    );

    assert_eq!(repository, *context.get::<RepositoryId>().unwrap());
}

#[test]
fn test_set_repository_not_enough_bytes() {
    let hash = random::<[u8; 12]>();
    let bytes = Bytes::copy_from_slice(hash.as_bytes());
    Connect::parse(bytes)
        .expect_err("Should have failed to parse, provided repo hash was not long enough");
}

#[tokio::test]
async fn test_set_repository_already_set() {
    let message = Connect {
        repository: random::<RepositoryId>(),
        auth_token: None,
    };

    let context = Arc::new(AttributeMap::default());
    context.insert(random::<RepositoryId>());

    assert!(matches!(
        message
            .handle_auth(
                context,
                Arc::new(None),
                Arc::new(AllowAllRepositoryAuthorizer)
            )
            .await
            .expect_err("expected error"),
        MessageHandleError::AlreadyConnected,
    ));
}

#[tokio::test]
async fn test_set_repository_already_set_value_matched() {
    let repository = random::<RepositoryId>();

    let context = Arc::new(AttributeMap::default());
    context.insert(repository);

    let message = Connect {
        repository,
        auth_token: None,
    };

    assert_eq!(
        LoreResponse::Connect(ConnectResponse::default()),
        message
            .handle_auth(
                context,
                Arc::new(None),
                Arc::new(AllowAllRepositoryAuthorizer)
            )
            .await
            .unwrap()
    );
}

/// A reconnect naming a different repository is refused, and refusing it must
/// leave the connection's existing identity alone.
#[tokio::test]
async fn a_rejected_reconnect_leaves_the_existing_token_alone() {
    let established = random::<RepositoryId>();
    let context = Arc::new(AttributeMap::default());
    context.insert(established);
    let original = AuthorizationToken {
        user_id: "original".to_string(),
        ..Default::default()
    };
    context.insert(original);

    let message = Connect {
        repository: random::<RepositoryId>(),
        auth_token: Some("would-be-verified".to_string()),
    };
    let result = message
        .handle_auth(
            context.clone(),
            Arc::new(None),
            Arc::new(AllowAllRepositoryAuthorizer),
        )
        .await;

    assert!(matches!(result, Err(MessageHandleError::AlreadyConnected)));
    assert_eq!(
        context
            .get::<AuthorizationToken>()
            .expect("token should survive a rejected reconnect")
            .user_id,
        "original",
    );
    assert_eq!(
        *context.get::<RepositoryId>().expect("repository"),
        established
    );
}

mod authorized_connect {
    use std::ops::Add;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use jsonwebtoken::Algorithm;
    use jsonwebtoken::DecodingKey;
    use jsonwebtoken::EncodingKey;
    use jsonwebtoken::Header;
    use jsonwebtoken::encode;
    use lore_server::auth::jwk::JWKService;
    use lore_server::auth::jwk::JWKServiceError;
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::auth::jwt::DEFAULT_IDENTITY_CLAIM;
    use lore_server::auth::jwt::JwtVerifier;
    use lore_server::auth::jwt::ResourcePermission;
    use lore_server::authnz::repository_authorizer::AuthClientAuthorizer;
    use lore_server::authnz::repository_authorizer::PartitionGrants;

    use super::*;

    const ALGORITHM: Algorithm = Algorithm::HS256;
    const SIGNING_SECRET: &str = "connect-test-secret";
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

    fn verifier() -> Arc<Option<JwtVerifier>> {
        let mut jwk_service = MockTestJWKService::new();
        jwk_service
            .expect_get_key()
            .returning(|_| Ok((DecodingKey::from_secret(SIGNING_SECRET.as_ref()), ALGORITHM)));
        Arc::new(Some(JwtVerifier {
            jwk_service: Arc::new(jwk_service),
            jwt_issuer: None,
            jwt_audience: Some(vec![TEST_AUDIENCE.to_string()]),
            jwt_typ: None,
            identity_claim: DEFAULT_IDENTITY_CLAIM.to_string(),
        }))
    }

    fn signed_token(resource_id: &str, permissions: &[&str]) -> String {
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
            resources: Some(vec![ResourcePermission {
                resource_id: resource_id.to_string(),
                permission: permissions.iter().map(ToString::to_string).collect(),
            }]),
            ..Default::default()
        };
        let mut header = Header::new(ALGORITHM);
        header.kid = Some("test-kid".to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_secret(SIGNING_SECRET.as_ref()),
        )
        .unwrap()
    }

    fn legacy_authorizer() -> Arc<dyn RepositoryAuthorizer> {
        Arc::new(AuthClientAuthorizer::new(
            "https://auth.invalid".to_string(),
        ))
    }

    /// A granted connect exposes the enumerated grants and the token
    /// halves in the connection context, for later per-action and
    /// copy-source checks.
    #[tokio::test]
    async fn granted_connect_exposes_grants_and_token() {
        let repository = random::<RepositoryId>();
        let token = signed_token(&format!("urc-{repository}"), &["read", "migrate"]);
        let message = Connect {
            repository,
            auth_token: Some(token.clone()),
        };
        let context = Arc::new(AttributeMap::default());

        message
            .handle_auth(context.clone(), verifier(), legacy_authorizer())
            .await
            .unwrap();

        let grants = context
            .get::<PartitionGrants>()
            .expect("enumerated grants must be exposed");
        assert_eq!(grants.repository_id, repository);
        assert!(grants.grants.permits("migrate"));
        assert!(!grants.grants.permits("obliterate"));
        assert_eq!(context.get::<RawToken>().unwrap().0, token);
        assert!(context.get::<AuthorizationToken>().is_some());
    }

    #[tokio::test]
    async fn ungranted_connect_is_refused() {
        let message = Connect {
            repository: random::<RepositoryId>(),
            auth_token: Some(signed_token("urc-somewhere-else", &["read"])),
        };
        let context = Arc::new(AttributeMap::default());

        let err = message
            .handle_auth(context.clone(), verifier(), legacy_authorizer())
            .await
            .expect_err("a token granting another partition must be refused");
        assert!(matches!(err, MessageHandleError::AuthorizationFailure(_)));
        assert!(context.get::<RepositoryId>().is_none());
        assert!(context.get::<PartitionGrants>().is_none());
    }
}
