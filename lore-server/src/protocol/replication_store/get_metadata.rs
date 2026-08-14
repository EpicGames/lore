// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Buf;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Partition;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use tracing::Span;
use tracing::debug;
use tracing::info_span;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::protocol::replication_store::REPLICATION_SERVICE_USER_ID;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::storage::messages::MessageParseError;
use crate::quic::replication_store_service::client::ReplicationStoreClientError;
use crate::quic::replication_store_service::server::ParsedReplicationStoreRequest;
use crate::quic::replication_store_service::server::RequestHandler;
use crate::util::setup_execution;

pub const BASE_REQUEST_SIZE: usize = size_of::<ReplicationHeader>() + size_of::<Address>();

#[derive(Clone, Debug, PartialEq)]
pub struct GetMetadata {
    pub header: ReplicationHeader,
    pub address: Address,
}

impl GetMetadata {
    pub fn to_quic_chunks(self) -> [Bytes; 3] {
        [
            Bytes::default(), // command header
            Bytes::from_owner(self.header),
            Bytes::from_owner(self.address),
        ]
    }

    pub fn parse(mut bytes: Bytes) -> Result<GetMetadata, MessageParseError> {
        if bytes.len() < BASE_REQUEST_SIZE {
            return Err(MessageParseError::InvalidFieldLength);
        };

        let header: ReplicationHeader = bytes.split_to(size_of::<ReplicationHeader>()).into();
        let address: Address = bytes.split_to(size_of::<Address>()).into();

        Ok(GetMetadata { header, address })
    }
}

fn serialize_response(data: StoreGetData) -> Vec<Bytes> {
    let match_made: u8 = data.match_made.into();
    vec![
        Bytes::copy_from_slice(data.fragment.as_bytes()),
        Bytes::copy_from_slice(&[match_made]),
        Bytes::copy_from_slice(data.partition.as_bytes()),
    ]
}

pub fn parse_response(mut bytes: Bytes) -> Result<StoreGetData, ReplicationStoreClientError> {
    let fragment: Fragment = bytes.split_to(size_of::<Fragment>()).into();
    let match_made: StoreMatch = bytes[0].try_into().map_err(|error| {
        warn!(?error, "failed to parse match_made");
        ReplicationStoreClientError::ResponseError(
            "failed to parse match_made from get_metadata response",
        )
    })?;
    bytes.advance(1);
    let partition: Partition = bytes.split_to(size_of::<Partition>()).into();
    Ok(StoreGetData {
        fragment,
        match_made,
        partition,
        payload: None,
    })
}

pub fn create_handler(
    bytes: Bytes,
    immutable_store: Arc<dyn ImmutableStore>,
    message_context: &'static str,
) -> Result<ParsedReplicationStoreRequest, MessageParseError> {
    let request = GetMetadata::parse(bytes)?;
    let handler = GetMetadataHandler {
        immutable_store,
        request,
        message_context,
    };

    Ok(ParsedReplicationStoreRequest::GetMetadata(handler))
}

#[derive(Debug)]
pub struct GetMetadataHandler {
    immutable_store: Arc<dyn ImmutableStore>,
    pub request: GetMetadata,
    message_context: &'static str,
}

#[async_trait::async_trait]
impl RequestHandler for GetMetadataHandler {
    fn span(&self) -> Span {
        info_span!("get_metadata",
            {CORRELATION_ID} = %self.request.header.correlation_id.as_hyphenated(),
            {REPOSITORY_ID} = %self.request.header.repository,
            message_context = self.message_context)
    }

    async fn run(self) -> Result<Vec<Bytes>, StoreError> {
        let request = self.request;
        debug!(
            {{ ADDRESS }} = %request.address,
            "get_metadata request"
        );

        let execution = setup_execution(
            module_path!(),
            request.header.correlation_id.to_string(),
            REPLICATION_SERVICE_USER_ID.to_string(),
        );

        let result = LORE_CONTEXT
            .scope(execution, async move {
                self.immutable_store
                    .get_metadata(request.header.repository.into(), request.address)
                    .await
            })
            .await?;

        Ok(serialize_response(result))
    }
}

#[cfg(test)]
pub mod tests {
    use lore_base::types::Context;
    use lore_revision::fragment;
    use rand::random;
    use uuid::Uuid;

    use super::*;
    use crate::quic::tests::collapse_bytes;
    use crate::quic::tests::collapse_bytes_without_header;

    mod request {
        use lore_base::types::Address;

        use super::*;
        use crate::protocol::replication_store::header::ReplicationHeader;

        #[test]
        fn parsing_works() {
            let repository = random::<Context>();
            let (_, address, _) = fragment::generate_random();

            let input = GetMetadata {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                address,
            };
            let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
            let output = GetMetadata::parse(input_bytes).expect("parse should work");

            assert_eq!(input, output);
        }

        #[test]
        fn parsing_fails_if_truncated() {
            let repository = random::<Context>();

            let input = GetMetadata {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                address: Address::default(),
            };
            let mut input_bytes = collapse_bytes_without_header(&input.to_quic_chunks());
            input_bytes.truncate(input_bytes.len() - 1);
            let output = GetMetadata::parse(input_bytes).expect_err("parse should fail");

            assert!(matches!(output, MessageParseError::InvalidFieldLength));
        }
    }

    mod response {
        use super::*;

        #[test]
        fn serialize_response_has_three_chunks() {
            let (fragment, _, _) = fragment::generate_random();
            let data = StoreGetData {
                fragment,
                match_made: StoreMatch::MatchFull,
                partition: random::<lore_base::types::Partition>(),
                payload: None,
            };
            // if this assertion fails, then new servers sending
            // this response to old clients will be silently wrong
            // and could end up parsing incorrect payloads.
            assert_eq!(serialize_response(data).len(), 3);
        }

        #[test]
        fn response_roundtrips() {
            let (fragment, _, _) = fragment::generate_random();
            let original = StoreGetData {
                fragment,
                match_made: StoreMatch::MatchFull,
                partition: random::<lore_base::types::Partition>(),
                payload: None,
            };
            let bytes = serialize_response(original.clone());
            let reparsed = parse_response(collapse_bytes(&bytes)).expect("parse should work");
            assert_eq!(reparsed.fragment, original.fragment);
            assert_eq!(reparsed.match_made, original.match_made);
            assert_eq!(reparsed.partition, original.partition);
            assert_eq!(reparsed.payload, None);
        }
    }
}
