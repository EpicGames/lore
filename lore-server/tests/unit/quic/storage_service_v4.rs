// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::storage::messages::MessageHandleError;
use lore_server::protocol::storage::session::MAX_CONCURRENT_SESSIONS;
use lore_server::quic::QuicService;
use lore_server::quic::storage_service_v4::*;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use lore_transport::quic::QuicErrorStatus;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use rand::random;

use crate::store::test_support::test_store_create;

fn make_service(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> StorageServiceV4 {
    StorageServiceV4::new(
        Arc::new(None),
        Arc::new(lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer),
        immutable_store.clone(),
        immutable_store.clone(),
        mutable_store,
        Arc::new(UserAgentFilter::default()),
    )
}

fn make_header(cmd: u8) -> CommandHeader {
    CommandHeader {
        cmd,
        ..CommandHeader::default()
    }
}

#[tokio::test]
async fn parse_client_identify_opcode_returns_variant() {
    use lore_transport::quic::storage_service::Command;
    // The stores are unused by parse_request_bytes, so a minimal service suffices.
    let (immutable_store, mutable_store, _exec) =
        test_store_create().await.expect("Failed to create stores");
    let service = make_service(immutable_store, mutable_store);

    let header = make_header(Command::ClientIdentify as u8);
    let payload = Bytes::from("my-client/1.0");

    let parsed = service
        .parse_request_bytes(&header, payload)
        .expect("parsing a ClientIdentify request must succeed");

    assert!(
        matches!(parsed, ParsedStorageRequestV4::ClientIdentify(_)),
        "expected ClientIdentify variant, got {parsed:?}"
    );
}

#[tokio::test]
async fn parse_client_identify_stores_value() {
    use lore_transport::quic::storage_service::Command;
    let (immutable_store, mutable_store, _exec) =
        test_store_create().await.expect("Failed to create stores");
    let service = make_service(immutable_store, mutable_store);

    let header = make_header(Command::ClientIdentify as u8);
    let payload = Bytes::from("my-client/1.0");

    let parsed = service
        .parse_request_bytes(&header, payload)
        .expect("parsing a ClientIdentify request must succeed");
    let ParsedStorageRequestV4::ClientIdentify(ci) = parsed else {
        panic!("wrong variant");
    };
    assert_eq!(ci.user_agent, Some("my-client/1.0".to_string()));
}

#[tokio::test]
async fn run_request_handler_client_identify_returns_empty_ok() {
    let (immutable_store, mutable_store, _exec) =
        test_store_create().await.expect("Failed to create stores");
    let service = make_service(immutable_store, mutable_store);

    let ci = lore_server::protocol::client_identify::ClientIdentify {
        user_agent: Some("my-client/1.0".to_string()),
        is_trusted: false,
    };

    let response = service
        .run_request_handler(
            Arc::new(AttributeMap::default()),
            ParsedStorageRequestV4::ClientIdentify(ci),
        )
        .await
        .expect("ClientIdentify must be handled successfully");

    assert!(response.is_empty(), "expected empty response vec");
}

/// Fill the session map to capacity then attempt one more `AuthorizeStart`,
/// verifying the handler returns `SlowDown` and that `transform_protocol_error`
/// classifies it the same way `stream_handler` would.
#[tokio::test]
async fn authorize_start_returns_slow_down_when_session_limit_reached() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    let service = make_service(immutable_store, mutable_store);

    let repo = random::<lore_revision::lore::RepositoryId>();

    // Fill the session map to capacity via the handler (jwt_verifier is None,
    // so each call goes straight to session_map.start with no I/O).
    for i in 0..MAX_CONCURRENT_SESSIONS {
        let result = service
            .run_request_handler(
                AttributeMap::default().into(),
                ParsedStorageRequestV4::AuthorizeStart {
                    repository: repo,
                    correlation_id: format!("fill-{i}"),
                    auth_token: vec![],
                },
            )
            .await;
        assert!(result.is_ok(), "session {i} should succeed");
    }

    // One more must hit the limit.
    let err = service
        .run_request_handler(
            AttributeMap::default().into(),
            ParsedStorageRequestV4::AuthorizeStart {
                repository: repo,
                correlation_id: "over-limit".into(),
                auth_token: vec![],
            },
        )
        .await
        .expect_err("expected SlowDown when session limit is reached");

    assert!(
        matches!(err, MessageHandleError::SessionLimitReached),
        "expected SessionLimitReached, got {err:?}"
    );

    // Verify stream_handler classification: SlowDown on the wire, not an internal
    // error, and suppressed from logging (same suppression path as SlowDown).
    let error_info = service.transform_protocol_error(&err);
    assert_eq!(
        error_info.response_error_code,
        QuicServiceError::SlowDown as QuicErrorStatus,
    );
    assert_eq!(error_info.message_handle_label, "SessionLimitReached");
    assert!(!error_info.is_internal_error);
    assert!(!error_info.is_appropriate_for_logging);
}

/// QUIC has no code for a rejected argument, so `InvalidArgument` answers the
/// generic `Failed`, is labelled as itself, and is not an internal error.
#[tokio::test]
async fn transform_protocol_error_maps_invalid_argument_to_failed() {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");
    let service = make_service(immutable_store, mutable_store);

    let error_info = service
        .transform_protocol_error(&MessageHandleError::InvalidArgument("bad key_type".into()));

    assert_eq!(
        error_info.response_error_code,
        QuicServiceError::Failed as QuicErrorStatus,
    );
    assert_eq!(error_info.message_handle_label, "InvalidArgument");
    assert!(!error_info.is_internal_error);
}

mod authorized_session {
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

    use super::*;

    const ALGORITHM: Algorithm = Algorithm::HS256;
    const SIGNING_SECRET: &str = "storage-v4-test-secret";
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

    fn signed_token(resource_ids: &[&str], permissions: &[&str]) -> String {
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
            resources: Some(
                resource_ids
                    .iter()
                    .map(|resource_id| ResourcePermission {
                        resource_id: (*resource_id).to_string(),
                        permission: permissions.iter().map(ToString::to_string).collect(),
                    })
                    .collect(),
            ),
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

    async fn authenticated_service() -> StorageServiceV4 {
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");
        StorageServiceV4::new(
            Arc::new(Some(verifier())),
            Arc::new(AuthClientAuthorizer::new(
                "https://auth.invalid".to_string(),
            )),
            immutable_store.clone(),
            immutable_store,
            mutable_store,
            Arc::new(UserAgentFilter::default()),
        )
    }

    async fn start_session(
        service: &StorageServiceV4,
        repository: lore_revision::lore::RepositoryId,
        token: &str,
    ) -> Result<u32, MessageHandleError> {
        let response = service
            .run_request_handler(
                AttributeMap::default().into(),
                ParsedStorageRequestV4::AuthorizeStart {
                    repository,
                    correlation_id: "corr".to_string(),
                    auth_token: token.as_bytes().to_vec(),
                },
            )
            .await?;
        Ok(u32::from_le_bytes(response[0][..4].try_into().unwrap()))
    }

    /// A granted session stores the enumerated grants and the verified
    /// token, so a per-operation action check answers from the session.
    #[tokio::test]
    async fn session_start_stores_grants_and_token() {
        let service = authenticated_service().await;
        let repository = random::<lore_revision::lore::RepositoryId>();
        let token = signed_token(&[&format!("urc-{repository}")], &["read", "migrate"]);

        let session_id = start_session(&service, repository, &token).await.unwrap();

        let session = service.session_map.get(session_id).unwrap();
        assert_eq!(session.token.as_ref().unwrap().raw, token);
        assert!(
            session
                .permits(&*service.repository_authorizer, "migrate")
                .await
        );
        assert!(
            !session
                .permits(&*service.repository_authorizer, "obliterate")
                .await
        );
    }

    fn copy_command(
        session_id: u32,
        source: lore_revision::lore::RepositoryId,
    ) -> ParsedStorageRequestV4 {
        use zerocopy::IntoBytes;
        let mut payload = bytes::BytesMut::with_capacity(80);
        payload.extend_from_slice(source.as_bytes());
        payload.extend_from_slice(&[0u8; 32]); // hash
        payload.extend_from_slice(&[0u8; 16]); // source context
        payload.extend_from_slice(&[0u8; 16]); // target context
        ParsedStorageRequestV4::StorageCommand {
            session_id,
            opcode: Command::Copy as u8,
            payload: payload.freeze(),
        }
    }

    /// One connection, two sessions with different credentials: the
    /// destination session's own token decides the copy source, so a
    /// grant another session brought to the connection must not vouch
    /// for it.
    #[tokio::test]
    async fn copy_source_is_checked_against_the_sessions_own_token() {
        let service = authenticated_service().await;
        let repo_a = random::<lore_revision::lore::RepositoryId>();
        let repo_b = random::<lore_revision::lore::RepositoryId>();
        let token_a = signed_token(&[&format!("urc-{repo_a}")], &["read", "write"]);
        let token_b = signed_token(&[&format!("urc-{repo_b}")], &["read", "write"]);

        let _session_a = start_session(&service, repo_a, &token_a).await.unwrap();
        let session_b = start_session(&service, repo_b, &token_b).await.unwrap();

        let err = service
            .run_request_handler(
                AttributeMap::default().into(),
                copy_command(session_b, repo_a),
            )
            .await
            .expect_err("session A's grant must not vouch for session B's copy source");
        assert!(matches!(err, MessageHandleError::AuthorizationFailure(_)));
    }

    /// The same shape with the destination session's token granting both
    /// partitions passes the source check and reaches the store, which
    /// answers `FragmentNotFound` for the absent address.
    #[tokio::test]
    async fn copy_source_granted_to_the_sessions_token_is_permitted() {
        let service = authenticated_service().await;
        let repo_a = random::<lore_revision::lore::RepositoryId>();
        let repo_b = random::<lore_revision::lore::RepositoryId>();
        let token = signed_token(
            &[&format!("urc-{repo_a}"), &format!("urc-{repo_b}")],
            &["read", "write"],
        );

        let session_b = start_session(&service, repo_b, &token).await.unwrap();

        let err = service
            .run_request_handler(
                AttributeMap::default().into(),
                copy_command(session_b, repo_a),
            )
            .await
            .expect_err("the absent address must be the only failure");
        assert!(matches!(err, MessageHandleError::FragmentNotFound));
    }

    /// A repeated cross-partition copy from a source the session's token
    /// already cleared reuses the cached decision instead of re-asking the
    /// authorizer — the per-session form of the retired connection-wide
    /// skip, safe because the cache is scoped to one token.
    #[tokio::test]
    async fn repeated_copy_from_a_cleared_source_skips_the_authorizer() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use tonic::Status;

        struct CountingAuthorizer {
            watched: lore_revision::lore::RepositoryId,
            source_checks: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl RepositoryAuthorizer for CountingAuthorizer {
            async fn check_repository_access(
                &self,
                token: Option<&VerifiedToken<'_>>,
                repository_id: lore_revision::lore::RepositoryId,
                _action: Option<&str>,
            ) -> Result<(), Status> {
                if repository_id == self.watched {
                    self.source_checks.fetch_add(1, Ordering::Relaxed);
                }
                if token.is_some() {
                    Ok(())
                } else {
                    Err(Status::permission_denied("no token"))
                }
            }
        }

        let repo_a = random::<lore_revision::lore::RepositoryId>();
        let repo_b = random::<lore_revision::lore::RepositoryId>();
        let source_checks = Arc::new(AtomicUsize::new(0));
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");
        let service = StorageServiceV4::new(
            Arc::new(Some(verifier())),
            Arc::new(CountingAuthorizer {
                watched: repo_a,
                source_checks: source_checks.clone(),
            }),
            immutable_store.clone(),
            immutable_store,
            mutable_store,
            Arc::new(UserAgentFilter::default()),
        );

        let token = signed_token(&[&format!("urc-{repo_b}")], &["read", "write"]);
        let session_b = start_session(&service, repo_b, &token).await.unwrap();

        for _ in 0..2 {
            let err = service
                .run_request_handler(
                    AttributeMap::default().into(),
                    copy_command(session_b, repo_a),
                )
                .await
                .expect_err("the absent address must be the only failure");
            assert!(matches!(err, MessageHandleError::FragmentNotFound));
        }

        assert_eq!(source_checks.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ungranted_session_start_is_refused() {
        let service = authenticated_service().await;
        let repository = random::<lore_revision::lore::RepositoryId>();
        let token = signed_token(&["urc-somewhere-else"], &["read"]);

        let err = start_session(&service, repository, &token)
            .await
            .expect_err("a token granting another partition must be refused");
        assert!(matches!(err, MessageHandleError::AuthorizationFailure(_)));
    }
}
