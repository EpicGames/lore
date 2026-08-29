// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `get_resolved`: `mutable_load` + `get` performed server-side, saving the caller one round
//! trip. The resolved hash is returned so the caller can cache the key->hash mapping and
//! verify the payload.
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use tracing::debug;
use tracing::info;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::util::setup_execution;

/// `flags` bits this build accepts. Reserved; none are defined. Unknown bits are rejected.
pub const KNOWN_FLAGS: u32 = 0;

/// Wire width of `flags` in bytes, sized so the request stays a multiple of 4.
pub const FLAGS_WIRE_SIZE: usize = size_of::<u32>();

/// Wire request: key `Hash` (32) ++ `Context` (16) ++ `flags` u32 LE (4).
///
/// No key type is carried: the key is always read as [`KeyType::Resolve`].
#[derive(Clone, Debug, PartialEq)]
pub struct GetResolved {
    pub key: Hash,
    /// Paired with the resolved hash to address the immutable read; the mutable store yields
    /// only a hash.
    pub context: Context,
    /// See [`KNOWN_FLAGS`].
    pub flags: u32,
}

impl GetResolved {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        const KEY: usize = size_of::<Hash>();
        const CTX: usize = size_of::<Context>();
        if bytes.len() < KEY + CTX + FLAGS_WIRE_SIZE {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let key = Hash::from(&bytes[..KEY]);
        let context = Context::from(&bytes[KEY..KEY + CTX]);
        let mut flag_bytes = [0u8; FLAGS_WIRE_SIZE];
        flag_bytes.copy_from_slice(&bytes[KEY + CTX..KEY + CTX + FLAGS_WIRE_SIZE]);
        let flags = u32::from_le_bytes(flag_bytes);

        Ok(Self {
            key,
            context,
            flags,
        })
    }
}

/// Resolve `key` as a [`KeyType::Resolve`] mapping and return the immutable blob it names.
#[allow(clippy::too_many_arguments)]
pub async fn handle_get_resolved(
    key: Hash,
    context: Context,
    flags: u32,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn MutableStore>,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(
        "Handling get_resolved for key: {} in repository: {}",
        key, repository
    );

    if flags & !KNOWN_FLAGS != 0 {
        warn!(
            "get_resolved: unsupported flags {:#x} (known: {:#x})",
            flags, KNOWN_FLAGS
        );
        return Err(MessageHandleError::NotImplemented);
    }

    LORE_CONTEXT
        .scope(execution, async move {
            let resolved = match mutable_store.load(repository, key, KeyType::Resolve).await {
                Ok(value) => value,
                Err(StoreError::SlowDown(_)) => return Err(MessageHandleError::SlowDown),
                Err(StoreError::AddressNotFound(_)) => {
                    info!("get_resolved: mutable key not found: {}", key);
                    return Err(MessageHandleError::MutableDataNotFound(key));
                }
                Err(err) => {
                    warn!(error = ?err, "get_resolved: failed to load mutable key: {}", key);
                    return Err(MessageHandleError::StoreFailure);
                }
            };

            let address = Address {
                hash: resolved,
                context,
            };
            match immutable_store.get(repository, address).await {
                Ok(data) => {
                    let mut fragment = data.fragment;
                    let Some(payload) = data.payload else {
                        debug!("get_resolved: key {key} resolved to {resolved} with no payload");
                        return Err(MessageHandleError::FragmentNotFound);
                    };
                    debug!(
                        "get_resolved: key {} -> {} ({} payload / {} content bytes)",
                        key, resolved, fragment.size_payload, fragment.size_content
                    );
                    fragment.flags &= !FragmentFlags::PayloadStored;
                    fragment.flags |= FragmentFlags::PayloadStoredDurable;
                    Ok(LoreResponse::GetResolved(GetResolvedResponse {
                        resolved,
                        fragment,
                        payload,
                    }))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(StoreError::AddressNotFound(_)) => {
                    info!(
                        "get_resolved: key {} resolved to {} but no fragment was found",
                        key, resolved
                    );
                    Err(MessageHandleError::FragmentNotFound)
                }
                Err(err) => {
                    warn!(error = ?err, "get_resolved: failed to get fragment for {}", address);
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

/// Dispatched from the v4 path: the defaults return `NotImplemented`, since v0 supplies only one
/// store and this needs both.
impl Message for GetResolved {}

#[derive(Debug, PartialEq)]
pub struct GetResolvedResponse {
    /// Hash the key resolved to.
    pub resolved: Hash,
    pub fragment: Fragment,
    pub payload: Bytes,
}

impl Response for GetResolvedResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![
            Bytes::copy_from_slice(self.resolved.as_bytes()),
            Bytes::copy_from_slice(self.fragment.as_bytes()),
            self.payload.clone(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use rand::random;

    use super::*;
    use crate::store::test_store_create;

    fn request_bytes(key: Hash, context: Context, flags: u32) -> Bytes {
        let mut bytes = bytes::BytesMut::with_capacity(
            size_of::<Hash>() + size_of::<Context>() + FLAGS_WIRE_SIZE,
        );
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(context.as_bytes());
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.freeze()
    }

    #[test]
    fn test_request_is_four_byte_aligned() {
        let len = request_bytes(Hash::default(), Context::default(), 0).len();
        assert_eq!(len, 52);
        assert_eq!(len % 4, 0, "request should stay a multiple of 4 bytes");
    }

    #[test]
    fn test_parse() {
        let key = Hash::hash_buffer(b"test-key");
        let context = Context::default();
        let parsed = GetResolved::parse(request_bytes(key, context, 0)).unwrap();
        assert_eq!(parsed.key, key);
        assert_eq!(parsed.context, context);
        assert_eq!(parsed.flags, 0);
    }

    #[test]
    fn test_parse_preserves_all_flag_bits() {
        let key = Hash::hash_buffer(b"test-key");
        for flags in [1u32, 0x80, 0xFF_FF, 0x00FF_FFFF, u32::MAX] {
            let parsed = GetResolved::parse(request_bytes(key, Context::default(), flags)).unwrap();
            assert_eq!(parsed.flags, flags, "flags {flags:#x} did not round trip");
        }
    }

    #[test]
    fn test_parse_invalid_length() {
        let bytes = Bytes::from(vec![
            0u8;
            size_of::<Hash>()
                + size_of::<Context>()
                + FLAGS_WIRE_SIZE
                - 1
        ]);
        assert_eq!(
            GetResolved::parse(bytes),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    #[tokio::test]
    async fn test_missing_key_is_mutable_not_found() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"missing-key");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let result = LORE_CONTEXT
            .scope(execution, async move {
                handle_get_resolved(
                    key,
                    Context::default(),
                    0,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await;

        assert!(matches!(
            result,
            Err(MessageHandleError::MutableDataNotFound(_))
        ));
    }

    #[tokio::test]
    async fn test_dangling_pointer_is_fragment_not_found() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"dangling-key");
        let value = Hash::hash_buffer(b"never-stored");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let result = LORE_CONTEXT
            .scope(execution, async move {
                mutable_store
                    .clone()
                    .store(repository, key, value, KeyType::Resolve)
                    .await
                    .unwrap();
                handle_get_resolved(
                    key,
                    Context::default(),
                    0,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await;

        assert!(matches!(result, Err(MessageHandleError::FragmentNotFound)));
    }

    #[tokio::test]
    async fn test_unknown_flag_rejected() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let result = LORE_CONTEXT
            .scope(execution, async move {
                handle_get_resolved(
                    Hash::hash_buffer(b"any-key"),
                    Context::default(),
                    1, // no bits are defined yet, so any bit must be refused
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await;

        assert!(matches!(result, Err(MessageHandleError::NotImplemented)));
    }
}
