// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use enum_dispatch::enum_dispatch;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_telemetry::tracing::fields::CONNECTION_ID;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::PROTOCOL;
use lore_telemetry::tracing::fields::QUIC_OPCODE;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::SAMPLING_TIER_LOW;
use lore_telemetry::tracing::fields::TRANSPORT;
use lore_telemetry::tracing::fields::USER_ID;
use lore_transport::quic::QuicOpCode;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::UnknownCommand;
use lore_transport::quic::command_header::CommandHeader;
use lore_transport::quic::storage_service::Command;
use lore_transport::quic::storage_service::MAX_CHUNK_SIZE;
use lore_transport::quic::storage_service::command_name;
use tracing::Span;
use tracing::debug;
use tracing::info_span;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::JwtVerifier;
use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::ConnectionId;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::protocol::storage::requests;
use crate::quic::NO_CONNECTION_ID;
use crate::quic::NO_CORRELATION_ID;
use crate::quic::NO_REPOSITORY_ID;
use crate::quic::NO_USER_ID;
use crate::quic::ProtocolErrorInfo;
use crate::quic::QuicErrorStatus;
use crate::quic::QuicService;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;

/// Build the per-RPC `OTel` root span for an inbound storage opcode. Shared
/// between v0 (`LoreStorageService`) and v4 (`StorageServiceV4`); the caller
/// passes its `StorageProtocol` so v0 and v4 are distinguishable in traces via
/// the `protocol` attribute.
pub(crate) fn build_storage_protocol_request_span(
    cmd: QuicOpCode,
    protocol: StorageProtocol,
    connection_id: &str,
    repository_id: &str,
    correlation_id: &str,
    user_id: &str,
) -> Span {
    let command_parse = Command::try_from(cmd);
    let opcode_label = command_parse
        .as_ref()
        .map_or("", |command| command_name(command));
    match command_parse {
        Ok(Command::Authorize) => info_span!(
            parent: None,
            "StorageAuthorizeTask",
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::Get) => info_span!(
            parent: None,
            "StorageGetTask",
            { SAMPLING_TIER_LOW } = true,
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::GetMetadata) => info_span!(
            parent: None,
            "StorageGetMetadataTask",
            { SAMPLING_TIER_LOW } = true,
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::PresignDownload) => info_span!(
            parent: None,
            "StoragePresignDownloadTask",
            { SAMPLING_TIER_LOW } = true,
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::Put) => info_span!(
            parent: None,
            "StoragePutTask",
            { SAMPLING_TIER_LOW } = true,
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::Query) => info_span!(
            parent: None,
            "StorageQueryTask",
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::Verify) => info_span!(
            parent: None,
            "StorageVerifyTask",
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::Copy) => info_span!(
            parent: None,
            "StorageCopyTask",
            { SAMPLING_TIER_LOW } = true,
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::MutableLoad) => info_span!(
            parent: None,
            "StorageMutableLoadTask",
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::MutableStore) => info_span!(
            parent: None,
            "StorageMutableStoreTask",
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Ok(Command::MutableCas) => info_span!(
            parent: None,
            "StorageMutableCompareAndSwapTask",
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
        Err(_) => info_span!(
            parent: None,
            "StorageUnknownTask",
            { TRANSPORT } = %Transport::Quic,
            { PROTOCOL } = %protocol,
            { QUIC_OPCODE } = opcode_label,
            { CONNECTION_ID } = connection_id,
            { REPOSITORY_ID } = repository_id,
            { CORRELATION_ID } = correlation_id,
            { USER_ID } = user_id,
        ),
    }
}

fn request_identifiers_from_context(
    context: &Arc<AttributeMap>,
) -> (String, String, String, String) {
    let connection_id = context
        .get::<ConnectionId>()
        .map_or_else(|| NO_CONNECTION_ID.to_string(), |id| id.0.to_string());
    let repository_id = context
        .get::<RepositoryId>()
        .map_or_else(|| NO_REPOSITORY_ID.to_string(), |id| id.to_string());
    let correlation_id = context
        .get::<CorrelationId>()
        .map_or_else(|| NO_CORRELATION_ID.to_string(), |id| id.to_string());
    let user_id = context
        .get::<AuthorizationToken>()
        .map(|token| token.user_id.clone())
        .filter(|user_id| !user_id.is_empty())
        .unwrap_or_else(|| NO_USER_ID.to_string());
    (connection_id, repository_id, correlation_id, user_id)
}

#[derive(Debug)]
#[enum_dispatch(Message)]
pub enum ParsedStorageRequest {
    Connect(requests::Connect),
    Copy(requests::Copy),
    Get(requests::Get),
    /// Wire-identical to `Get`; the dispatcher routes this to `handle_get_metadata` so the
    /// response carries fragment metadata only — no payload bytes.
    GetMetadata(crate::protocol::storage::get::GetMetadata),
    PresignDownload(requests::PresignDownload),
    Put(requests::Put),
    Query(requests::Query),
    Correlate(requests::Correlate),
    Verify(requests::Verify),
    MutableLoad(requests::MutableLoad),
    MutableStoreOp(requests::MutableStoreOp),
    MutableCas(requests::MutableCas),
}

fn quic_error(message_error: &MessageHandleError) -> QuicServiceError {
    match message_error {
        MessageHandleError::AuthorizationFailure(_) | MessageHandleError::MissingToken => {
            QuicServiceError::NotAuthorized
        }
        MessageHandleError::FragmentNotFound | MessageHandleError::MutableDataNotFound(_) => {
            QuicServiceError::NotFound
        }
        MessageHandleError::SlowDown => QuicServiceError::SlowDown,
        MessageHandleError::Oversized => QuicServiceError::Oversized,
        MessageHandleError::NotImplemented => QuicServiceError::NotSupported,
        _ => QuicServiceError::Failed,
    }
}

/// Legacy opcode for the Correlate command, which was removed from the client
/// Command enum in the lore-storage/0.4 protocol but must still be handled
/// server-side for backward compatibility with urc/0.2 clients.
const LEGACY_CORRELATE_OPCODE: QuicOpCode = 5;

pub fn parse_message_for_opcode(
    opcode: QuicOpCode,
    bytes: Bytes,
) -> Result<ParsedStorageRequest, MessageParseError> {
    debug!(
        "Attempting to parse {} bytes for opcode: {opcode}",
        bytes.len()
    );

    // Handle legacy Correlate opcode (removed from client Command enum in lore-storage/0.4)
    if opcode == LEGACY_CORRELATE_OPCODE {
        return Ok(ParsedStorageRequest::Correlate(requests::Correlate::parse(
            bytes,
        )?));
    }

    match opcode
        .try_into()
        .map_err(|_e| MessageParseError::UnknownOpcode(opcode))?
    {
        Command::Authorize => Ok(ParsedStorageRequest::Connect(requests::Connect::parse(
            bytes,
        )?)),
        Command::Get => Ok(ParsedStorageRequest::Get(requests::Get::parse(bytes)?)),
        Command::GetMetadata => Ok(ParsedStorageRequest::GetMetadata(
            crate::protocol::storage::get::GetMetadata::parse(bytes)?,
        )),
        Command::PresignDownload => Ok(ParsedStorageRequest::PresignDownload(
            requests::PresignDownload::parse(bytes)?,
        )),
        Command::Put => Ok(ParsedStorageRequest::Put(requests::Put::parse(bytes)?)),
        Command::Query => Ok(ParsedStorageRequest::Query(requests::Query::parse(bytes)?)),
        Command::Verify => Ok(ParsedStorageRequest::Verify(requests::Verify::parse(
            bytes,
        )?)),
        Command::Copy => Ok(ParsedStorageRequest::Copy(requests::Copy::parse(bytes)?)),
        Command::MutableLoad => Ok(ParsedStorageRequest::MutableLoad(
            requests::MutableLoad::parse(bytes)?,
        )),
        Command::MutableStore => Ok(ParsedStorageRequest::MutableStoreOp(
            requests::MutableStoreOp::parse(bytes)?,
        )),
        Command::MutableCas => Ok(ParsedStorageRequest::MutableCas(
            requests::MutableCas::parse(bytes)?,
        )),
    }
}

/// `parse_message_for_opcode` variant used by the lore-storage/0.4 service. Identical to the
/// urc/0.2 parser except `Command::Copy` decodes the v4 wire (80 bytes, with `target_context`
/// on the tail) instead of the legacy 64-byte format.
pub fn parse_message_for_opcode_v4(
    opcode: QuicOpCode,
    bytes: Bytes,
) -> Result<ParsedStorageRequest, MessageParseError> {
    if let Ok(Command::Copy) = opcode.try_into() {
        return Ok(ParsedStorageRequest::Copy(requests::Copy::parse_v4(bytes)?));
    }
    parse_message_for_opcode(opcode, bytes)
}

pub fn message_handle_error_to_label(value: &MessageHandleError) -> &'static str {
    match value {
        MessageHandleError::AlreadyConnected => "AlreadyConnected",
        MessageHandleError::BranchExists => "BranchExists",
        MessageHandleError::BranchMismatch => "BranchMismatch",
        MessageHandleError::HashMismatch => "HashMismatch",
        MessageHandleError::FragmentNotFound => "FragmentNotFound",
        MessageHandleError::InvalidParentBranch => "InvalidParentBranch",
        MessageHandleError::InternalError => "InternalError",
        MessageHandleError::MutableDataNotFound(_) => "MutableDataNotFound",
        MessageHandleError::NoSuchBranch => "NoSuchBranch",
        MessageHandleError::NotConnected => "NotConnected",
        MessageHandleError::QueryResultSizeMismatch => "QueryResultSizeMismatch",
        MessageHandleError::StoreFailure => "StoreFailure",
        MessageHandleError::AuthorizationFailure(_) => "AuthorizationFailure",
        MessageHandleError::MissingToken => "MissingToken",
        MessageHandleError::BranchProtected => "BranchProtected",
        MessageHandleError::Metadata => "Metadata",
        MessageHandleError::NotImplemented => "NotImplemented",
        MessageHandleError::SlowDown => "SlowDown",
        MessageHandleError::Oversized => "Oversized",
        MessageHandleError::HashFailed => "HashFailed",
        MessageHandleError::InvalidFragment => "InvalidFragment",
        MessageHandleError::HandlerTimeout => "HandlerTimeout",
        MessageHandleError::SessionLimitReached => "SessionLimitReached",
    }
}

pub fn is_internal_error(error: &MessageHandleError) -> bool {
    match error {
        MessageHandleError::AuthorizationFailure(_)
        | MessageHandleError::AlreadyConnected
        | MessageHandleError::BranchExists
        | MessageHandleError::BranchMismatch
        | MessageHandleError::BranchProtected
        | MessageHandleError::FragmentNotFound
        | MessageHandleError::HashMismatch
        | MessageHandleError::InvalidParentBranch
        | MessageHandleError::MissingToken
        | MessageHandleError::MutableDataNotFound(_)
        | MessageHandleError::NoSuchBranch
        | MessageHandleError::NotConnected
        | MessageHandleError::Oversized
        | MessageHandleError::Metadata
        | MessageHandleError::HashFailed
        | MessageHandleError::InvalidFragment
        | MessageHandleError::SessionLimitReached => false,
        MessageHandleError::HandlerTimeout
        | MessageHandleError::InternalError
        | MessageHandleError::NotImplemented
        | MessageHandleError::QueryResultSizeMismatch
        | MessageHandleError::StoreFailure
        | MessageHandleError::SlowDown => true,
    }
}

pub struct StorageService {
    jwt_verifier: Arc<Option<JwtVerifier>>,
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
}

impl StorageService {
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
        }
    }
}

#[async_trait]
impl QuicService for StorageService {
    type ParsedRequestType = ParsedStorageRequest;
    type RequestParseErrorType = MessageParseError;
    type RequestHandlerError = MessageHandleError;

    fn get_service_name_label(&self) -> &'static str {
        StorageProtocol::StorageV0.as_str()
    }

    fn parse_request_bytes(
        &self,
        header: &lore_transport::quic::command_header::CommandHeader,
        bytes: Bytes,
    ) -> Result<Self::ParsedRequestType, Self::RequestParseErrorType> {
        parse_message_for_opcode(header.cmd, bytes)
    }

    async fn run_request_handler(
        &self,
        context: Arc<AttributeMap>,
        request: Self::ParsedRequestType,
    ) -> Result<Vec<Bytes>, Self::RequestHandlerError> {
        let lore_response = match request {
            ParsedStorageRequest::Connect(request) => {
                request
                    .handle_auth(context, self.jwt_verifier.clone())
                    .await
            }
            ParsedStorageRequest::MutableLoad(_)
            | ParsedStorageRequest::MutableStoreOp(_)
            | ParsedStorageRequest::MutableCas(_) => {
                request
                    .handle_mutable(context, self.mutable_store.clone())
                    .await
            }
            ParsedStorageRequest::Verify(verify) => {
                verify.handle(context, self.local_store.clone()).await
            }
            other => other.handle(context, self.immutable_store.clone()).await,
        }?;

        Ok(lore_response.data())
    }

    fn command_to_metrics_label(&self, opcode: QuicOpCode) -> &'static str {
        if opcode == LEGACY_CORRELATE_OPCODE {
            return "correlate";
        }
        let command: Result<Command, UnknownCommand> = opcode.try_into();
        match command {
            Ok(command) => command_name(&command),
            Err(_) => "unknown",
        }
    }

    fn transform_protocol_error(&self, error: &Self::RequestHandlerError) -> ProtocolErrorInfo {
        let service_error = quic_error(error);
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

    fn build_request_span(
        &self,
        header: &CommandHeader,
        _message: &Self::ParsedRequestType,
        context: &Arc<AttributeMap>,
    ) -> Span {
        let (connection_id, repository_id, correlation_id, user_id) =
            request_identifiers_from_context(context);
        build_storage_protocol_request_span(
            header.cmd,
            StorageProtocol::StorageV0,
            &connection_id,
            &repository_id,
            &correlation_id,
            &user_id,
        )
    }
}
