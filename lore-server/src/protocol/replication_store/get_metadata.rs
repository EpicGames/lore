// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use tracing::Span;
use tracing::debug;
use tracing::info_span;

use crate::protocol::replication_store::REPLICATION_SERVICE_USER_ID;
use crate::protocol::replication_store::query::Query;
use crate::protocol::replication_store::query::QueryResponse;
use crate::protocol::storage::messages::MessageParseError;
use crate::quic::replication_store_service::server::ParsedReplicationStoreRequest;
use crate::quic::replication_store_service::server::RequestHandler;
use crate::util::setup_execution;

pub fn create_handler(
    bytes: Bytes,
    immutable_store: Arc<dyn ImmutableStore>,
    message_context: &'static str,
) -> Result<ParsedReplicationStoreRequest, MessageParseError> {
    let request = Query::parse(bytes)?;
    let handler = GetMetadataHandler {
        immutable_store,
        request,
        message_context,
    };
    Ok(ParsedReplicationStoreRequest::GetMetadata(handler))
}

#[derive(Debug)]
pub struct GetMetadataHandler {
    pub immutable_store: Arc<dyn ImmutableStore>,
    pub request: Query,
    message_context: &'static str,
}

#[async_trait::async_trait]
impl RequestHandler for GetMetadataHandler {
    fn span(&self) -> Span {
        info_span!("get_metadata",
            { CORRELATION_ID } = %self.request.0.header.correlation_id.as_hyphenated(),
            { REPOSITORY_ID } = %self.request.0.header.repository,
            message_context = self.message_context)
    }

    async fn run(self) -> Result<Vec<Bytes>, StoreError> {
        let inner = self.request.0;
        debug!(
            {{ ADDRESS }} = %inner.addresses[0],
            "get_metadata request"
        );

        let execution = setup_execution(
            module_path!(),
            inner.header.correlation_id.to_string(),
            REPLICATION_SERVICE_USER_ID.to_string(),
        );

        let result = LORE_CONTEXT
            .scope(execution, async move {
                self.immutable_store
                    .get_metadata(inner.header.repository.into(), inner.addresses[0])
                    .await
            })
            .await?;

        let response = QueryResponse {
            fragment: result.fragment,
            match_made: result.match_made,
        };
        Ok(response.data())
    }
}
