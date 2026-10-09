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
use lore_server::protocol::storage::mutable_cas::*;
use rand::random;
use zerocopy::IntoBytes;

use crate::store::test_support::test_store_create;

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

/// Repository and branch metadata and a branch's latest pointer are written
/// only through requests that validate the write; a compare-and-swap through
/// the generic path would skip that, so it is refused and the stored value is
/// left as it was.
#[tokio::test]
async fn rejects_key_types_with_a_dedicated_write_request() {
    let repository = random::<RepositoryId>();
    let key = Hash::hash_buffer(b"cas-protected-key");
    let initial_value = Hash::hash_buffer(b"initial");
    let new_value = Hash::hash_buffer(b"new");

    for key_type in [
        KeyType::RepositoryMetadata,
        KeyType::BranchMetadata,
        KeyType::BranchLatestPointer,
    ] {
        let context = Arc::new(AttributeMap::default());
        context.insert(repository);

        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                mutable_store
                    .clone()
                    .store(repository, key, initial_value, key_type)
                    .await
                    .expect("seeding the protected key");

                let message = MutableCas {
                    key,
                    expected: initial_value,
                    value: new_value,
                    key_type,
                };
                let error = message
                    .handle_mutable(context, mutable_store.clone())
                    .await
                    .expect_err("a protected key type must be refused on the generic path");
                let MessageHandleError::InvalidArgument(reason) = error else {
                    panic!("{key_type:?} must be refused as an invalid argument, got {error:?}");
                };
                assert!(
                    reason.contains(&format!("{key_type:?}")),
                    "the reason must name {key_type:?}, got {reason:?}"
                );

                let loaded = mutable_store
                    .load(repository, key, key_type)
                    .await
                    .expect("loading the protected key");
                assert_eq!(loaded, initial_value);
            })
            .await;
    }
}
