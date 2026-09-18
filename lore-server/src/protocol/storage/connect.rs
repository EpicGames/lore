// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_revision::lore::RepositoryId;
use lore_telemetry::tracing::fields::USER_ID;
use tracing::debug;
use tracing::warn;

use crate::auth::jwt::JwtVerifier;
use crate::authnz::repository_authorizer::PartitionGrants;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::util::get_user_id_from_token;

#[derive(Clone, Debug, PartialEq)]
pub struct Connect {
    pub repository: RepositoryId,
    pub auth_token: Option<String>,
}

impl Connect {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        if bytes.len() < size_of::<RepositoryId>() {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let mut bytes = bytes;
        let context = bytes.split_to(size_of::<RepositoryId>()).into();

        let auth_token: Option<String> = if !bytes.is_empty() {
            String::from_utf8(bytes.to_vec()).ok()
        } else {
            None
        };

        Ok(Self {
            repository: context,
            auth_token,
        })
    }
}

#[async_trait]
impl Message for Connect {
    #[tracing::instrument(name = "Connect::handle_auth", skip_all)]
    async fn handle_auth(
        &self,
        context: Arc<AttributeMap>,
        jwt_verifier: Arc<Option<JwtVerifier>>,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> Result<LoreResponse, MessageHandleError> {
        // Make sure a correlation ID exists
        if context.get::<CorrelationId>().is_none() {
            warn!("Connection is missing correlation ID");
            let correlation_id = CorrelationId::default();

            if let Some(span) = context.get::<tracing::Span>() {
                span.record("correlation_id", correlation_id.to_string());
            }

            context.insert(correlation_id);
        }

        if let Some(span) = context.get::<tracing::Span>() {
            span.record("repository_id", self.repository.to_string());
        }

        debug!("Handling connect request");

        if let Some(jwt_verifier) = jwt_verifier.as_ref() {
            match self.auth_token.as_ref() {
                Some(auth_token) => {
                    let authorization = jwt_verifier
                        .verify_token(auth_token)
                        .await
                        .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;
                    let token = VerifiedToken {
                        raw: auth_token,
                        claims: &authorization,
                    };
                    let grants = repository_authorizer
                        .granted_access(Some(&token), self.repository)
                        .await
                        .map_err(|status| {
                            MessageHandleError::AuthorizationFailure(status.message().to_string())
                        })?;
                    if let Some(grants) = grants {
                        context.insert(PartitionGrants {
                            repository_id: self.repository,
                            grants,
                        });
                    }
                    // Both halves of the verified token, so a later command's
                    // check (copy's source) can rebuild a `VerifiedToken`.
                    context.insert(RawToken(auth_token.clone()));
                    context.insert(authorization.clone());
                    if let Some(span) = context.get::<tracing::Span>() {
                        span.record(USER_ID, get_user_id_from_token(Some(authorization)));
                    }
                }
                None => {
                    return Err(MessageHandleError::MissingToken);
                }
            }
        }

        if let Some(id) = context.get::<RepositoryId>() {
            if *id != self.repository {
                warn!("Attempted to set repository id for connection, but it was already set!");
                Err(MessageHandleError::AlreadyConnected)
            } else {
                Ok(LoreResponse::Connect(ConnectResponse::default()))
            }
        } else {
            context.insert(self.repository);
            Ok(LoreResponse::Connect(ConnectResponse::default()))
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ConnectResponse {}

impl Response for ConnectResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;

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

        use super::*;
        use crate::auth::jwk::JWKService;
        use crate::auth::jwk::JWKServiceError;
        use crate::auth::jwt::AuthorizationToken;
        use crate::auth::jwt::DEFAULT_IDENTITY_CLAIM;
        use crate::auth::jwt::JwtVerifier;
        use crate::auth::jwt::ResourcePermission;
        use crate::authnz::repository_authorizer::AuthClientAuthorizer;
        use crate::authnz::repository_authorizer::PartitionGrants;

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
}
