// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::TypedBytes;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_transport::quic::storage_service::QueryStatus;
use tracing::debug;

use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;

pub const MAX_FRAGMENTS: usize = lore_base::types::FRAGMENT_SIZE_THRESHOLD / size_of::<Address>();
const MAX_FRAGMENTS_LENGTH: usize = size_of::<Address>() * MAX_FRAGMENTS;

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub address: Bytes,
}

impl Query {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        let length = bytes.len();
        if !length.is_multiple_of(size_of::<Address>()) {
            return Err(MessageParseError::InvalidQueryLength);
        }

        if length > MAX_FRAGMENTS_LENGTH {
            return Err(MessageParseError::TooManyFragments(
                MAX_FRAGMENTS,
                length / size_of::<Address>(),
            ));
        }

        Ok(Self { address: bytes })
    }
}

/// The one place a resolved level is turned into something a client is told.
///
/// Both client-facing protocols land here - the QUIC message and the gRPC v1 handler - so the
/// collapse that keeps one tenant from learning about another's content is applied once, in the
/// `match` below, rather than restated per protocol. Anything that starts answering `Query`
/// without coming through here has to bring the rule with it.
pub async fn handle_query(
    address: &Bytes,
    repository: RepositoryId,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let address_count = address.count::<Address>();
    debug!("Handling Query request to find {address_count} fragments in repository: {repository}");

    let addresses = address.as_type_slice::<Address>();
    let mut resolved = vec![StoreMatchResult::default(); addresses.len()];
    immutable_store
        .query(repository, addresses, &mut resolved)
        .await?;

    Ok(LoreResponse::Query(QueryResponse {
        results: Bytes::from(
            resolved
                .iter()
                .map(|resolved| match resolved.match_made {
                    StoreMatch::MatchFull => QueryStatus::ExistFullMatch,
                    StoreMatch::MatchPartition => QueryStatus::ExistPartitionMatch,
                    // A hash the caller's partition does not hold is another tenant's content.
                    // It collapses into absence rather than being reported one level down.
                    StoreMatch::MatchNone | StoreMatch::MatchHash => QueryStatus::NotFound,
                } as u8)
                .collect::<Vec<_>>(),
        ),
    }))
}

#[async_trait]
impl Message for Query {
    #[tracing::instrument(name = "Query::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        handle_query(&self.address, repository, immutable_store).await
    }
}

#[derive(Debug, PartialEq)]
pub struct QueryResponse {
    pub results: Bytes,
}

impl Response for QueryResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![self.results.clone()]
    }
}

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::Hash;
    use rand::Rng;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::store::test_store_create;
    use crate::util::address_with_random_context;

    #[tokio::test]
    async fn test_not_found() {
        let hash = Hash::hash_buffer(b"some fragment hash");
        let context = random::<Context>();

        let repository = random::<RepositoryId>();

        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(repository);

        let (immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                assert_eq!(
                    LoreResponse::Query(QueryResponse {
                        results: Bytes::copy_from_slice(&[QueryStatus::NotFound as u8])
                    }),
                    Query {
                        address: Bytes::copy_from_slice(Address { hash, context }.as_bytes()),
                    }
                    .handle(context_map, immutable_store)
                    .await
                    .unwrap()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_found() {
        let repository = random::<RepositoryId>();

        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(repository);

        let (immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let payload = Bytes::copy_from_slice(&random::<[u8; 32]>());
                let hash = Hash::hash_buffer(payload.as_ref());
                let context = random::<Context>();

                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };

                let address = Address { hash, context };

                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Failed to write fragment");

                assert_eq!(
                    LoreResponse::Query(QueryResponse {
                        results: Bytes::copy_from_slice(&[QueryStatus::ExistPartitionMatch as u8])
                    }),
                    Query {
                        address: Bytes::copy_from_slice(
                            address_with_random_context(address).as_bytes()
                        )
                    }
                    .handle(context_map, immutable_store)
                    .await
                    .unwrap()
                );
            })
            .await;
    }

    /// The collapse rule, which is the reason this mapping exists in one place.
    ///
    /// Content stored by another partition resolves to a hash match in a store that does not
    /// isolate, and reporting that back would tell this caller a hash it has no claim to exists
    /// somewhere. It has to arrive as absence, indistinguishable from content nobody stored.
    #[tokio::test]
    async fn a_hash_held_only_by_another_partition_reports_absence() {
        let stored_under = random::<RepositoryId>();
        let asked_under = random::<RepositoryId>();

        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(asked_under);

        let (immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let payload = Bytes::copy_from_slice(&random::<[u8; 32]>());
                let hash = Hash::hash_buffer(payload.as_ref());
                let address = Address {
                    hash,
                    context: random::<Context>(),
                };

                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };

                immutable_store
                    .clone()
                    .put(stored_under, address, fragment, Some(payload), false)
                    .await
                    .expect("Failed to write fragment");

                // Without this the assertion below would pass on a store that simply found
                // nothing, and would stop testing the collapse the day resolution changed.
                let resolved = lore_storage::immutable_store::query_one(
                    &immutable_store,
                    asked_under,
                    address,
                )
                .await
                .expect("resolve should work");
                assert_eq!(resolved.match_made, StoreMatch::MatchHash);

                assert_eq!(
                    LoreResponse::Query(QueryResponse {
                        results: Bytes::copy_from_slice(&[QueryStatus::NotFound as u8])
                    }),
                    Query {
                        address: Bytes::copy_from_slice(address.as_bytes())
                    }
                    .handle(context_map, immutable_store)
                    .await
                    .unwrap()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_found_in_context() {
        let repository = random::<RepositoryId>();

        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(repository);

        let (immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let payload = Bytes::copy_from_slice(&random::<[u8; 32]>());
                let hash = Hash::hash_buffer(payload.as_ref());
                let context = random::<Context>();

                let fragment = Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };

                let address = Address { hash, context };

                immutable_store
                    .clone()
                    .put(repository, address, fragment, Some(payload), false)
                    .await
                    .expect("Failed to write fragment");

                assert_eq!(
                    LoreResponse::Query(QueryResponse {
                        results: Bytes::copy_from_slice(&[QueryStatus::ExistFullMatch as u8])
                    }),
                    Query {
                        address: Bytes::copy_from_slice(address.as_bytes())
                    }
                    .handle(context_map, immutable_store)
                    .await
                    .unwrap()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_query_fragment_bulk() {
        let repository = random::<RepositoryId>();

        let context = random::<Context>();

        let (immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let count = 10;

                // we use an IndexMap which lets you iterate values in insertion order, which will allow us
                // to later verify the results in the response are in the same order as the fragments in the
                // request.
                let mut results = indexmap::IndexMap::new();
                for _ in 0..count {
                    let payload = Bytes::copy_from_slice(&random::<[u8; 32]>());
                    let hash = Hash::hash_buffer(payload.as_ref());
                    let fragment = Fragment {
                        flags: 0,
                        size_payload: payload.len() as u32,
                        size_content: payload.len() as u64,
                    };

                    let address = Address { hash, context };

                    // QueryStatus does not exist for value 2 (which would be exist, but in another repository)
                    // since client-server protocol cannot leak the existence in another repo. Avoid the value
                    let result: u8 = loop {
                        let result = rand::rng().random_range(
                            QueryStatus::ExistFullMatch as u8..=QueryStatus::NotFound as u8,
                        );
                        if result != 2 {
                            break result;
                        }
                    };
                    results.insert(address, result);

                    let state: QueryStatus = result.into();
                    match state {
                        QueryStatus::ExistPartitionMatch => immutable_store
                            .clone()
                            .put(
                                repository,
                                address_with_random_context(address),
                                fragment,
                                Some(payload),
                                false,
                            )
                            .await
                            .expect("Failed to store item"),
                        QueryStatus::ExistFullMatch => immutable_store
                            .clone()
                            .put(repository, address, fragment, Some(payload), false)
                            .await
                            .expect("Failed to store item"),
                        QueryStatus::NotFound => {}
                    }
                }

                let context_map = Arc::new(AttributeMap::default());
                context_map.insert(repository);

                let addresses: Vec<Address> = results.keys().cloned().collect();
                let message = Query {
                    address: Bytes::copy_from_slice(addresses.as_bytes()),
                };

                let results_clone = results.clone();

                assert_eq!(
                    LoreResponse::Query(QueryResponse {
                        results: results_clone.values().cloned().collect()
                    }),
                    message.handle(context_map, immutable_store).await.unwrap()
                );
            })
            .await;
    }
}
