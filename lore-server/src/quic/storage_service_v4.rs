// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use lore_transport::quic::QuicOpCode;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::UnknownCommand;
use lore_transport::quic::command_header::COMMAND_HEADER_SIZE_V4;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use lore_transport::quic::storage_service::MAX_CHUNK_SIZE;
use lore_transport::quic::storage_service::command_name;
use tracing::Span;
use tracing::debug;

use crate::auth::jwt::JwtVerifier;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::authnz::repository_authorizer::VerifiedTokenOwned;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::ConnectionId;
use crate::protocol::client_identify::ClientIdentify;
use crate::protocol::storage::authorize::AuthorizeAction;
use crate::protocol::storage::authorize::parse_authorize;
use crate::protocol::storage::copy::handle_copy;
use crate::protocol::storage::get::handle_get;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::protocol::storage::mutable_cas::handle_mutable_cas;
use crate::protocol::storage::mutable_load::handle_mutable_load;
use crate::protocol::storage::mutable_store_handler::handle_mutable_store;
use crate::protocol::storage::put::handle_put;
use crate::protocol::storage::query::handle_query;
use crate::protocol::storage::session::SessionError;
use crate::protocol::storage::session::SessionMap;
use crate::protocol::storage::verify::handle_verify;
use crate::quic::NO_CONNECTION_ID;
use crate::quic::NO_CORRELATION_ID;
use crate::quic::NO_REPOSITORY_ID;
use crate::quic::NO_USER_ID;
use crate::quic::ProtocolErrorInfo;
use crate::quic::QuicErrorStatus;
use crate::quic::QuicService;
use crate::quic::storage_service::build_storage_protocol_request_span;
use crate::quic::storage_service::is_internal_error;
use crate::quic::storage_service::message_handle_error_to_label;
use crate::quic::storage_service::parse_message_for_opcode_v4;
use crate::telemetry::StorageProtocol;

const RESERVED_OPCODE_PING: QuicOpCode = 4;
const RESERVED_OPCODE_CORRELATE: QuicOpCode = 5;

#[derive(Debug)]
pub enum ParsedStorageRequestV4 {
    AuthorizeStart {
        repository: lore_revision::lore::RepositoryId,
        correlation_id: String,
        auth_token: Vec<u8>,
    },
    AuthorizeStop {
        session_id: u32,
    },
    StorageCommand {
        session_id: u32,
        opcode: QuicOpCode,
        payload: Bytes,
    },
    ClientIdentify(ClientIdentify),
}

fn quic_error_v4(error: &MessageHandleError) -> QuicServiceError {
    match error {
        MessageHandleError::AuthorizationFailure(_) | MessageHandleError::MissingToken => {
            QuicServiceError::NotAuthorized
        }
        MessageHandleError::FragmentNotFound | MessageHandleError::MutableDataNotFound(_) => {
            QuicServiceError::NotFound
        }
        MessageHandleError::SlowDown | MessageHandleError::SessionLimitReached => {
            QuicServiceError::SlowDown
        }
        MessageHandleError::Oversized => QuicServiceError::Oversized,
        _ => QuicServiceError::Failed,
    }
}

pub struct StorageServiceV4 {
    jwt_verifier: Arc<Option<JwtVerifier>>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    session_map: Arc<SessionMap>,
    user_agent_filter: Arc<UserAgentFilter>,
}

impl StorageServiceV4 {
    pub fn new(
        jwt_verifier: Arc<Option<JwtVerifier>>,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
        immutable_store: Arc<dyn ImmutableStore>,
        local_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
        user_agent_filter: Arc<UserAgentFilter>,
    ) -> Self {
        Self {
            jwt_verifier,
            repository_authorizer,
            immutable_store,
            local_store,
            mutable_store,
            session_map: Arc::new(SessionMap::default()),
            user_agent_filter,
        }
    }
}

#[async_trait]
impl QuicService for StorageServiceV4 {
    type ParsedRequestType = ParsedStorageRequestV4;
    type RequestParseErrorType = MessageParseError;
    type RequestHandlerError = MessageHandleError;

    fn get_service_name_label(&self) -> &'static str {
        StorageProtocol::StorageV4.as_str()
    }

    fn parse_request_bytes(
        &self,
        header: &CommandHeader,
        bytes: Bytes,
    ) -> Result<Self::ParsedRequestType, Self::RequestParseErrorType> {
        let opcode = header.cmd;
        let session_id = header.session_id;

        if opcode == RESERVED_OPCODE_PING || opcode == RESERVED_OPCODE_CORRELATE {
            return Err(MessageParseError::UnknownOpcode(opcode));
        }

        if opcode == Command::ClientIdentify as u8 {
            return Ok(ParsedStorageRequestV4::ClientIdentify(
                ClientIdentify::parse(bytes, false)?,
            ));
        }

        if opcode == Command::Authorize as u8 {
            let action = parse_authorize(session_id, bytes)?;
            return match action {
                AuthorizeAction::Start(start) => Ok(ParsedStorageRequestV4::AuthorizeStart {
                    repository: start.repository,
                    correlation_id: start.correlation_id,
                    auth_token: start.auth_token,
                }),
                AuthorizeAction::Stop(stop) => Ok(ParsedStorageRequestV4::AuthorizeStop {
                    session_id: stop.session_id,
                }),
            };
        }

        // Validate this is a known storage opcode (but don't parse yet — we need session context)
        let _command: Command = opcode
            .try_into()
            .map_err(|_err| MessageParseError::UnknownOpcode(opcode))?;

        Ok(ParsedStorageRequestV4::StorageCommand {
            session_id,
            opcode,
            payload: bytes,
        })
    }

    async fn run_request_handler(
        &self,
        context: Arc<AttributeMap>,
        request: Self::ParsedRequestType,
    ) -> Result<Vec<Bytes>, Self::RequestHandlerError> {
        match request {
            ParsedStorageRequestV4::ClientIdentify(msg) => {
                msg.apply(&context, &self.user_agent_filter);
                Ok(vec![])
            }
            ParsedStorageRequestV4::AuthorizeStart {
                repository,
                correlation_id,
                auth_token,
            } => {
                let mut user_id = String::new();
                let mut grants = None;
                let mut token = None;

                if let Some(jwt_verifier) = self.jwt_verifier.as_ref() {
                    let token_str = String::from_utf8(auth_token).map_err(|err| {
                        MessageHandleError::AuthorizationFailure(format!(
                            "invalid token encoding: {err}"
                        ))
                    })?;

                    if token_str.is_empty() {
                        return Err(MessageHandleError::MissingToken);
                    }

                    let authorization = jwt_verifier
                        .verify_token(&token_str)
                        .await
                        .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;

                    let verified = VerifiedToken {
                        raw: &token_str,
                        claims: &authorization,
                    };
                    grants = self
                        .repository_authorizer
                        .granted_access(Some(&verified), repository)
                        .await
                        .map_err(|status| {
                            MessageHandleError::AuthorizationFailure(status.message().to_string())
                        })?;
                    token = Some(Arc::new(verified.owned()));

                    user_id = crate::util::get_user_id_from_token(Some(authorization));
                }

                let session_map = self.session_map.clone();
                match session_map.start(repository, correlation_id, user_id, grants, token) {
                    Ok((session_id, correlation_id)) => {
                        debug!(
                            session_id,
                            repository = %repository,
                            correlation_id,
                            "Authorized session"
                        );
                        let response_data = vec![Bytes::copy_from_slice(&session_id.to_le_bytes())];
                        Ok(response_data)
                    }
                    Err(SessionError::LimitReached) => Err(MessageHandleError::SessionLimitReached),
                    Err(SessionError::CounterExhausted | SessionError::NotFound) => {
                        Err(MessageHandleError::InternalError)
                    }
                }
            }
            ParsedStorageRequestV4::AuthorizeStop { session_id } => {
                let session_map = self.session_map.clone();
                match session_map.stop(session_id) {
                    Ok(()) => {
                        debug!(session_id, "Session stopped");
                        Ok(vec![])
                    }
                    Err(SessionError::NotFound) => Err(MessageHandleError::NotConnected),
                    Err(_) => Err(MessageHandleError::InternalError),
                }
            }
            ParsedStorageRequestV4::StorageCommand {
                session_id,
                opcode,
                payload,
            } => {
                let session_map = self.session_map.clone();
                let session = session_map
                    .get(session_id)
                    .ok_or(MessageHandleError::NotConnected)?;

                let repository = session.repository;
                let correlation_id = session.correlation_id.clone();
                let user_id = session.user_id.clone();
                let token = session.token.clone();
                let authorized_sources = session.authorized_sources.clone();
                drop(session);

                // Parse the storage command payload using v4-aware parsers — Copy carries an
                // extra `target_context` field on the wire that the legacy parser cannot decode.
                let parsed = parse_message_for_opcode_v4(opcode, payload).map_err(|err| {
                    tracing::warn!("Failed to parse v4 storage command: {err}");
                    MessageHandleError::InternalError
                })?;

                // Dispatch to standalone handler functions with explicit session context
                let response = match parsed {
                    crate::quic::storage_service::ParsedStorageRequest::Get(get) => {
                        handle_get(
                            get.address,
                            repository,
                            correlation_id,
                            user_id,
                            self.immutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::GetMetadata(get) => {
                        crate::protocol::storage::get::handle_get_metadata(
                            get.address,
                            repository,
                            correlation_id,
                            user_id,
                            self.immutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::Put(put) => {
                        handle_put(
                            &put,
                            repository,
                            correlation_id,
                            user_id,
                            self.immutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::Query(_query) => {
                        // Query uses the raw bytes, not the parsed struct.
                        // Re-parse is needed because parse_message_for_opcode_v4 consumed the bytes.
                        // However, the Query struct stores the bytes internally.
                        handle_query(&_query.address, repository, self.immutable_store.clone())
                            .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::Verify(verify) => {
                        handle_verify(
                            verify.address,
                            verify.heal,
                            repository,
                            correlation_id,
                            user_id,
                            self.local_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::Copy(copy) => {
                        // The destination was checked at this session's start;
                        // a cross-partition source is this command's own
                        // question, asked with this session's token — another
                        // session's authorization on the connection must not
                        // vouch for it. A source this session's token already
                        // cleared is remembered, so a repeated copy from it
                        // skips the check.
                        if copy.source_repository != repository
                            && !authorized_sources.contains(&copy.source_repository)
                        {
                            let verified = token.as_deref().map(VerifiedTokenOwned::as_token);
                            self.repository_authorizer
                                .check_repository_access(
                                    verified.as_ref(),
                                    copy.source_repository,
                                    None,
                                )
                                .await
                                .map_err(|status| {
                                    MessageHandleError::AuthorizationFailure(
                                        status.message().to_string(),
                                    )
                                })?;
                            authorized_sources.insert(copy.source_repository);
                        }
                        handle_copy(
                            copy.source_repository,
                            copy.source_address,
                            repository,
                            copy.target_context,
                            correlation_id,
                            user_id,
                            self.immutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::MutableLoad(load) => {
                        handle_mutable_load(
                            load.key,
                            load.key_type,
                            repository,
                            correlation_id,
                            user_id,
                            self.mutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::GetResolved(resolved) => {
                        crate::protocol::storage::get_resolved::handle_get_resolved(
                            resolved.key,
                            resolved.context,
                            resolved.flags,
                            repository,
                            correlation_id,
                            user_id,
                            self.mutable_store.clone(),
                            self.immutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::PutResolved(resolved) => {
                        crate::protocol::storage::put_resolved::handle_put_resolved(
                            resolved.key,
                            resolved.put(),
                            resolved.address,
                            repository,
                            correlation_id,
                            user_id,
                            self.mutable_store.clone(),
                            self.immutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::MutableStoreOp(store) => {
                        handle_mutable_store(
                            store.key,
                            store.value,
                            store.key_type,
                            repository,
                            correlation_id,
                            user_id,
                            self.mutable_store.clone(),
                        )
                        .await
                    }
                    crate::quic::storage_service::ParsedStorageRequest::MutableCas(cas) => {
                        handle_mutable_cas(
                            cas.key,
                            cas.expected,
                            cas.value,
                            cas.key_type,
                            repository,
                            correlation_id,
                            user_id,
                            self.mutable_store.clone(),
                        )
                        .await
                    }
                    // Connect and Correlate are v2-only, handled as reserved opcodes above
                    crate::quic::storage_service::ParsedStorageRequest::Connect(_)
                    | crate::quic::storage_service::ParsedStorageRequest::Correlate(_) => {
                        Err(MessageHandleError::NotImplemented)
                    }
                }?;

                Ok(response.data())
            }
        }
    }

    fn command_to_metrics_label(&self, opcode: QuicOpCode) -> &'static str {
        if opcode == RESERVED_OPCODE_PING || opcode == RESERVED_OPCODE_CORRELATE {
            return "reserved";
        }
        if opcode == Command::Authorize as u8 {
            return "authorize";
        }
        let command: Result<Command, UnknownCommand> = opcode.try_into();
        match command {
            Ok(command) => command_name(&command),
            Err(_) => "unknown",
        }
    }

    fn transform_protocol_error(&self, error: &Self::RequestHandlerError) -> ProtocolErrorInfo {
        let service_error = quic_error_v4(error);
        let is_appropriate_for_logging = !matches!(
            service_error,
            QuicServiceError::SlowDown | QuicServiceError::NotFound
        );

        ProtocolErrorInfo {
            response_error_code: service_error as QuicErrorStatus,
            message_handle_label: message_handle_error_to_label(error),
            is_internal_error: is_internal_error(error),
            is_appropriate_for_logging,
        }
    }

    fn max_chunk_size(&self) -> usize {
        MAX_CHUNK_SIZE
    }

    fn header_size(&self) -> usize {
        COMMAND_HEADER_SIZE_V4
    }

    fn build_request_span(
        &self,
        header: &CommandHeader,
        _message: &Self::ParsedRequestType,
        context: &Arc<AttributeMap>,
    ) -> Span {
        let connection_id = context
            .get::<ConnectionId>()
            .map_or_else(|| NO_CONNECTION_ID.to_string(), |id| id.0.to_string());

        let session = if header.session_id != 0 {
            self.session_map.get(header.session_id)
        } else {
            None
        };

        let (repository_id, correlation_id, user_id) = match session {
            Some(session) => {
                let repository_id = session.repository.to_string();
                let repository_id = if repository_id.is_empty() {
                    NO_REPOSITORY_ID.to_string()
                } else {
                    repository_id
                };
                let correlation_id = if session.correlation_id.is_empty() {
                    NO_CORRELATION_ID.to_string()
                } else {
                    session.correlation_id.clone()
                };
                let user_id = if session.user_id.is_empty() {
                    NO_USER_ID.to_string()
                } else {
                    session.user_id.clone()
                };
                (repository_id, correlation_id, user_id)
            }
            None => (
                NO_REPOSITORY_ID.to_string(),
                NO_CORRELATION_ID.to_string(),
                NO_USER_ID.to_string(),
            ),
        };

        let user_agent = context.get::<crate::protocol::client_identify::UserAgentValue>();
        build_storage_protocol_request_span(
            header.cmd,
            StorageProtocol::StorageV4,
            &connection_id,
            &repository_id,
            &correlation_id,
            &user_id,
            user_agent
                .as_ref()
                .map_or(crate::quic::NO_USER_AGENT, |v| v.0.as_ref()),
        )
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use lore_telemetry::user_agent_filter::UserAgentFilter;
    use lore_transport::quic::QuicServiceError;
    use lore_transport::quic::command_header::CommandHeader;
    use rand::random;

    use super::*;
    use crate::protocol::storage::session::MAX_CONCURRENT_SESSIONS;
    use crate::quic::QuicService;
    use crate::store::test_store_create;

    fn make_service(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> StorageServiceV4 {
        StorageServiceV4::new(
            Arc::new(None),
            Arc::new(crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer),
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

        let ci = crate::protocol::client_identify::ClientIdentify {
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

        use super::*;
        use crate::auth::jwk::JWKService;
        use crate::auth::jwk::JWKServiceError;
        use crate::auth::jwt::AuthorizationToken;
        use crate::auth::jwt::DEFAULT_IDENTITY_CLAIM;
        use crate::auth::jwt::JwtVerifier;
        use crate::auth::jwt::ResourcePermission;
        use crate::authnz::repository_authorizer::AuthClientAuthorizer;

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
}
