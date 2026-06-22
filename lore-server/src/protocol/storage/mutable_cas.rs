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
        if let Some(token) = context.get::<AuthorizationToken>() {
            verify_repository_write_authorization(&token, repository)
                .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;
        }
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
        let key = Hash::hash_buffer(b"key");
        let expected = Hash::hash_buffer(b"expected");
        let value = Hash::hash_buffer(b"value");
        let mut bytes = bytes::BytesMut::with_capacity(3 * size_of::<Hash>() + 1);
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(expected.as_bytes());
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(&[KeyType::RepositoryMetadata as u8]);
        let result = MutableCas::parse(bytes.freeze()).unwrap();
        assert_eq!(result.key, key);
        assert_eq!(result.expected, expected);
        assert_eq!(result.value, value);
        assert_eq!(result.key_type, KeyType::RepositoryMetadata);
    }

    #[test]
    fn test_parse_invalid_length() {
        let bytes = Bytes::from_static(&[0u8; 32]);
        assert_eq!(
            MutableCas::parse(bytes),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    #[tokio::test]
    async fn test_handle_cas_success() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"cas-key");
        let initial_value = Hash::hash_buffer(b"initial");
        let new_value = Hash::hash_buffer(b"new");

        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                // Store initial value
                mutable_store
                    .clone()
                    .store(repository, key, initial_value, KeyType::Untyped)
                    .await
                    .unwrap();

                // CAS with correct expected value
                let message = MutableCas {
                    key,
                    expected: initial_value,
                    value: new_value,
                    key_type: KeyType::Untyped,
                };
                let result = message
                    .handle_mutable(context, mutable_store)
                    .await
                    .unwrap();

                // Successful CAS returns the previous value (which equals expected)
                assert_eq!(
                    result,
                    LoreResponse::MutableCas(MutableCasResponse {
                        current_value: initial_value
                    })
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_cas_failure() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"cas-fail-key");
        let initial_value = Hash::hash_buffer(b"initial");
        let wrong_expected = Hash::hash_buffer(b"wrong");
        let new_value = Hash::hash_buffer(b"new");

        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                mutable_store
                    .clone()
                    .store(repository, key, initial_value, KeyType::Untyped)
                    .await
                    .unwrap();

                // CAS with wrong expected value — should not swap
                let message = MutableCas {
                    key,
                    expected: wrong_expected,
                    value: new_value,
                    key_type: KeyType::Untyped,
                };
                let result = message
                    .handle_mutable(context, mutable_store.clone())
                    .await
                    .unwrap();

                // Returns the actual current value (not the wrong expected)
                assert_eq!(
                    result,
                    LoreResponse::MutableCas(MutableCasResponse {
                        current_value: initial_value
                    })
                );

                // Value should be unchanged
                let loaded = mutable_store
                    .load(repository, key, KeyType::Untyped)
                    .await
                    .unwrap();
                assert_eq!(loaded, initial_value);
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_cas_verifies_new_value() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"cas-verify-key");
        let initial_value = Hash::hash_buffer(b"initial");
        let new_value = Hash::hash_buffer(b"updated");

        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                mutable_store
                    .clone()
                    .store(repository, key, initial_value, KeyType::Untyped)
                    .await
                    .unwrap();

                let message = MutableCas {
                    key,
                    expected: initial_value,
                    value: new_value,
                    key_type: KeyType::Untyped,
                };
                message
                    .handle_mutable(context, mutable_store.clone())
                    .await
                    .unwrap();

                // Verify the value was actually updated
                let loaded = mutable_store
                    .load(repository, key, KeyType::Untyped)
                    .await
                    .unwrap();
                assert_eq!(loaded, new_value);
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_cas_on_nonexistent_key() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"cas-missing-key");
        let expected = Hash::default();
        let new_value = Hash::hash_buffer(b"new");

        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                // CAS on a key that doesn't exist, expecting zero hash
                let message = MutableCas {
                    key,
                    expected,
                    value: new_value,
                    key_type: KeyType::Untyped,
                };
                let result = message
                    .handle_mutable(context, mutable_store.clone())
                    .await
                    .unwrap();

                // Should succeed with previous value being zero (default)
                assert_eq!(
                    result,
                    LoreResponse::MutableCas(MutableCasResponse {
                        current_value: expected
                    })
                );

                // Value should now be set
                let loaded = mutable_store
                    .load(repository, key, KeyType::Untyped)
                    .await
                    .unwrap();
                assert_eq!(loaded, new_value);
            })
            .await;
    }
}
