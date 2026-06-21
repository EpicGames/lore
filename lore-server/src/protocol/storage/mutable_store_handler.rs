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

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::verify_repository_write_authorization;
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

pub async fn handle_mutable_store(
    key: Hash,
    value: Hash,
    key_type: KeyType,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn MutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
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
        if let Some(token) = context.get::<AuthorizationToken>() {
            verify_repository_write_authorization(&token, repository)
                .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;
        }
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

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::KeyType;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::store::test_store_create;

    #[test]
    fn test_parse() {
        let key = Hash::hash_buffer(b"test-key");
        let value = Hash::hash_buffer(b"test-value");
        let mut bytes = bytes::BytesMut::with_capacity(2 * size_of::<Hash>() + 1);
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(&[KeyType::BranchId as u8]);
        let result = MutableStoreOp::parse(bytes.freeze()).unwrap();
        assert_eq!(result.key, key);
        assert_eq!(result.value, value);
        assert_eq!(result.key_type, KeyType::BranchId);
    }

    #[test]
    fn test_parse_invalid_length() {
        let bytes = Bytes::from_static(&[0u8; 16]);
        assert_eq!(
            MutableStoreOp::parse(bytes),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    #[tokio::test]
    async fn test_handle_store_and_load() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"test-key");
        let value = Hash::hash_buffer(b"test-value");

        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let message = MutableStoreOp {
            key,
            value,
            key_type: KeyType::Untyped,
        };
        LORE_CONTEXT
            .scope(execution, async move {
                let result = message
                    .handle_mutable(context, mutable_store.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    result,
                    LoreResponse::MutableStore(MutableStoreResponse::default())
                );

                // Verify the value was stored
                let loaded = mutable_store
                    .load(repository, key, KeyType::Untyped)
                    .await
                    .unwrap();
                assert_eq!(loaded, value);
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_store_overwrite() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"overwrite-key");
        let first_value = Hash::hash_buffer(b"first");
        let second_value = Hash::hash_buffer(b"second");

        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                let msg1 = MutableStoreOp {
                    key,
                    value: first_value,
                    key_type: KeyType::Untyped,
                };
                msg1.handle_mutable(context.clone(), mutable_store.clone())
                    .await
                    .unwrap();

                let msg2 = MutableStoreOp {
                    key,
                    value: second_value,
                    key_type: KeyType::Untyped,
                };
                msg2.handle_mutable(context, mutable_store.clone())
                    .await
                    .unwrap();

                let loaded = mutable_store
                    .load(repository, key, KeyType::Untyped)
                    .await
                    .unwrap();
                assert_eq!(loaded, second_value);
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_store_independent_keys() {
        let repository = random::<RepositoryId>();
        let key_a = Hash::hash_buffer(b"key-a");
        let key_b = Hash::hash_buffer(b"key-b");
        let value_a = Hash::hash_buffer(b"value-a");
        let value_b = Hash::hash_buffer(b"value-b");

        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                let msg_a = MutableStoreOp {
                    key: key_a,
                    value: value_a,
                    key_type: KeyType::Untyped,
                };
                msg_a
                    .handle_mutable(context.clone(), mutable_store.clone())
                    .await
                    .unwrap();

                let msg_b = MutableStoreOp {
                    key: key_b,
                    value: value_b,
                    key_type: KeyType::Untyped,
                };
                msg_b
                    .handle_mutable(context, mutable_store.clone())
                    .await
                    .unwrap();

                assert_eq!(
                    mutable_store
                        .clone()
                        .load(repository, key_a, KeyType::Untyped)
                        .await
                        .unwrap(),
                    value_a
                );
                assert_eq!(
                    mutable_store
                        .load(repository, key_b, KeyType::Untyped)
                        .await
                        .unwrap(),
                    value_b
                );
            })
            .await;
    }
}
