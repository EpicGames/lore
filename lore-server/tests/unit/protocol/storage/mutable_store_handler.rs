// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::lore::RepositoryId;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::storage::messages::LoreResponse;
use lore_server::protocol::storage::messages::Message;
use lore_server::protocol::storage::messages::MessageHandleError;
use lore_server::protocol::storage::messages::MessageParseError;
use lore_server::protocol::storage::mutable_store_handler::*;
use rand::random;
use zerocopy::IntoBytes;

use crate::store::test_support::test_store_create;

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

/// The dedicated APIs (`RepositoryMetadataSet`, `BranchMetadataSet`) validate
/// read-only fields; reaching the same keys through the generic store would
/// skip that.
#[tokio::test]
async fn rejects_repository_and_branch_metadata_key_types() {
    let repository = random::<RepositoryId>();

    for key_type in [KeyType::RepositoryMetadata, KeyType::BranchMetadata] {
        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let message = MutableStoreOp {
            key: Hash::hash_buffer(b"test-key"),
            value: Hash::hash_buffer(b"test-value"),
            key_type,
        };
        let result = LORE_CONTEXT
            .scope(execution, async move {
                message.handle_mutable(context, mutable_store.clone()).await
            })
            .await;

        assert!(
            matches!(result, Err(MessageHandleError::AuthorizationFailure(_))),
            "{key_type:?} must be refused on the generic path, got {result:?}"
        );
    }
}
