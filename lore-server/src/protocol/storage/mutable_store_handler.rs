// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::lore::RepositoryId;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use tracing::debug;
use tracing::warn;

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
pub struct MutableStoreOp {
    pub key: Hash,
    pub value: Hash,
    pub key_type: KeyType,
}

impl MutableStoreOp {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() < 2 * size_of::<Hash>() + 1 {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let key = Hash::from(&bytes[..size_of::<Hash>()]);
        let value = Hash::from(&bytes[size_of::<Hash>()..2 * size_of::<Hash>()]);
        let key_type = KeyType::try_from(bytes[2 * size_of::<Hash>()])
            .map_err(|_err| MessageParseError::InvalidFieldLength)?;

        Ok(Self {
            key,
            value,
            key_type,
        })
    }
}

/// Key types with their own dedicated, more-restrictive write API
/// (`RepositoryMetadataSet`, `BranchMetadataSet`) that validates read-only
/// fields before writing -- this generic path, reachable from gRPC v1,
/// QUIC v4 and the legacy QUIC protocol, must not become a way to write
/// them directly and bypass that validation.
pub const DISALLOWED_KEY_TYPES: &[KeyType] =
    &[KeyType::RepositoryMetadata, KeyType::BranchMetadata];

pub async fn handle_mutable_store(
    key: Hash,
    value: Hash,
    key_type: KeyType,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn MutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    if DISALLOWED_KEY_TYPES.contains(&key_type) {
        return Err(MessageHandleError::AuthorizationFailure(format!(
            "key_type {key_type:?} must be written through its dedicated API, not MutableStore"
        )));
    }

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(
        "Handling mutable_store for key: {} key_type: {:?} in repository: {}",
        key, key_type, repository
    );

    LORE_CONTEXT
        .scope(execution, async move {
            match mutable_store.store(repository, key, value, key_type).await {
                Ok(()) => {
                    debug!("Successfully stored mutable key: {}", key);
                    Ok(LoreResponse::MutableStore(MutableStoreResponse::default()))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(err) => {
                    warn!(error = ?err, "Failed to store mutable key: {}", key);
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

#[async_trait]
impl Message for MutableStoreOp {
    async fn handle_mutable(
        &self,
        context: Arc<AttributeMap>,
        mutable_store: Arc<dyn MutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_mutable_store(
            self.key,
            self.value,
            self.key_type,
            repository,
            correlation_id.to_string(),
            user_id,
            mutable_store,
        )
        .await
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct MutableStoreResponse {}

impl Response for MutableStoreResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![]
    }
}
