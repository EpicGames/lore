// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use enum_dispatch::enum_dispatch;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_telemetry::tracing::fields::CONNECTION_ID;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::PROTOCOL;
use lore_telemetry::tracing::fields::QUIC_OPCODE;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::SAMPLING_TIER_LOW;
use lore_telemetry::tracing::fields::TRANSPORT;
use lore_transport::quic::QuicErrorStatus;
use lore_transport::quic::QuicOpCode;
use lore_transport::quic::command_header::CommandHeader;
use tracing::Instrument;
use tracing::Span;
use tracing::info_span;

use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::ConnectionId;
use crate::protocol::replication_store::exists_batch;
use crate::protocol::replication_store::exists_batch::ExistsBatchHandler;
use crate::protocol::replication_store::get;
use crate::protocol::replication_store::get::GetHandler;
use crate::protocol::replication_store::get_metadata;
use crate::protocol::replication_store::get_metadata::GetMetadataHandler;
use crate::protocol::replication_store::obliterate;
use crate::protocol::replication_store::obliterate::ObliterateHandler;
use crate::protocol::replication_store::put;
use crate::protocol::replication_store::put::PutHandler;
use crate::protocol::replication_store::query;
use crate::protocol::replication_store::query::QueryHandler;
use crate::protocol::storage::messages::MessageParseError;
use crate::quic::NO_CONNECTION_ID;
use crate::quic::ProtocolErrorInfo;
use crate::quic::QuicService;
use crate::quic::replication_store_service::Command;
use crate::quic::replication_store_service::MAX_CHUNK_SIZE;
use crate::quic::replication_store_service::ReplicationServiceErrorCode;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;

/// A trait to represent request handlers for the `ReplicationStoreService`,
/// this trait exists for convenience in reducing boilerplate in running
/// request handlers
#[async_trait::async_trait]
#[enum_dispatch]
pub trait RequestHandler {
    /// Creates a span that the `QuicService` will enter before running the handler
    fn span(&self) -> Span;

    /// Runs the request handler with its given configuration.
    /// If successful, the Ok result is the response bytes to send to the client.
    async fn run(self) -> Result<Vec<Bytes>, StoreError>;
}

#[derive(Debug)]
#[enum_dispatch(RequestHandler)]
pub enum ParsedReplicationStoreRequest {
    Put(PutHandler),
    ExistsBatch(ExistsBatchHandler),
    Get(GetHandler),
    Obliterate(ObliterateHandler),
    Query(QueryHandler),
    GetMetadata(GetMetadataHandler),
}

pub fn command_name(command: &Command) -> &'static str {
    match command {
        Command::ImmutableExistBatch => "immutable_exist_batch",
        Command::ImmutableGet => "immutable_get",
        Command::ImmutablePut => "immutable_put",
        Command::ImmutableObliterate => "immutable_obliterate",
        Command::ImmutableQuery => "immutable_query",
        Command::ImmutableLocalExistBatch => "immutable_local_exist_batch",
        Command::ImmutableLocalGet => "immutable_local_get",
        Command::ImmutableLocalPut => "immutable_local_put",
        Command::ImmutableLocalQuery => "immutable_local_query",
        Command::ImmutableGetMetadata => "immutable_get_metadata",
        Command::ImmutableLocalGetMetadata => "immutable_local_get_metadata",
    }
}

pub struct ReplicationStoreService {
    immutable_store: Arc<dyn ImmutableStore>,
    local_store: Arc<dyn ImmutableStore>,
}

impl ReplicationStoreService {
    pub fn new(
        immutable_store: Arc<dyn ImmutableStore>,
        local_store: Arc<dyn ImmutableStore>,
    ) -> Self {
        Self {
            immutable_store,
            local_store,
        }
    }
}

#[async_trait]
impl QuicService for ReplicationStoreService {
    type ParsedRequestType = ParsedReplicationStoreRequest;
    // MessageParseError is a convenient type that encapsulates most of the kind of errors
    // that we can encounter with message parsing as well. If the overlap changes too much we can
    // create our own type instead - it does not strictly need to be `MessageParseError`
    type RequestParseErrorType = MessageParseError;
    type RequestHandlerError = StoreError;

    fn get_service_name_label(&self) -> &'static str {
        StorageProtocol::Replication.as_str()
    }

    fn parse_request_bytes(
        &self,
        header: &lore_transport::quic::command_header::CommandHeader,
        bytes: Bytes,
    ) -> Result<Self::ParsedRequestType, Self::RequestParseErrorType> {
        let command: Command = header
            .cmd
            .try_into()
            .map_err(|_e| MessageParseError::UnknownOpcode(header.cmd))?;

        let handler = match command {
            Command::ImmutableExistBatch => {
                exists_batch::create_handler(bytes, self.immutable_store.clone(), "exists_batch")?
            }
            Command::ImmutableGet => {
                get::create_handler(bytes, self.immutable_store.clone(), "get")?
            }
            Command::ImmutablePut => {
                put::create_handler(bytes, self.immutable_store.clone(), "put")?
            }
            Command::ImmutableObliterate => {
                obliterate::create_handler(bytes, self.immutable_store.clone())?
            }
            Command::ImmutableQuery => {
                query::create_handler(bytes, self.immutable_store.clone(), "query")?
            }
            Command::ImmutableLocalExistBatch => {
                exists_batch::create_handler(bytes, self.local_store.clone(), "local_exists_batch")?
            }
            Command::ImmutableLocalGet => {
                get::create_handler(bytes, self.local_store.clone(), "local_get")?
            }
            Command::ImmutableLocalPut => {
                put::create_handler(bytes, self.local_store.clone(), "local_put")?
            }
            Command::ImmutableLocalQuery => {
                query::create_handler(bytes, self.local_store.clone(), "local_query")?
            }
            Command::ImmutableGetMetadata => {
                get_metadata::create_handler(bytes, self.immutable_store.clone(), "get_metadata")?
            }
            Command::ImmutableLocalGetMetadata => {
                get_metadata::create_handler(bytes, self.local_store.clone(), "local_get_metadata")?
            }
        };

        Ok(handler)
    }

    async fn run_request_handler(
        &self,
        _context: Arc<AttributeMap>,
        request: Self::ParsedRequestType,
    ) -> Result<Vec<Bytes>, Self::RequestHandlerError> {
        let span = request.span();
        request.run().instrument(span).await
    }

    fn command_to_metrics_label(&self, opcode: QuicOpCode) -> &'static str {
        opcode.try_into().as_ref().map_or("unknown", command_name)
    }

    fn transform_protocol_error(&self, error: &Self::RequestHandlerError) -> ProtocolErrorInfo {
        let error_code: ReplicationServiceErrorCode = error.into();
        let is_appropriate_for_logging = match error_code {
            ReplicationServiceErrorCode::Internal | ReplicationServiceErrorCode::Oversized => true,
            ReplicationServiceErrorCode::AddressNotFound
            | ReplicationServiceErrorCode::SlowDown
            | ReplicationServiceErrorCode::PayloadNotFound => false,
        };
        let is_internal_error = match error_code {
            // if something has gone wrong, or we are failing to provide a good service, then treat
            // as internal like
            ReplicationServiceErrorCode::Internal | ReplicationServiceErrorCode::SlowDown => true,
            ReplicationServiceErrorCode::AddressNotFound
            | ReplicationServiceErrorCode::PayloadNotFound
            | ReplicationServiceErrorCode::Oversized => false,
        };

        ProtocolErrorInfo {
            response_error_code: error_code as QuicErrorStatus,
            message_handle_label: error_code_to_label(error_code),
            is_internal_error,
            is_appropriate_for_logging,
        }
    }

    fn max_chunk_size(&self) -> usize {
        MAX_CHUNK_SIZE
    }

    fn build_request_span(
        &self,
        header: &CommandHeader,
        message: &Self::ParsedRequestType,
        context: &Arc<AttributeMap>,
    ) -> Span {
        let replication_header = match message {
            ParsedReplicationStoreRequest::Get(h) => &h.request.header,
            ParsedReplicationStoreRequest::Put(h) => &h.request.header,
            ParsedReplicationStoreRequest::ExistsBatch(h) => &h.request.header,
            ParsedReplicationStoreRequest::Obliterate(h) => &h.request.header,
            ParsedReplicationStoreRequest::Query(h) => &h.request.0.header,
            ParsedReplicationStoreRequest::GetMetadata(h) => &h.request.0.header,
        };
        let repository_id = replication_header.repository.to_string();
        let correlation_id = replication_header
            .correlation_id
            .as_hyphenated()
            .to_string();

        let connection_id = context
            .get::<ConnectionId>()
            .map_or_else(|| NO_CONNECTION_ID.to_string(), |id| id.0.to_string());

        let command_parse = Command::try_from(header.cmd);
        let opcode_label = command_parse
            .as_ref()
            .map_or("", |command| command_name(command));

        match command_parse {
            Ok(Command::ImmutableExistBatch) => info_span!(
                parent: None,
                "ReplicationExistBatchTask",
                { SAMPLING_TIER_LOW } = true,
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableGet) => info_span!(
                parent: None,
                "ReplicationGetTask",
                { SAMPLING_TIER_LOW } = true,
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutablePut) => info_span!(
                parent: None,
                "ReplicationPutTask",
                { SAMPLING_TIER_LOW } = true,
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableObliterate) => info_span!(
                parent: None,
                "ReplicationObliterateTask",
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableQuery) => info_span!(
                parent: None,
                "ReplicationQueryTask",
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableLocalExistBatch) => info_span!(
                parent: None,
                "ReplicationLocalExistBatchTask",
                { SAMPLING_TIER_LOW } = true,
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableLocalGet) => info_span!(
                parent: None,
                "ReplicationLocalGetTask",
                { SAMPLING_TIER_LOW } = true,
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableLocalPut) => info_span!(
                parent: None,
                "ReplicationLocalPutTask",
                { SAMPLING_TIER_LOW } = true,
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableLocalQuery) => info_span!(
                parent: None,
                "ReplicationLocalQueryTask",
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableGetMetadata) => info_span!(
                parent: None,
                "ReplicationGetMetadataTask",
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Ok(Command::ImmutableLocalGetMetadata) => info_span!(
                parent: None,
                "ReplicationLocalGetMetadataTask",
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { QUIC_OPCODE } = opcode_label,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
            Err(_) => info_span!(
                parent: None,
                "ReplicationUnknownTask",
                { TRANSPORT } = %Transport::Quic,
                { PROTOCOL } = %StorageProtocol::Replication,
                { CONNECTION_ID } = connection_id,
                { REPOSITORY_ID } = repository_id,
                { CORRELATION_ID } = correlation_id,
            ),
        }
    }
}

pub fn error_code_to_label(code: ReplicationServiceErrorCode) -> &'static str {
    match code {
        ReplicationServiceErrorCode::Internal => "Internal",
        ReplicationServiceErrorCode::AddressNotFound => "StoreNotFound",
        ReplicationServiceErrorCode::SlowDown => "StoreSlowDown",
        ReplicationServiceErrorCode::PayloadNotFound => "PayloadNotFound",
        ReplicationServiceErrorCode::Oversized => "Oversized",
    }
}

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_revision::fragment;
    use lore_storage::StoreMatch;
    use lore_transport::quic::command_header::CommandHeader;
    use rand::random;
    use uuid::Uuid;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::protocol::replication_store::exists_batch::ExistsBatch;
    use crate::protocol::replication_store::get::Get;
    use crate::protocol::replication_store::get::GetResponse;
    use crate::protocol::replication_store::header::ReplicationHeader;
    use crate::protocol::replication_store::obliterate::Obliterate;
    use crate::protocol::replication_store::put::Put;
    use crate::protocol::replication_store::query::Query;
    use crate::protocol::replication_store::query::QueryResponse;
    use crate::quic::QuicService;
    use crate::quic::replication_store_service::*;
    use crate::quic::tests::collapse_bytes;
    use crate::quic::tests::collapse_bytes_without_header;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn immutable_put_works_end_to_end() {
        let (immutable_store, _, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // sanity check the above address does not exist in the store
        {
            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    assert!(
                        immutable_store
                            .clone()
                            .get(repository.into(), address, StoreMatch::MatchFull)
                            .await
                            .unwrap_err()
                            .is_address_not_found()
                    );
                })
                .await;
        }

        let request = Put {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            address,
            fragment,
            flags: 0,
            payload: Some(payload.clone()),
        };

        let service =
            ReplicationStoreService::new(immutable_store.clone(), immutable_store.clone());

        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutablePut as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::Put(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        assert!(handle_output.is_empty());

        LORE_CONTEXT
            .scope(execution, async move {
                let get_output = immutable_store
                    .get(repository.into(), address, StoreMatch::MatchFull)
                    .await
                    .expect("get should have worked");
                assert_eq!(get_output.1, payload);
            })
            .await;
    }

    // batch version
    #[tokio::test]
    async fn immutable_exists_batch_works_end_to_end() {
        let (immutable_store, _, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repository = random::<Context>();

        let address_match_full = {
            let (fragment, address, payload) = fragment::generate_random();

            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;

            address
        };

        let address_other_repository = {
            let other_repository = random::<Context>();
            let (fragment, address, payload) = fragment::generate_random();

            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(
                            other_repository.into(),
                            address,
                            fragment,
                            Some(payload),
                            false,
                        )
                        .await
                        .expect("put should work");
                })
                .await;

            address
        };

        let address_different_context = {
            let (fragment, address, payload) = fragment::generate_random();

            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;

            let different_context = random::<Context>();
            Address {
                hash: address.hash,
                context: different_context,
            }
        };

        let (_, address_no_match, _) = fragment::generate_random();

        let addresses = vec![
            address_match_full,
            address_other_repository,
            address_different_context,
            address_no_match,
        ];

        let request = ExistsBatch {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            store_match: StoreMatch::MatchFull,
            addresses: addresses.clone(),
        };

        let service =
            ReplicationStoreService::new(immutable_store.clone(), immutable_store.clone());

        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableExistBatch as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::ExistsBatch(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        assert_eq!(handle_output, vec![Bytes::from(vec![3, 1, 2, 0])]);

        // and the output matches as if we went to the store directly
        let direct_store_output = LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .exist_batch(repository.into(), &addresses, StoreMatch::MatchFull)
                    .await
                    .expect("direct should work")
            })
            .await;
        assert_eq!(
            handle_output,
            vec![Bytes::from(
                direct_store_output
                    .into_iter()
                    .map(u8::from)
                    .collect::<Vec<_>>()
            )]
        );
    }

    // single address version
    #[tokio::test]
    async fn immutable_exists_works_end_to_end() {
        let (immutable_store, _, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repository = random::<Context>();

        let address_match_full = {
            let (fragment, address, payload) = fragment::generate_random();

            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;

            address
        };

        let addresses = vec![address_match_full];

        let request = ExistsBatch {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            store_match: StoreMatch::MatchFull,
            addresses: addresses.clone(),
        };

        let service =
            ReplicationStoreService::new(immutable_store.clone(), immutable_store.clone());

        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableExistBatch as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::ExistsBatch(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        assert_eq!(handle_output, vec![Bytes::from(vec![3])]);

        // and the output matches as if we went to the store directly
        let direct_store_output = LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .exist_batch(repository.into(), &addresses, StoreMatch::MatchFull)
                    .await
                    .expect("direct should work")
            })
            .await;
        assert_eq!(
            handle_output,
            vec![Bytes::from(
                direct_store_output
                    .into_iter()
                    .map(u8::from)
                    .collect::<Vec<_>>()
            )]
        );
    }

    #[tokio::test]
    async fn immutable_get_works_end_to_end() {
        let (immutable_store, _, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repository = random::<Context>();

        let (fragment, address, payload) = fragment::generate_random();
        {
            let payload = payload.clone();
            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;
        };

        let request = Get {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            match_required: StoreMatch::MatchFull,
            address,
        };

        let service =
            ReplicationStoreService::new(immutable_store.clone(), immutable_store.clone());

        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableGet as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::Get(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        let response_parsed =
            GetResponse::parse(collapse_bytes(&handle_output)).expect("response parse should work");
        assert_eq!(response_parsed.fragment, fragment);
        assert_eq!(response_parsed.payload, payload);
    }

    #[tokio::test]
    async fn obliterate_works_end_to_end() {
        let (immutable_store, _, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let get_address = || {
            let execution = execution.clone();
            let immutable_store = immutable_store.clone();
            async move {
                LORE_CONTEXT
                    .scope(execution, async move {
                        immutable_store
                            .get(repository.into(), address, StoreMatch::MatchFull)
                            .await
                    })
                    .await
            }
        };

        // set up an address for deletion
        {
            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;
        }
        get_address().await.expect("address should exist");

        let request = Obliterate {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            address,
        };

        let service =
            ReplicationStoreService::new(immutable_store.clone(), immutable_store.clone());

        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableObliterate as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::Obliterate(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        assert_eq!(
            handle_output,
            vec![
                Bytes::copy_from_slice(1u64.as_bytes()),
                Bytes::copy_from_slice(1u64.as_bytes()),
            ]
        );

        get_address()
            .await
            .expect_err("address should have been obliterated");
    }

    #[tokio::test]
    async fn query_works_end_to_end() {
        let (immutable_store, _, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repository = random::<Context>();

        let (fragment, address, payload) = fragment::generate_random();
        {
            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;

            (fragment, address)
        };

        let request = Query(ExistsBatch {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            store_match: StoreMatch::MatchFull,
            addresses: vec![address],
        });

        let service =
            ReplicationStoreService::new(immutable_store.clone(), immutable_store.clone());

        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableQuery as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::Query(_)
        ));

        let service_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        let parsed_response =
            QueryResponse::parse(collapse_bytes(&service_output)).expect("Failed to parse");

        let store_direct_output = LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .query(repository.into(), address, StoreMatch::MatchFull)
                    .await
                    .expect("query should work")
            })
            .await;

        assert_eq!(parsed_response.fragment, store_direct_output.fragment);
        assert_eq!(parsed_response.match_made, store_direct_output.match_made);
    }

    #[tokio::test]
    async fn get_metadata_works_end_to_end() {
        let (immutable_store, _, execution) =
            test_store_create().await.expect("Failed to create stores");

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();
        {
            let immutable_store = immutable_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    immutable_store
                        .clone()
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;
        }

        let request = Query(ExistsBatch {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            store_match: StoreMatch::MatchFull,
            addresses: vec![address],
        });

        let service =
            ReplicationStoreService::new(immutable_store.clone(), immutable_store.clone());

        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableGetMetadata as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::GetMetadata(_)
        ));

        let service_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        let parsed_response =
            QueryResponse::parse(collapse_bytes(&service_output)).expect("Failed to parse");

        let store_direct_output = LORE_CONTEXT
            .scope(execution.clone(), async move {
                immutable_store
                    .clone()
                    .get_metadata(repository.into(), address)
                    .await
                    .expect("get_metadata should work")
            })
            .await;

        assert_eq!(parsed_response.fragment, store_direct_output.fragment);
        assert_eq!(parsed_response.match_made, store_direct_output.match_made);
    }

    #[tokio::test]
    async fn immutable_local_get_metadata_routes_to_local_store() {
        let (main_store, local_store, execution) = create_two_stores().await;

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // put data only in the local store
        {
            let local_store = local_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    local_store
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;
        }

        let request = Query(ExistsBatch {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            store_match: StoreMatch::MatchFull,
            addresses: vec![address],
        });

        let service = ReplicationStoreService::new(main_store.clone(), local_store.clone());

        // ImmutableLocalGetMetadata should find the data via the local store
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableLocalGetMetadata as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.clone().to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::GetMetadata(_)
        ));

        let service_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        let parsed_response =
            QueryResponse::parse(collapse_bytes(&service_output)).expect("Failed to parse");
        assert_eq!(parsed_response.match_made, StoreMatch::MatchFull);

        // Regular ImmutableGetMetadata should NOT find it (main store is empty)
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableGetMetadata as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");

        let service_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler should succeed even for a miss");
        let parsed_response =
            QueryResponse::parse(collapse_bytes(&service_output)).expect("Failed to parse");
        assert_eq!(parsed_response.match_made, StoreMatch::MatchNone);
    }

    /// Helper to create a second independent store for local-store routing tests
    async fn create_two_stores() -> (
        Arc<dyn ImmutableStore>,
        Arc<dyn ImmutableStore>,
        Arc<lore_revision::interface::ExecutionContext>,
    ) {
        let (main_store, _, execution) = test_store_create()
            .await
            .expect("Failed to create main store");
        let (local_store, _, _) = test_store_create()
            .await
            .expect("Failed to create local store");
        (main_store, local_store, execution)
    }

    #[tokio::test]
    async fn immutable_local_exists_batch_routes_to_local_store() {
        let (main_store, local_store, execution) = create_two_stores().await;

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // put data only in the local store
        {
            let local_store = local_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    local_store
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;
        }

        let request = ExistsBatch {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            store_match: StoreMatch::MatchFull,
            addresses: vec![address],
        };

        let service = ReplicationStoreService::new(main_store.clone(), local_store.clone());

        // ImmutableLocalExistBatch should find the data via the local store
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableLocalExistBatch as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.clone().to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::ExistsBatch(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        assert_eq!(handle_output, vec![Bytes::from(vec![3])]);

        // Regular ImmutableExistBatch should NOT find it (main store is empty)
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableExistBatch as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        assert_eq!(handle_output, vec![Bytes::from(vec![0])]);
    }

    #[tokio::test]
    async fn immutable_local_get_routes_to_local_store() {
        let (main_store, local_store, execution) = create_two_stores().await;

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // put data only in the local store
        {
            let payload = payload.clone();
            let local_store = local_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    local_store
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;
        }

        let request = Get {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            match_required: StoreMatch::MatchFull,
            address,
        };

        let service = ReplicationStoreService::new(main_store.clone(), local_store.clone());

        // ImmutableLocalGet should find the data via the local store
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableLocalGet as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.clone().to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::Get(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        let response_parsed =
            GetResponse::parse(collapse_bytes(&handle_output)).expect("response parse should work");
        assert_eq!(response_parsed.fragment, fragment);
        assert_eq!(response_parsed.payload, payload);

        // Regular ImmutableGet should NOT find it (main store is empty)
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableGet as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await;
        assert!(handle_output.unwrap_err().is_address_not_found());
    }

    #[tokio::test]
    async fn immutable_local_put_routes_to_local_store() {
        let (main_store, local_store, execution) = create_two_stores().await;

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // sanity check the address does not exist in either store
        {
            let local_store = local_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    assert!(
                        local_store
                            .get(repository.into(), address, StoreMatch::MatchFull)
                            .await
                            .unwrap_err()
                            .is_address_not_found()
                    );
                })
                .await;
        }

        let request = Put {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            address,
            fragment,
            flags: 0,
            payload: Some(payload.clone()),
        };

        let service = ReplicationStoreService::new(main_store.clone(), local_store.clone());

        // ImmutableLocalPut should write to the local store
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableLocalPut as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::Put(_)
        ));

        let handle_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        assert!(handle_output.is_empty());

        // Verify the data landed in the local store
        {
            let local_store = local_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    let get_output = local_store
                        .get(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .expect("get from local store should work");
                    assert_eq!(get_output.1, payload);
                })
                .await;
        }

        // Verify the data is NOT in the main store
        LORE_CONTEXT
            .scope(execution, async move {
                assert!(
                    main_store
                        .get(repository.into(), address, StoreMatch::MatchFull)
                        .await
                        .unwrap_err()
                        .is_address_not_found()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn immutable_local_query_routes_to_local_store() {
        let (main_store, local_store, execution) = create_two_stores().await;

        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        // put data only in the local store
        {
            let local_store = local_store.clone();
            LORE_CONTEXT
                .scope(execution.clone(), async move {
                    local_store
                        .put(repository.into(), address, fragment, Some(payload), false)
                        .await
                        .expect("put should work");
                })
                .await;
        }

        let request = Query(ExistsBatch {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            store_match: StoreMatch::MatchFull,
            addresses: vec![address],
        });

        let service = ReplicationStoreService::new(main_store.clone(), local_store.clone());

        // ImmutableLocalQuery should find the data via the local store
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableLocalQuery as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.clone().to_quic_chunks()),
            )
            .expect("Failed to parse");
        assert!(matches!(
            parse_output,
            ParsedReplicationStoreRequest::Query(_)
        ));

        let service_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        let parsed_response =
            QueryResponse::parse(collapse_bytes(&service_output)).expect("Failed to parse");
        assert_eq!(parsed_response.match_made, StoreMatch::MatchFull);

        // Regular ImmutableQuery should NOT find it (main store is empty)
        let parse_output = service
            .parse_request_bytes(
                &CommandHeader::new(Command::ImmutableQuery as QuicOpCode, 0, 0),
                collapse_bytes_without_header(&request.to_quic_chunks()),
            )
            .expect("Failed to parse");

        let service_output = service
            .run_request_handler(AttributeMap::default().into(), parse_output)
            .await
            .expect("handler failed");
        let parsed_response =
            QueryResponse::parse(collapse_bytes(&service_output)).expect("Failed to parse");
        assert_eq!(parsed_response.match_made, StoreMatch::MatchNone);
    }
}
