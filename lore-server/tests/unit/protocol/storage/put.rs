// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_revision::fragment;
use lore_revision::lore::RepositoryId;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::storage::messages::LoreResponse;
use lore_server::protocol::storage::messages::Message;
use lore_server::protocol::storage::messages::MessageHandleError;
use lore_server::protocol::storage::messages::MessageParseError;
use lore_server::protocol::storage::put::*;
use rand::random;

use crate::store::test_support::test_store_create;

fn allow_all() -> Arc<dyn RepositoryAuthorizer> {
    Arc::new(lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer)
}

fn mock_message() -> Put {
    let (fragment, address, payload) = fragment::generate_random();

    Put {
        address,
        fragment,
        payload: Some(payload),
    }
}

mod unvalidated_put {
    use lore_base::types::FRAGMENT_SIZE_THRESHOLD;

    use super::*;

    #[test]
    fn put_valid_with_payload() {
        let (fragment, address, payload) = fragment::generate_random();
        let unvalidated = UnvalidatedPut {
            address,
            fragment,
            payload: Some(payload.clone()),
        };

        let put: Put = unvalidated.validate().unwrap();
        assert_eq!(put.address, address);
        assert_eq!(put.fragment, fragment);
        assert_eq!(put.payload, Some(payload));
    }

    #[test]
    fn put_valid_without_payload() {
        let (fragment, address, _) = fragment::generate_random();
        let unvalidated = UnvalidatedPut {
            address,
            fragment,
            payload: None,
        };

        let put: Put = unvalidated.validate().unwrap();
        assert_eq!(put.address, address);
        assert_eq!(put.fragment, fragment);
        assert_eq!(put.payload, None);
    }

    #[test]
    fn rejects_oversized_fragment() {
        let (mut fragment, address, payload) = fragment::generate_random();
        fragment.size_payload = FRAGMENT_SIZE_THRESHOLD as u32 + 1;
        let unvalidated = UnvalidatedPut {
            address,
            fragment,
            payload: Some(payload),
        };

        assert_eq!(
            unvalidated.validate(),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    #[test]
    fn rejects_payload_length_mismatch() {
        let (fragment, address, _) = fragment::generate_random();
        // Provide a payload whose length doesn't match fragment.size_payload
        let wrong_payload = Bytes::from(vec![0u8; fragment.size_payload as usize + 10]);
        let unvalidated = UnvalidatedPut {
            address,
            fragment,
            payload: Some(wrong_payload),
        };

        assert_eq!(
            unvalidated.validate(),
            Err(MessageParseError::InvalidFieldLength)
        );
    }
}

#[test]
fn test_parse() {
    let message = mock_message();

    let message_bytes = message.to_bytes();

    assert_eq!(Put::parse(message_bytes), Ok(message));
}

#[tokio::test]
async fn test_handle() {
    let message = mock_message();

    let repository = random::<RepositoryId>();

    let context = Arc::new(AttributeMap::default());
    context.insert(repository);

    let (immutable_store, _mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    assert_eq!(
        LoreResponse::Put(PutResponse::default()),
        message
            .handle(context, immutable_store, allow_all())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn test_hash_mismatch() {
    let mut message = mock_message();
    message.address.hash = Hash::hash_buffer(b"some bad hash");

    let repository = random::<RepositoryId>();

    let context = Arc::new(AttributeMap::default());
    context.insert(repository);

    let (immutable_store, _mutable_store, _execution) =
        test_store_create().await.expect("Failed to create stores");

    match message.handle(context, immutable_store, allow_all()).await {
        Err(MessageHandleError::HashMismatch) => (),
        Err(e) => panic!("Expected hash mismatch error, but got {e:?}"),
        _ => panic!("Expected hash mismatch error"),
    }
}

mod validate_fragment {
    // Metadata-only validation (flags, size_payload vs size_content,
    // compressed+fragmented exclusion, etc.) is covered by
    // `lore_storage::validate_fragment_metadata` and its tests. The tests
    // here exercise `Put::validate_fragment`, which now only runs the
    // payload-dependent fragment-list checks.
    use lore_base::types::FragmentReference;
    use zerocopy::IntoBytes;

    use super::*;

    /// Encodes `refs` as a fragment list payload. The tests give each reference that names a
    /// fragment a nonzero hash: a zero hash is a hole, and a list holding a hole is checked under
    /// further rules.
    fn make_fragment_ref_payload(refs: &[FragmentReference]) -> Bytes {
        Bytes::copy_from_slice(refs.as_bytes())
    }

    fn fragmented_put(refs: &[FragmentReference], size_content: u64) -> Put {
        let payload = make_fragment_ref_payload(refs);
        let hash = Hash::hash_buffer(payload.as_ref());
        Put {
            address: Address {
                hash,
                context: rand::random(),
            },
            fragment: Fragment {
                flags: FragmentFlags::PayloadFragmented.into(),
                size_payload: payload.len() as u32,
                size_content,
            },
            payload: Some(payload),
        }
    }

    #[test]
    fn uncompressed_unfragmented_ok() {
        let (fragment, address, payload) = fragment::generate_random();
        let put = Put {
            address,
            fragment,
            payload: Some(payload),
        };
        assert!(put.validate_fragment().is_ok());
    }

    #[test]
    fn compressed_unfragmented_ok() {
        let put = Put {
            address: Address::default(),
            fragment: Fragment {
                flags: FragmentFlags::PayloadCompressedLZ4.into(),
                size_payload: 100,
                size_content: 200,
            },
            payload: None,
        };
        assert!(put.validate_fragment().is_ok());
    }

    #[test]
    fn fragmented_valid_two_refs() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 1000,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(put.validate_fragment().is_ok());
    }

    #[test]
    fn fragmented_valid_three_refs() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 1000,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(put.validate_fragment().is_ok());
    }

    #[test]
    fn fragmented_fewer_than_two_refs_one() {
        let refs = [FragmentReference {
            hash: Hash::from_u64(1),
            offset_content: 0,
        }];
        let put = fragmented_put(&refs, 2000);
        assert!(matches!(
            put.validate_fragment(),
            Err(MessageHandleError::InvalidFragment)
        ));
    }

    #[test]
    fn fragmented_fewer_than_two_refs_zero() {
        let payload = Bytes::from_static(&[0u8; 4]);
        let hash = Hash::hash_buffer(payload.as_ref());
        let put = Put {
            address: Address {
                hash,
                context: rand::random(),
            },
            fragment: Fragment {
                flags: FragmentFlags::PayloadFragmented.into(),
                size_payload: payload.len() as u32,
                size_content: 2000,
            },
            payload: Some(payload),
        };
        assert!(matches!(
            put.validate_fragment(),
            Err(MessageHandleError::InvalidFragment)
        ));
    }

    #[test]
    fn fragmented_first_offset_nonzero() {
        // Non-zero first offset is valid (multi-level fragment list child blob)
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 10,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 1000,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(put.validate_fragment().is_ok());
    }

    #[test]
    fn fragmented_offset_span_exceeds_content_size() {
        // Span (last - first) must be less than size_content
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 3000,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(matches!(
            put.validate_fragment(),
            Err(MessageHandleError::InvalidFragment)
        ));
    }

    #[test]
    fn fragmented_offsets_not_increasing() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 1000,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(matches!(
            put.validate_fragment(),
            Err(MessageHandleError::InvalidFragment)
        ));
    }

    #[test]
    fn fragmented_offsets_equal() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(matches!(
            put.validate_fragment(),
            Err(MessageHandleError::InvalidFragment)
        ));
    }

    #[test]
    fn fragmented_last_offset_equals_content_size() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 2000,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(matches!(
            put.validate_fragment(),
            Err(MessageHandleError::InvalidFragment)
        ));
    }

    #[test]
    fn fragmented_last_offset_exceeds_content_size() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 3000,
            },
        ];
        let put = fragmented_put(&refs, 2000);
        assert!(matches!(
            put.validate_fragment(),
            Err(MessageHandleError::InvalidFragment)
        ));
    }

    #[test]
    fn fragmented_no_payload_skips_fragment_list_validation() {
        // No payload, no fragment list to walk: this check defers.
        // `validate_hash` refuses such a put — see
        // `put_without_payload_is_refused`. Both run in `handle_put`.
        let put = Put {
            address: Address::default(),
            fragment: Fragment {
                flags: FragmentFlags::PayloadFragmented.into(),
                size_payload: 80,
                size_content: 2000,
            },
            payload: None,
        };
        assert!(put.validate_fragment().is_ok());
    }

    #[test]
    fn put_without_payload_is_refused() {
        // No payload, nothing to hash: refuse rather than pass. The store
        // deduplicates on hash across partitions, so an unproven address
        // would associate another partition's content with the caller's.
        let put = Put {
            address: Address::default(),
            fragment: Fragment {
                flags: 0,
                size_payload: 80,
                size_content: 80,
            },
            payload: None,
        };
        assert!(matches!(
            put.validate_hash(),
            Err(MessageHandleError::HashFailed)
        ));
    }
}
