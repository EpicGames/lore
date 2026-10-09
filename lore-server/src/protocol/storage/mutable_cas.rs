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
use zerocopy::IntoBytes;

use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::get_user_id_from_context;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::protocol::storage::mutable_store_handler::check_generic_write_key_type;
use crate::util::setup_execution;

#[derive(Clone, Debug, PartialEq)]
pub struct MutableCas {
    pub key: Hash,
    pub expected: Hash,
    pub value: Hash,
    pub key_type: KeyType,
}

impl MutableCas {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() < 3 * size_of::<Hash>() + 1 {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let key = Hash::from(&bytes[..size_of::<Hash>()]);
        let expected = Hash::from(&bytes[size_of::<Hash>()..2 * size_of::<Hash>()]);
        let value = Hash::from(&bytes[2 * size_of::<Hash>()..3 * size_of::<Hash>()]);
        let key_type = KeyType::try_from(bytes[3 * size_of::<Hash>()])
            .map_err(|_err| MessageParseError::InvalidFieldLength)?;

        Ok(Self {
            key,
            expected,
            value,
            key_type,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_mutable_cas(
    key: Hash,
    expected: Hash,
    value: Hash,
    key_type: KeyType,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn MutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    check_generic_write_key_type(key_type)?;

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(
        "Handling mutable_cas for key: {} key_type: {:?} in repository: {}",
        key, key_type, repository
    );

    LORE_CONTEXT
        .scope(execution, async move {
            match mutable_store
                .compare_and_swap(repository, key, expected, value, key_type)
                .await
            {
                Ok(current) => {
                    debug!("CAS for key {} returned current: {}", key, current);
                    Ok(LoreResponse::MutableCas(MutableCasResponse {
                        current_value: current,
                    }))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(err) => {
                    warn!(error = ?err, "Failed to CAS mutable key: {}", key);
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

#[async_trait]
impl Message for MutableCas {
    async fn handle_mutable(
        &self,
        context: Arc<AttributeMap>,
        mutable_store: Arc<dyn MutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_mutable_cas(
            self.key,
            self.expected,
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

#[derive(Debug, PartialEq)]
pub struct MutableCasResponse {
    pub current_value: Hash,
}

impl Response for MutableCasResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![Bytes::copy_from_slice(self.current_value.as_bytes())]
    }
}
