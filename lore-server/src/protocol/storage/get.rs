// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::ADDRESS;
use opentelemetry::metrics::Histogram;
use tracing::debug;
use tracing::info;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::get_user_id_from_context;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::util::setup_execution;

#[derive(Clone, Debug, PartialEq)]
pub struct Get {
    pub address: Address,
}

struct GetInstrumentProvider;

impl InstrumentProvider for GetInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.quic.message.get"
    }
}

impl GetInstrumentProvider {
    fn payload_size_histogram(&self) -> Histogram<u64> {
        self.size_histogram("payload_size")
    }

    fn content_size_histogram(&self) -> Histogram<u64> {
        self.size_histogram("content_size")
    }
}

struct GetInstrument {
    provider: &'static GetInstrumentProvider,
    payload_size_histogram: Histogram<u64>,
    content_size_histogram: Histogram<u64>,
}

impl GetInstrument {
    fn payload_size(&self, size: u32) {
        self.payload_size_histogram
            .record(size as u64, self.provider.labels());
    }

    fn content_size(&self, size: u64) {
        self.content_size_histogram
            .record(size, self.provider.labels());
    }
}

fn instruments() -> &'static GetInstrument {
    static PROVIDER: OnceLock<GetInstrumentProvider> = OnceLock::new();
    static INSTRUMENTS: OnceLock<GetInstrument> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let provider = PROVIDER.get_or_init(|| GetInstrumentProvider);
        GetInstrument {
            provider,
            payload_size_histogram: provider.payload_size_histogram(),
            content_size_histogram: provider.content_size_histogram(),
        }
    })
}

impl Get {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        if bytes.len() < size_of::<Address>() {
            return Err(MessageParseError::InvalidFieldLength);
        }

        Ok(Self {
            address: bytes.into(),
        })
    }
}

/// Variant of [`handle_get`] that returns only the fragment metadata — no payload bytes.
///
/// Wire request and address validation are identical to `Get`; the only difference is the
/// response carries an empty `payload`. Used by callers that need fragment metadata (size,
/// flags) for existence/size lookups without paying the full payload transfer cost.
/// Dispatches to `ImmutableStore::query`, which never reads the payload bytes server-side
/// — savings are both wire-side and server-side.
pub async fn handle_get_metadata(
    address: Address,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!("Handling get_metadata request for address {address} in repository {repository}",);

    LORE_CONTEXT
        .scope(execution, async move {
            match immutable_store
                .get_metadata(repository, address)
                .await
            {
                Ok(StoreGetData {
                    mut fragment,
                    match_made,
                    ..
                }) => {
                    // A miss is reported as `Ok` with `MatchNone`, so absence is checked here
                    // rather than caught as an error. Anything weaker than a full match is still a
                    // match: the store already applied its own read scope, and refusing what it
                    // was willing to describe would make this stricter than `handle_get`, which
                    // serves whatever `get` returns.
                    if match_made == StoreMatch::MatchNone {
                        debug!({ADDRESS} = %address, "Did not find any fragment for address");
                        return Err(MessageHandleError::FragmentNotFound);
                    }
                    fragment.flags &= !FragmentFlags::PayloadStored;
                    fragment.flags |= FragmentFlags::PayloadStoredDurable;
                    Ok(LoreResponse::Get(GetResponse {
                        fragment,
                        payload: Bytes::new(),
                    }))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(StoreError::AddressNotFound(_)) => {
                    debug!({ADDRESS} = %address, "Did not find any fragment for address");
                    Err(MessageHandleError::FragmentNotFound)
                }
                Err(err) => {
                    warn!(error = ?err, {ADDRESS} = %address, "Failed to query metadata for address");
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

pub async fn handle_get(
    address: Address,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(
        "Handling get request to retrieve fragment with address: {} for repository: {}",
        address, repository,
    );

    LORE_CONTEXT
        .scope(execution, async move {
            match immutable_store
                .get(repository, address)
                .await
                .and_then(lore_storage::StoreGetData::into_payload)
            {
                Ok((mut fragment, payload)) => {
                    debug!(
                        "Found fragment for address: {} with length: {} for {}",
                        address, fragment.size_payload, fragment.size_content
                    );

                    let instruments = instruments();
                    instruments.payload_size(fragment.size_payload);
                    instruments.content_size(fragment.size_content);

                    fragment.flags &= !FragmentFlags::PayloadStored;
                    fragment.flags |= FragmentFlags::PayloadStoredDurable;
                    Ok(LoreResponse::Get(GetResponse { fragment, payload }))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(StoreError::AddressNotFound(_)) => {
                    info!({ADDRESS} = %address, "Did not find any fragment for address");
                    Err(MessageHandleError::FragmentNotFound)
                }
                Err(err) => {
                    warn!(error = ?err, {ADDRESS} = %address, "Failed to get fragment for address");
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

#[async_trait]
impl Message for Get {
    #[tracing::instrument(name = "Get::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        immutable_store: Arc<dyn ImmutableStore>,
        _repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_get(
            self.address,
            repository,
            correlation_id.to_string(),
            user_id,
            immutable_store,
        )
        .await
    }
}

/// Wire-identical to `Get`, dispatched via [`handle_get_metadata`] so the response carries
/// `Fragment` only — no payload bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct GetMetadata {
    pub address: Address,
}

impl GetMetadata {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() < size_of::<Address>() {
            return Err(MessageParseError::InvalidFieldLength);
        }
        Ok(Self {
            address: bytes.into(),
        })
    }
}

#[async_trait]
impl Message for GetMetadata {
    #[tracing::instrument(name = "GetMetadata::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        immutable_store: Arc<dyn ImmutableStore>,
        _repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_get_metadata(
            self.address,
            repository,
            correlation_id.to_string(),
            user_id,
            immutable_store,
        )
        .await
    }
}

#[derive(Debug, PartialEq)]
pub struct GetResponse {
    pub fragment: Fragment,
    pub payload: Bytes,
}

impl Response for GetResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![
            Bytes::copy_from_slice(self.fragment.as_bytes()),
            self.payload.clone(),
        ]
    }
}
