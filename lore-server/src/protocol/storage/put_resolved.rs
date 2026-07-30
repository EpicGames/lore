// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `put_resolved`: `put` + `mutable_store` performed server-side, saving the caller one round
//! trip. The write side of [`super::get_resolved`], and the only thing that makes a key readable
//! by it.
//!
//! The fragment is stored through [`handle_put`] rather than a parallel implementation, so it
//! inherits `put`'s hash and fragment validation exactly. Only once that succeeds is the
//! `KeyType::Resolve` mapping published, so a key never names content the server does not hold —
//! the ordering the revision layer uses for branch pointers, and the one `read_resolved`'s
//! write-back already follows on the client.
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use tracing::debug;
use tracing::warn;

use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::protocol::storage::put::Put;
use crate::protocol::storage::put::UnvalidatedPut;
use crate::protocol::storage::put::handle_put;
use crate::util::setup_execution;

/// Wire request: key `Hash` (32) ++ `Address` (48) ++ `Fragment` (16) ++ payload.
#[derive(Clone, Debug, PartialEq)]
pub struct PutResolved {
    /// Mutable key to publish the stored hash under.
    pub key: Hash,
    /// Content address of the fragment; `address.hash` is what `key` will resolve to. Held
    /// alongside `put` because `Put` does not expose it.
    pub address: Address,
    put: Put,
}

impl PutResolved {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        const KEY: usize = size_of::<Hash>();
        if bytes.len() < KEY + size_of::<Address>() + size_of::<Fragment>() {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let mut bytes = bytes;
        let key = Hash::from(&bytes.split_to(KEY)[..]);
        if key.is_zero() {
            // A zero key is a tombstone value in the mutable store, never a storable key.
            return Err(MessageParseError::ParseFailure(
                "put_resolved: key must be non-zero",
            ));
        }

        let address: Address = bytes.split_to(size_of::<Address>()).into();
        let fragment: Fragment = bytes.split_to(size_of::<Fragment>()).into();
        let payload = if bytes.is_empty() { None } else { Some(bytes) };

        let put = UnvalidatedPut {
            address,
            fragment,
            payload,
        }
        .validate()?;

        Ok(Self { key, address, put })
    }

    /// The validated fragment write this request carries.
    pub fn put(&self) -> &Put {
        &self.put
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_put_resolved(
    key: Hash,
    put: &Put,
    address: Address,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn MutableStore>,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    // Step 1: store the fragment with `put`'s own validation and semantics. A failure here
    // leaves no mapping behind, which is the whole point of doing it first.
    handle_put(
        put,
        repository,
        correlation_id.clone(),
        user_id.clone(),
        immutable_store,
    )
    .await?;

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    LORE_CONTEXT
        .scope(execution, async move {
            // Step 2: publish the mapping now that the content behind it is durable.
            match mutable_store
                .store(repository, key, address.hash, KeyType::Resolve)
                .await
            {
                Ok(()) => {
                    debug!(
                        "put_resolved: key {} -> {} in repository {}",
                        key, address.hash, repository
                    );
                    Ok(LoreResponse::PutResolved(PutResolvedResponse::default()))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(err) => {
                    // The fragment is stored but unreachable by key. Reporting failure lets the
                    // caller retry; a retry re-puts the same content address idempotently.
                    warn!(error = ?err, "put_resolved: stored {} but failed to map key {}", address.hash, key);
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

// Needs both stores; the v0 message path supplies only one, so the defaulted trait methods
// return `NotImplemented` and dispatch happens in the v4 path.
impl Message for PutResolved {}

#[derive(Debug, Default, PartialEq)]
pub struct PutResolvedResponse {}

impl Response for PutResolvedResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use lore_base::types::FragmentFlags;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::store::test_store_create;

    fn request_bytes(key: Hash, address: Address, fragment: Fragment, payload: &[u8]) -> Bytes {
        let mut bytes = bytes::BytesMut::new();
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(address.as_bytes());
        bytes.extend_from_slice(fragment.as_bytes());
        bytes.extend_from_slice(payload);
        bytes.freeze()
    }

    fn fragment_for(payload: &[u8]) -> (Address, Fragment) {
        let hash = lore_storage::hash_slice(payload);
        (
            Address {
                hash,
                context: Default::default(),
            },
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
        )
    }

    #[test]
    fn test_parse_round_trips_key_address_and_payload() {
        let payload = b"put-resolved-parse".as_slice();
        let (address, fragment) = fragment_for(payload);
        let key = Hash::hash_buffer(b"parse-key");
        let parsed = PutResolved::parse(request_bytes(key, address, fragment, payload)).unwrap();
        assert_eq!(parsed.key, key);
        assert_eq!(parsed.address, address);
    }

    #[test]
    fn test_parse_rejects_zero_key() {
        let payload = b"put-resolved-zero-key".as_slice();
        let (address, fragment) = fragment_for(payload);
        assert!(matches!(
            PutResolved::parse(request_bytes(Hash::default(), address, fragment, payload)),
            Err(MessageParseError::ParseFailure(_))
        ));
    }

    #[test]
    fn test_parse_invalid_length() {
        let bytes = Bytes::from(vec![0u8; size_of::<Hash>() + size_of::<Address>()]);
        assert_eq!(
            PutResolved::parse(bytes),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    /// The mapping must be readable straight after the call, and must name the stored content.
    #[tokio::test]
    async fn test_stores_fragment_then_publishes_mapping() {
        let repository = random::<RepositoryId>();
        let payload = b"put-resolved-round-trip".as_slice();
        let (address, fragment) = fragment_for(payload);
        let key = Hash::hash_buffer(b"publish-key");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let parsed =
            PutResolved::parse(request_bytes(key, address, fragment, payload)).expect("parse");

        LORE_CONTEXT
            .scope(execution, async move {
                handle_put_resolved(
                    parsed.key,
                    parsed.put(),
                    parsed.address,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store.clone(),
                    immutable_store.clone(),
                )
                .await
                .expect("put_resolved must succeed");

                let mapped = mutable_store
                    .load(repository, key, KeyType::Resolve)
                    .await
                    .expect("mapping must exist after put_resolved");
                assert_eq!(mapped, address.hash, "key must resolve to the stored hash");

                let (_, stored) = immutable_store
                    .get(repository, address, lore_storage::StoreMatch::MatchFull)
                    .await
                    .expect("fragment must be stored");
                assert_eq!(stored.as_ref(), payload);
            })
            .await;
    }

    /// A payload that does not hash to the advertised address must be refused, and must leave no
    /// mapping behind — otherwise a rejected write would still publish the key.
    #[tokio::test]
    async fn test_hash_mismatch_leaves_no_mapping() {
        let repository = random::<RepositoryId>();
        let payload = b"put-resolved-mismatch".as_slice();
        let (_, fragment) = fragment_for(payload);
        let wrong_address = Address {
            hash: Hash::hash_buffer(b"not-the-payload-hash"),
            context: Default::default(),
        };
        let key = Hash::hash_buffer(b"mismatch-key");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let parsed = PutResolved::parse(request_bytes(key, wrong_address, fragment, payload))
            .expect("parse accepts it; the hash check happens in the handler");

        LORE_CONTEXT
            .scope(execution, async move {
                let result = handle_put_resolved(
                    parsed.key,
                    parsed.put(),
                    parsed.address,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store.clone(),
                    immutable_store,
                )
                .await;

                assert!(matches!(result, Err(MessageHandleError::HashMismatch)));
                assert!(
                    mutable_store
                        .load(repository, key, KeyType::Resolve)
                        .await
                        .is_err(),
                    "a rejected fragment must not publish its key"
                );
            })
            .await;
    }
}
