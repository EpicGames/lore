// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
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
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::ConnectionId;
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
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
    session_map: Arc<SessionMap>,
}

impl StorageServiceV4 {
    pub fn new(
        jwt_verifier: Arc<Option<JwtVerifier>>,
        immutable_store: Arc<dyn ImmutableStore>,
        local_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
    ) -> Self {
        Self {
            jwt_verifier,
            immutable_store,
            local_store,
            mutable_store,
            session_map: Arc::new(SessionMap::default()),
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
        _context: Arc<AttributeMap>,
        request: Self::ParsedRequestType,
    ) -> Result<Vec<Bytes>, Self::RequestHandlerError> {
        match request {
            ParsedStorageRequestV4::AuthorizeStart {
                repository,
                correlation_id,
                auth_token,
            } => {
                let mut user_id = String::new();

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

                    crate::auth::jwt::verify_authorization(&authorization, repository)
                        .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;

                    user_id = crate::util::get_user_id_from_token(Some(authorization));
                }

                let session_map = self.session_map.clone();
                match session_map.start(repository, correlation_id, user_id) {
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
                        handle_copy(
                            copy.source_repository,
                            copy.source_address,
                            repository,
                            copy.target_context,
                            correlation_id,
                            user_id,
                            Some(&session_map),
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

        build_storage_protocol_request_span(
            header.cmd,
            StorageProtocol::StorageV4,
            &connection_id,
            &repository_id,
            &correlation_id,
            &user_id,
        )
    }
}

#[cfg(test)]
mod tests {
    use lore_transport::quic::QuicServiceError;
    use rand::random;

    use super::*;
    use crate::protocol::storage::session::MAX_CONCURRENT_SESSIONS;
    use crate::quic::QuicService;
    use crate::store::test_store_create;

    /// Fill the session map to capacity then attempt one more `AuthorizeStart`,
    /// verifying the handler returns `SlowDown` and that `transform_protocol_error`
    /// classifies it the same way `stream_handler` would.
    #[tokio::test]
    async fn authorize_start_returns_slow_down_when_session_limit_reached() {
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        let service = StorageServiceV4::new(
            Arc::new(None),
            immutable_store.clone(),
            immutable_store.clone(),
            mutable_store,
        );

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
}
