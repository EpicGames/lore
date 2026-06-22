// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Buf;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::TypedBytes;
use lore_base::types::VecBytes;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use tracing::Span;
use tracing::debug;
use tracing::info_span;
use tracing::warn;

use crate::protocol::replication_store::REPLICATION_SERVICE_USER_ID;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::storage::messages::MessageParseError;
use crate::quic::replication_store_service::client::ReplicationStoreClientError;
use crate::quic::replication_store_service::server::ParsedReplicationStoreRequest;
use crate::quic::replication_store_service::server::RequestHandler;
use crate::util::setup_execution;

pub const MAX_ADDRESSES: usize = 100;

pub const BASE_REQUEST_SIZE: usize = size_of::<ReplicationHeader>() +
        // store match byte
        1 +
        // at least 1 address
        size_of::<Address>();

#[derive(Clone, Debug, PartialEq)]
pub struct ExistsBatch {
    pub header: ReplicationHeader,
    pub store_match: StoreMatch,
    pub addresses: Vec<Address>,
}

impl ExistsBatch {
    pub fn to_quic_chunks(self) -> [Bytes; 4] {
        let store_match_num: u8 = self.store_match.into();
        [
            Bytes::default(), // command header
            Bytes::from_owner(self.header),
            Bytes::copy_from_slice(&[store_match_num]),
            Bytes::from_owner(VecBytes(self.addresses)),
        ]
    }

    pub fn parse(mut bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() < BASE_REQUEST_SIZE {
            return Err(MessageParseError::InvalidFieldLength);
        };

        let header: ReplicationHeader = bytes.split_to(size_of::<ReplicationHeader>()).into();
        let store_match: StoreMatch = {
            let raw_value = bytes[0];
            bytes.advance(1);
            raw_value.try_into().map_err(|error| {
                warn!(?error, "failed to parse store match");
                MessageParseError::ParseFailure("Invalid store match")
            })?
        };
        let addresses = bytes.as_type_slice::<Address>().to_vec();

        if addresses.len() > MAX_ADDRESSES {
            return Err(MessageParseError::InvalidFieldLength);
        }

        Ok(Self {
            header,
            store_match,
            addresses,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExistsBatchResponse {
    pub matches: Vec<StoreMatch>,
}

impl ExistsBatchResponse {
    fn data(self) -> Vec<Bytes> {
        let matches_bytes: Vec<u8> = self.matches.into_iter().map(u8::from).collect();
        vec![Bytes::from(matches_bytes)]
    }

    pub fn parse(bytes: Bytes) -> Result<Self, ReplicationStoreClientError> {
        let matches_data: Vec<u8> = bytes.into();
        let matches: Vec<StoreMatch> = matches_data
            .into_iter()
            .map(|b| {
                StoreMatch::try_from(b).map_err(|error| {
                    warn!(?error, "failed to parse store match");
                    ReplicationStoreClientError::ResponseError(
                        "Failed to parse store match from ExistsBatchResponse",
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { matches })
    }
}

pub fn create_handler(
    bytes: Bytes,
    immutable_store: Arc<dyn ImmutableStore>,
    message_context: &'static str,
) -> Result<ParsedReplicationStoreRequest, MessageParseError> {
    let request = ExistsBatch::parse(bytes)?;
    let handler = ExistsBatchHandler {
        immutable_store,
        request,
        message_context,
    };

    Ok(ParsedReplicationStoreRequest::ExistsBatch(handler))
}

#[derive(Debug)]
pub struct ExistsBatchHandler {
    immutable_store: Arc<dyn ImmutableStore>,
    pub request: ExistsBatch,
    message_context: &'static str,
}

#[async_trait::async_trait]
impl RequestHandler for ExistsBatchHandler {
    fn span(&self) -> Span {
        info_span!("exists_batch",
            {CORRELATION_ID} = %self.request.header.correlation_id.as_hyphenated(),
            {REPOSITORY_ID} = %self.request.header.repository,
            message_context = self.message_context)
    }

    async fn run(self) -> Result<Vec<Bytes>, StoreError> {
        debug!(
            num_items = self.request.addresses.len(),
            "exists_batch request"
        );

        let execution = setup_execution(
            module_path!(),
            self.request.header.correlation_id.to_string(),
            REPLICATION_SERVICE_USER_ID.to_string(),
        );

        let matches = LORE_CONTEXT
            .scope(execution, async move {
                // at the time of writing, the AWS Immutable Store
                // has subtle differences between `exists` and `exists_batch` so keep
                // the behaviour consistent between AWS Immutable Store clients and Replicated
                // Store clients who query single/multiple
                if self.request.addresses.len() == 1 {
                    let output = self
                        .immutable_store
                        .exist(
                            self.request.header.repository.into(),
                            self.request.addresses[0],
                            self.request.store_match,
                        )
                        .await?;
                    Ok(vec![output])
                } else {
                    self.immutable_store
                        .exist_batch(
                            self.request.header.repository.into(),
                            &self.request.addresses,
                            self.request.store_match,
                        )
                        .await
                }
            })
            .await?;

        let response = ExistsBatchResponse { matches };
        Ok(response.data())
    }
}

#[cfg(test)]
pub mod tests {
    use lore_base::types::Context;
    use lore_revision::fragment;
    use lore_transport::quic::command_header::CommandHeader;
    use rand::random;
    use uuid::Uuid;

    use super::*;
    use crate::quic::replication_store_service::MAX_CHUNK_SIZE;
    use crate::quic::tests::collapse_bytes_without_header;

    #[test]
    fn is_under_max_chunk_size() {
        // 1 address is included in base request size, so pad with max addresses-1
        let max_request_size = BASE_REQUEST_SIZE + (size_of::<Address>() * (MAX_ADDRESSES - 1));
        // the request is bigger than the response, so if it is under then all good
        assert!(max_request_size + size_of::<CommandHeader>() < MAX_CHUNK_SIZE);
    }

    mod request {
        use super::*;

        #[test]
        fn parsing_single_exists_works() {
            let repository = random::<Context>();
            let (_, address, _) = fragment::generate_random();

            let input = ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                store_match: StoreMatch::MatchFull,
                addresses: vec![address],
            };
            let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
            let output = ExistsBatch::parse(input_bytes).expect("parse should work");

            assert_eq!(input, output);
        }

        #[test]
        fn parsing_with_multiple_addresses_works() {
            let repository = random::<Context>();
            let addresses: Vec<Address> = (0..99)
                .map(|_| {
                    let (_, address, _) = fragment::generate_random();
                    address
                })
                .collect();

            let input = ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                store_match: StoreMatch::MatchPartition,
                addresses,
            };
            let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
            let output = ExistsBatch::parse(input_bytes).expect("parse should work");

            assert_eq!(input, output);
        }

        #[test]
        fn parsing_fails_if_too_big() {
            let repository = random::<Context>();
            let addresses: Vec<Address> = (0..101)
                .map(|_| {
                    let (_, address, _) = fragment::generate_random();
                    address
                })
                .collect();

            let input = ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                store_match: StoreMatch::MatchPartition,
                addresses,
            };
            let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
            let output = ExistsBatch::parse(input_bytes).expect_err("parse should fail");

            assert_eq!(output, MessageParseError::InvalidFieldLength);
        }

        #[test]
        fn parsing_fails_if_too_small() {
            let repository = random::<Context>();

            let input = ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                store_match: StoreMatch::MatchFull,
                // no address is invalid
                addresses: vec![],
            };
            let input_bytes = collapse_bytes_without_header(&input.to_quic_chunks());
            let output = ExistsBatch::parse(input_bytes).expect_err("parse should fail");

            assert_eq!(output, MessageParseError::InvalidFieldLength);
        }
    }

    mod response {
        use super::*;
        use crate::quic::tests::collapse_bytes;

        #[test]
        fn response_to_bytes_works() {
            let original = ExistsBatchResponse {
                matches: vec![
                    StoreMatch::MatchNone,
                    StoreMatch::MatchHash,
                    StoreMatch::MatchFull,
                    StoreMatch::MatchPartition,
                    StoreMatch::MatchNone,
                ],
            };

            let bytes = original.clone().data();
            assert_eq!(bytes, vec![Bytes::from(vec![0, 1, 3, 2, 0])]);

            let reparsed_response =
                ExistsBatchResponse::parse(collapse_bytes(&bytes)).expect("parse should work");
            assert_eq!(reparsed_response, original);
        }

        #[test]
        fn parsing_fails_for_unknown_store_match() {
            // 255 is not a known StoreMatch at time of writing
            let bytes = vec![Bytes::from(vec![255])];

            let error =
                ExistsBatchResponse::parse(collapse_bytes(&bytes)).expect_err("parse should fail");
            assert!(matches!(
                error,
                ReplicationStoreClientError::ResponseError(
                    "Failed to parse store match from ExistsBatchResponse"
                )
            ));
        }
    }
}
