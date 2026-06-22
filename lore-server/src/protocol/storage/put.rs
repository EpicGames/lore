// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt;
use std::fmt::Formatter;
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
use lore_storage::validate_fragment_list;
use lore_storage::validate_fragment_metadata;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::ADDRESS;
use opentelemetry::metrics::Histogram;
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
pub struct Put {
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnvalidatedPut {
    pub address: Address,
    pub fragment: Fragment,
    pub payload: Option<Bytes>,
}

impl UnvalidatedPut {
    pub fn validate(self) -> Result<Put, MessageParseError> {
        validate_fragment_metadata(&self.fragment).map_err(|err| {
            warn!(
                fragment = ?self.fragment,
                error = ?err,
                "Put rejected: invalid fragment metadata"
            );
            MessageParseError::InvalidFieldLength
        })?;

        if let Some(payload) = &self.payload
            && payload.len() != self.fragment.size_payload as usize
        {
            return Err(MessageParseError::InvalidFieldLength);
        }

        Ok(Put {
            address: self.address,
            fragment: self.fragment,
            payload: self.payload,
        })
    }
}

struct PutInstrumentProvider;

impl InstrumentProvider for PutInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.quic.message.put"
    }
}

impl PutInstrumentProvider {
    fn payload_size_histogram(&self) -> Histogram<u64> {
        self.size_histogram("payload_size")
    }

    fn content_size_histogram(&self) -> Histogram<u64> {
        self.size_histogram("content_size")
    }
}

struct PutInstrument {
    provider: &'static PutInstrumentProvider,
    payload_size_histogram: Histogram<u64>,
    content_size_histogram: Histogram<u64>,
}

impl PutInstrument {
    fn payload_size(&self, size: u32) {
        self.payload_size_histogram
            .record(size as u64, self.provider.labels());
    }

    fn content_size(&self, size: u64) {
        self.content_size_histogram
            .record(size, self.provider.labels());
    }
}

fn instruments() -> &'static PutInstrument {
    static PROVIDER: OnceLock<PutInstrumentProvider> = OnceLock::new();
    static INSTRUMENTS: OnceLock<PutInstrument> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let provider = PROVIDER.get_or_init(|| PutInstrumentProvider);
        PutInstrument {
            provider,
            payload_size_histogram: provider.payload_size_histogram(),
            content_size_histogram: provider.content_size_histogram(),
        }
    })
}

impl Put {
    pub fn address(&self) -> &Address {
        &self.address
    }

    fn validate_hash(&self) -> Result<(), MessageHandleError> {
        if let Some(payload) = self.payload.as_ref() {
            match lore_storage::hash_fragment(self.fragment, payload.as_ref()) {
                Ok(hash) => {
                    if hash != self.address.hash {
                        warn!(
                            fragment = ?self.fragment,
                            {ADDRESS} = %self.address,
                            computed_hash = %hash,
                            "Hash validation failed, computed hash does not match address"
                        );
                        return Err(MessageHandleError::HashMismatch);
                    }
                }
                Err(err) => {
                    warn!(
                        fragment = ?self.fragment,
                        {ADDRESS} = %self.address,
                        error = ?err,
                        "Hash validation failed, unable to hash"
                    );
                    return Err(MessageHandleError::HashFailed);
                }
            }
        }
        Ok(())
    }

    fn validate_fragment(&self) -> Result<(), MessageHandleError> {
        // Metadata checks (size bounds, flag sanity, size_payload ≤ size_content,
        // uncompressed/unfragmented size equality, compressed+fragmented exclusion,
        // etc.) are performed at ingress in `UnvalidatedPut::validate`. Here we
        // only need the payload-dependent checks for fragment lists.
        if self.fragment.flags & FragmentFlags::PayloadFragmented != 0
            && let Some(payload) = self.payload.as_ref()
        {
            validate_fragment_list(&self.fragment, payload).map_err(|err| {
                warn!(
                    fragment = ?self.fragment,
                    {ADDRESS} = %self.address,
                    error = ?err,
                    "Fragment validation failed"
                );
                MessageHandleError::InvalidFragment
            })?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn to_bytes(&self) -> Bytes {
        use zerocopy::IntoBytes;

        let mut bytes =
            bytes::BytesMut::with_capacity(size_of::<Address>() + size_of::<Fragment>());
        bytes.extend_from_slice(self.address.as_bytes());
        bytes.extend_from_slice(self.fragment.as_bytes());
        if let Some(payload) = self.payload.as_ref() {
            bytes.extend_from_slice(payload.as_ref());
        }
        bytes.freeze()
    }
}

impl fmt::Display for Put {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:#?}", self.fragment)
    }
}

impl Put {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        if bytes.len() < size_of::<Address>() + size_of::<Fragment>() {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let mut bytes = bytes;
        let address = bytes.split_to(size_of::<Address>()).into();
        let fragment: Fragment = bytes.split_to(size_of::<Fragment>()).into();

        let payload = if bytes.is_empty() { None } else { Some(bytes) };

        let unvalidated = UnvalidatedPut {
            address,
            fragment,
            payload,
        };
        unvalidated.validate()
    }
}

pub async fn handle_put(
    put: &Put,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(
        "Handling PutFragment request for fragment with address: {} ({} bytes payload) in repository: {}",
        put.address,
        put.payload
            .as_ref()
            .map(|payload| payload.len())
            .unwrap_or_default(),
        repository
    );

    let instruments = instruments();
    instruments.payload_size(put.fragment.size_payload);
    instruments.content_size(put.fragment.size_content);

    let address = put.address;
    let fragment = put.fragment;
    let payload = put.payload.clone();

    LORE_CONTEXT
        .scope(execution, async move {
            put.validate_fragment()?;
            put.validate_hash()?;

            let mut fragment = fragment;
            fragment.flags &= !FragmentFlags::PayloadStored;

            match immutable_store
                .put(repository, address, fragment, payload, false)
                .await
            {
                Ok(_) => {
                    debug!("Successfully stored fragment for address: {}", address);
                    Ok(LoreResponse::Put(PutResponse::default()))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(err) => {
                    warn!(error = ?err, {ADDRESS} = %address, "Failed to put fragment for address");
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

#[async_trait]
impl Message for Put {
    #[tracing::instrument(name = "Put::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        if let Some(token) = context.get::<AuthorizationToken>() {
            verify_repository_write_authorization(&token, repository)
                .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;
        }
        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_put(
            self,
            repository,
            correlation_id.to_string(),
            user_id,
            immutable_store,
        )
        .await
    }
}

impl InstrumentProvider for Put {
    fn namespace(&self) -> &'static str {
        "quic.message.put"
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct PutResponse {}

impl Response for PutResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use lore_base::types::Hash;
    use lore_revision::fragment;
    use rand::random;

    use super::*;
    use crate::store::test_store_create;

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
            message.handle(context, immutable_store).await.unwrap()
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

        match message.handle(context, immutable_store).await {
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
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
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
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 500,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 1000,
                },
            ];
            let put = fragmented_put(&refs, 2000);
            assert!(put.validate_fragment().is_ok());
        }

        #[test]
        fn fragmented_fewer_than_two_refs_one() {
            let refs = [FragmentReference {
                hash: Hash::default(),
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
                    hash: Hash::default(),
                    offset_content: 10,
                },
                FragmentReference {
                    hash: Hash::default(),
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
                    hash: Hash::default(),
                    offset_content: 500,
                },
                FragmentReference {
                    hash: Hash::default(),
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
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 1000,
                },
                FragmentReference {
                    hash: Hash::default(),
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
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
                    offset_content: 500,
                },
                FragmentReference {
                    hash: Hash::default(),
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
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
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
                    hash: Hash::default(),
                    offset_content: 0,
                },
                FragmentReference {
                    hash: Hash::default(),
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
        fn fragmented_dedup_put_no_payload_ok() {
            // Dedup puts (payload omitted by the client; server relies on
            // existing fragment in the repository) skip fragment-list
            // validation because there's nothing to validate. Metadata
            // validation — which runs earlier in the flow — already covers
            // these fragments.
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
    }
}
