// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Buf;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Fragment;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
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
use crate::protocol::replication_store::exists_batch::ExistsBatch;
use crate::protocol::storage::messages::MessageParseError;
use crate::quic::replication_store_service::client::ReplicationStoreClientError;
use crate::quic::replication_store_service::server::ParsedReplicationStoreRequest;
use crate::quic::replication_store_service::server::RequestHandler;
use crate::util::setup_execution;

// At time of writing, Query and ExistsBatch have the same request payload shape
// so it is convenient to just reuse the request. If they stray,
// then it would be wiser to define them as 2 distinct but similar looking request
// payloads
#[derive(Clone, Debug, PartialEq)]
pub struct Query(pub ExistsBatch);

impl Query {
    pub fn to_quic_chunks(self) -> [Bytes; 4] {
        ExistsBatch::to_quic_chunks(self.0)
    }

    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        let inner = ExistsBatch::parse(bytes)?;

        // not possible unless clients we control are malicious for some reason
        // or have an implementation bug. Sanity check to ensure only 1 address is being supplied
        if inner.addresses.len() != 1 {
            return Err(MessageParseError::InvalidFieldLength);
        }

        Ok(Self(inner))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryResponse {
    pub fragment: Fragment,
    pub match_made: StoreMatch,
}

impl QueryResponse {
    fn data(self) -> Vec<Bytes> {
        let match_made: u8 = self.match_made.into();
        vec![
            Bytes::copy_from_slice(self.fragment.as_bytes()),
            Bytes::copy_from_slice(&[match_made]),
        ]
    }

    pub fn parse(mut bytes: Bytes) -> Result<Self, ReplicationStoreClientError> {
        let fragment: Fragment = bytes.split_to(size_of::<Fragment>()).into();
        let match_made: StoreMatch = {
            let raw_value = bytes[0];
            bytes.advance(1);
            raw_value.try_into().map_err(|error| {
                warn!(?error, "failed to parse match_made");
                ReplicationStoreClientError::ResponseError("Invalid match_made")
            })?
        };

        Ok(Self {
            fragment,
            match_made,
        })
    }
}

pub fn create_handler(
    bytes: Bytes,
    immutable_store: Arc<dyn ImmutableStore>,
    message_context: &'static str,
) -> Result<ParsedReplicationStoreRequest, MessageParseError> {
    let request = Query::parse(bytes)?;
    let handler = QueryHandler {
        immutable_store,
        request,
        message_context,
    };

    Ok(ParsedReplicationStoreRequest::Query(handler))
}

#[derive(Debug)]
pub struct QueryHandler {
    immutable_store: Arc<dyn ImmutableStore>,
    pub request: Query,
    message_context: &'static str,
}

#[async_trait::async_trait]
impl RequestHandler for QueryHandler {
    fn span(&self) -> Span {
        info_span!("query",
            {CORRELATION_ID} = %self.request.0.header.correlation_id.as_hyphenated(),
            {REPOSITORY_ID} = %self.request.0.header.repository,
            message_context = self.message_context)
    }

    async fn run(self) -> Result<Vec<Bytes>, StoreError> {
        let inner = self.request.0;
        debug!(
            {{ ADDRESS }} = %inner.addresses[0],
            "query request"
        );

        let execution = setup_execution(
            module_path!(),
            inner.header.correlation_id.to_string(),
            REPLICATION_SERVICE_USER_ID.to_string(),
        );

        let query_result = LORE_CONTEXT
            .scope(execution, async move {
                self.immutable_store
                    .query(
                        inner.header.repository.into(),
                        inner.addresses[0],
                        inner.store_match,
                    )
                    .await
            })
            .await?;

        let response = QueryResponse {
            fragment: query_result.fragment,
            match_made: query_result.match_made,
        };
        Ok(response.data())
    }
}

#[cfg(test)]
pub mod tests {
    use lore_base::types::Context;
    use lore_revision::fragment;
    use rand::random;
    use uuid::Uuid;

    use super::*;
    use crate::quic::tests::collapse_bytes_without_header;

    mod request {
        use lore_base::types::Address;

        use super::*;
        use crate::protocol::replication_store::header::ReplicationHeader;

        #[test]
        fn parsing_works() {
            let repository = random::<Context>();
            let (_, address, _) = fragment::generate_random();

            let input = Query(ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                store_match: StoreMatch::MatchFull,
                addresses: vec![address],
            });
            let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
            let output = Query::parse(input_bytes).expect("parse should work");

            assert_eq!(input, output);
        }

        #[test]
        fn parsing_fails_if_too_big() {
            let repository = random::<Context>();

            let input = Query(ExistsBatch {
                header: ReplicationHeader {
                    correlation_id: Uuid::new_v4(),
                    repository,
                },
                store_match: StoreMatch::MatchFull,
                addresses: vec![Address::default(), Address::default()],
            });
            let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
            let output = Query::parse(input_bytes).expect_err("parse should fail");

            assert_eq!(output, MessageParseError::InvalidFieldLength);
        }
    }

    mod response {
        use super::*;
        use crate::quic::tests::collapse_bytes;

        #[test]
        fn to_bytes_works() {
            let (fragment, _, _) = fragment::generate_random();

            let original = QueryResponse {
                fragment,
                match_made: StoreMatch::MatchFull,
            };
            let bytes = original.clone().data();

            let reparsed_response =
                QueryResponse::parse(collapse_bytes(&bytes)).expect("parse should work");
            assert_eq!(reparsed_response, original);
        }
    }
}
