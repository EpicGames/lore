// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::verify_authorization;
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
pub struct Copy {
    pub source_repository: RepositoryId,
    pub source_address: Address,
    /// Destination context. The destination address is `(target_partition,
    /// source_address.hash, target_context)` — same hash, possibly different context. Allows
    /// in-partition payload duplication when only the dedup tag changes.
    pub target_context: Context,
}

impl Copy {
    /// Legacy urc/0.2 wire — 64 bytes, no `target_context` on the wire. The destination's context
    /// is implicitly the source's, preserving the behavior the protocol shipped with.
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() != 64 {
            return Err(MessageParseError::InvalidFieldLength);
        }
        let mut bytes = bytes;
        let source_repository = RepositoryId::from(&bytes.split_to(size_of::<RepositoryId>())[..]);
        let hash = Hash::from(bytes.split_to(size_of::<Hash>()));
        let context = Context::from(bytes.split_to(size_of::<Context>()));
        let source_address = Address { hash, context };
        Ok(Self {
            source_repository,
            source_address,
            target_context: context,
        })
    }

    /// lore-storage/0.4 wire — 80 bytes, with `target_context` on the tail. Lets the destination
    /// take a different context from the source, including the same-partition different-context
    /// case used for in-partition payload deduplication.
    pub fn parse_v4(bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() != 80 {
            return Err(MessageParseError::InvalidFieldLength);
        }
        let mut bytes = bytes;
        let source_repository = RepositoryId::from(&bytes.split_to(size_of::<RepositoryId>())[..]);
        let hash = Hash::from(bytes.split_to(size_of::<Hash>()));
        let context = Context::from(bytes.split_to(size_of::<Context>()));
        let target_context = Context::from(bytes.split_to(size_of::<Context>()));
        let source_address = Address { hash, context };
        Ok(Self {
            source_repository,
            source_address,
            target_context,
        })
    }
}

/// Source-repo authorization check for v4 sessions.
/// When `session_map` is provided (v4 path), checks that the source repository has been
/// authorized (had at least one session started) on this connection.
/// When `None` (urc/0.2 path), uses the legacy `AuthorizationToken` check.
///
/// `destination_context` selects the destination tuple's dedup tag — destination address is
/// `(destination_repository, source_address.hash, destination_context)`. Legacy urc/0.2 callers
/// pass the source's context so behavior is unchanged; lore-storage/0.4 callers can pass a
/// different context to perform in-partition or cross-partition duplication without payload
/// transfer.
#[allow(clippy::too_many_arguments)]
pub async fn handle_copy(
    source_repository: RepositoryId,
    source_address: Address,
    destination_repository: RepositoryId,
    destination_context: Context,
    correlation_id: String,
    user_id: String,
    session_map: Option<&crate::protocol::storage::session::SessionMap>,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    if let Some(session_map) = session_map
        && !session_map.is_repository_authorized(source_repository)
    {
        return Err(MessageHandleError::AuthorizationFailure(
            "source repository not authorized".to_string(),
        ));
    }

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    LORE_CONTEXT
        .scope(execution, async move {
            match immutable_store
                .copy(
                    source_repository,
                    source_address,
                    destination_repository,
                    destination_context,
                    true,
                )
                .await
            {
                Ok(()) => Ok(LoreResponse::Copy(CopyResponse::default())),
                Err(err) if err.is_address_not_found() => Err(MessageHandleError::FragmentNotFound),
                Err(err) => {
                    warn!(error = ?err, "Failed to copy fragment");
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

#[async_trait]
impl Message for Copy {
    #[tracing::instrument(name = "Copy::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let destination_repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;

        if let Some(token) = context.get::<AuthorizationToken>() {
            verify_authorization(&token, self.source_repository)
                .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;
        }

        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_copy(
            self.source_repository,
            self.source_address,
            destination_repository,
            // urc/0.2 has no target_context on the wire; `Copy::parse` filled this with the
            // source's context so the destination tuple is exactly the source tuple under the
            // destination repository — matches the protocol's shipped behavior.
            self.target_context,
            correlation_id.to_string(),
            user_id,
            None, // urc/0.2 path: no SessionMap, auth check done above via AuthorizationToken
            immutable_store,
        )
        .await
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct CopyResponse {}

impl Response for CopyResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;

    use lore_base::error::AddressNotFound;
    use lore_base::types::Fragment;
    use lore_base::types::Partition;
    use lore_storage::ImmutableStore;
    use lore_storage::StoreError;
    use lore_storage::StoreMatch;
    use lore_storage::StoreObliterateStats;
    use lore_storage::StoreQueryResult;
    use rand::distr::Alphanumeric;
    use rand::distr::SampleString as _;
    use rand::random;

    use super::*;
    use crate::auth::jwt::ResourcePermission;
    use crate::store::test_store_create;

    /// A mock `ImmutableStore` whose `copy` method returns an `AddressNotFound` error.
    struct MockCopyFailStore;

    #[async_trait]
    impl ImmutableStore for MockCopyFailStore {
        async fn exist(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreMatch, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn exist_batch(
            self: Arc<Self>,
            _partition: Partition,
            _addresses: &[Address],
            _match_requested: StoreMatch,
        ) -> Result<Vec<StoreMatch>, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn query(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreQueryResult, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn get(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_required: StoreMatch,
        ) -> Result<(Fragment, Bytes), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn put(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _fragment: Fragment,
            _payload: Option<Bytes>,
            _force: bool,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn obliterate(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _stats: Arc<StoreObliterateStats>,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn evict(
            self: Arc<Self>,
            _max_capacity: usize,
            _sync_data: bool,
            _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<usize, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn compact(
            self: Arc<Self>,
            _max_size: usize,
            _at: Option<usize>,
            _sync_data: bool,
            _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            None
        }

        async fn compact_stop(self: Arc<Self>) {}

        fn max_query_batch(&self) -> Option<usize> {
            None
        }

        async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
            Ok(())
        }

        async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn copy(
            self: Arc<Self>,
            _source_partition: Partition,
            _source_address: Address,
            _destination_partition: Partition,
            _destination_context: Context,
            _durable: bool,
        ) -> Result<(), StoreError> {
            Err(StoreError::from(AddressNotFound::from(_source_address)))
        }

        fn as_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    /// A mock `ImmutableStore` whose `copy` method succeeds.
    struct MockCopySuccessStore;

    #[async_trait]
    impl ImmutableStore for MockCopySuccessStore {
        async fn exist(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreMatch, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn exist_batch(
            self: Arc<Self>,
            _partition: Partition,
            _addresses: &[Address],
            _match_requested: StoreMatch,
        ) -> Result<Vec<StoreMatch>, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn query(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_requested: StoreMatch,
        ) -> Result<StoreQueryResult, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn get(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _match_required: StoreMatch,
        ) -> Result<(Fragment, Bytes), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn put(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _fragment: Fragment,
            _payload: Option<Bytes>,
            _force: bool,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn obliterate(
            self: Arc<Self>,
            _partition: Partition,
            _address: Address,
            _stats: Arc<StoreObliterateStats>,
        ) -> Result<(), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn evict(
            self: Arc<Self>,
            _max_capacity: usize,
            _sync_data: bool,
            _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<usize, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn compact(
            self: Arc<Self>,
            _max_size: usize,
            _at: Option<usize>,
            _sync_data: bool,
            _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            None
        }

        async fn compact_stop(self: Arc<Self>) {}

        fn max_query_batch(&self) -> Option<usize> {
            None
        }

        async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
            Ok(())
        }

        async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
            Err(StoreError::internal("Not supported"))
        }

        async fn copy(
            self: Arc<Self>,
            _source_partition: Partition,
            _source_address: Address,
            _destination_partition: Partition,
            _destination_context: Context,
            _durable: bool,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        fn as_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    fn make_copy_message() -> Copy {
        let source_context = random::<Context>();
        Copy {
            source_repository: random::<RepositoryId>(),
            source_address: Address {
                hash: random::<Hash>(),
                context: source_context,
            },
            // Legacy parser fills this with `source_address.context`; mirror that here so the
            // round-trip test (`test_parse_valid`) keeps comparing equal under the legacy parser.
            target_context: source_context,
        }
    }

    fn make_copy_bytes(source_partition: Partition, source_address: Address) -> Bytes {
        use zerocopy::IntoBytes;

        let mut buf = bytes::BytesMut::with_capacity(64);
        buf.extend_from_slice(source_partition.as_bytes());
        buf.extend_from_slice(source_address.hash.as_bytes());
        buf.extend_from_slice(source_address.context.as_bytes());
        buf.freeze()
    }

    #[test]
    fn test_parse_valid() {
        let message = make_copy_message();
        let bytes = make_copy_bytes(message.source_repository, message.source_address);
        assert_eq!(Copy::parse(bytes), Ok(message));
    }

    #[test]
    fn test_parse_too_short() {
        assert_eq!(
            Copy::parse(Bytes::from(vec![0u8; 32])),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    #[test]
    fn test_parse_too_long() {
        assert_eq!(
            Copy::parse(Bytes::from(vec![0u8; 65])),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    #[tokio::test]
    async fn test_handle_not_connected() {
        let message = make_copy_message();
        let context_map = Arc::new(AttributeMap::default());

        let (immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                match message.handle(context_map, immutable_store).await {
                    Err(MessageHandleError::NotConnected) => (),
                    Err(e) => panic!("Expected NotConnected error, got {e:?}"),
                    Ok(_) => panic!("Expected NotConnected error, got Ok"),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_authorization_failure() {
        let message = make_copy_message();

        let destination_repository = random::<RepositoryId>();
        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(destination_repository);

        // Token that only permits a fixed, different repository — not the source_repository
        let token = AuthorizationToken {
            resources: Some(vec![ResourcePermission {
                resource_id: "urc-00000000000000000000000000000000".to_string(),
                permission: vec![],
            }]),
            ..Default::default()
        };
        context_map.insert(token);

        let (immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                match message.handle(context_map, immutable_store).await {
                    Err(MessageHandleError::AuthorizationFailure(_)) => (),
                    Err(e) => panic!("Expected AuthorizationFailure error, got {e:?}"),
                    Ok(_) => panic!("Expected AuthorizationFailure error, got Ok"),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_fragment_not_found() {
        let message = make_copy_message();

        let destination_repository = random::<RepositoryId>();
        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(destination_repository);

        let store: Arc<dyn ImmutableStore> = Arc::new(MockCopyFailStore);

        let (_unused_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                match message.handle(context_map, store).await {
                    Err(MessageHandleError::FragmentNotFound) => (),
                    Err(e) => panic!("Expected FragmentNotFound error, got {e:?}"),
                    Ok(_) => panic!("Expected FragmentNotFound error, got Ok"),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_handle_success() {
        let message = make_copy_message();

        let destination_repository = random::<RepositoryId>();
        let context_map = Arc::new(AttributeMap::default());
        context_map.insert(destination_repository);

        let store: Arc<dyn ImmutableStore> = Arc::new(MockCopySuccessStore);

        let (_unused_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                match message.handle(context_map, store).await {
                    Ok(LoreResponse::Copy(resp)) => {
                        assert_eq!(resp, CopyResponse::default());
                    }
                    Ok(other) => panic!("Expected Copy response, got {other:?}"),
                    Err(e) => panic!("Expected success, got error: {e:?}"),
                }
            })
            .await;
    }

    fn generate_tempdir() -> std::path::PathBuf {
        let testname = format!(
            "lore-copy-test-{}",
            Alphanumeric.sample_string(&mut rand::rng(), 8).as_str()
        );
        let mut dir = std::env::temp_dir();
        dir.push(testname);
        std::fs::create_dir_all(&dir).expect("Create test directory");
        std::fs::canonicalize(dir).expect("Canonicalize temporary test dir")
    }

    fn setup_test_execution() -> Arc<lore_revision::interface::ExecutionContext> {
        Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ))
    }

    /// R1 / R2 / R3 / R6 — full put → copy-via-handler → get pipeline.
    ///
    /// Puts a fragment into repo A, copies it to repo B through the `Copy`
    /// message handler, then asserts:
    ///  * The handler returns `Ok(LoreResponse::Copy(_))` (not Put — R3).
    ///  * The fragment is retrievable from repo B (R1/R2).
    ///  * The original fragment is still accessible in repo A (R2).
    ///  * A second call through the handler also returns `Ok(LoreResponse::Copy(_))` (R6).
    #[tokio::test]
    async fn test_copy_full_pipeline_success() {
        use lore_revision::fragment::generate_random;
        use lore_storage::ImmutableStore as ImmutableStoreTrait;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;

        let dir = generate_tempdir();
        let dir_cleanup = dir.clone();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution, async move {
                let store = lore_storage::LocalImmutableStore::new(
                    Some(dir),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repo_a: RepositoryId = random();
                let repo_b: RepositoryId = random();

                let (fragment, address, payload) = generate_random();

                // Put into repo A
                store
                    .clone()
                    .put(repo_a, address, fragment, Some(payload.clone()), false)
                    .await
                    .expect("Failed to put fragment into repo A");

                store.clone().flush(true).await.expect("Failed to flush");

                // Build a context map connected to repo B (the destination)
                let context_map = Arc::new(AttributeMap::default());
                context_map.insert(repo_b);

                let copy_message = Copy {
                    source_repository: repo_a,
                    source_address: address,
                    target_context: address.context,
                };

                // First copy — must return LoreResponse::Copy (R1/R2/R3)
                let store_dyn: Arc<dyn ImmutableStore> = store.clone();
                match copy_message.handle(context_map.clone(), store_dyn).await {
                    Ok(LoreResponse::Copy(resp)) => {
                        assert_eq!(
                            resp,
                            CopyResponse::default(),
                            "Unexpected CopyResponse value"
                        );
                    }
                    Ok(other) => panic!("Expected LoreResponse::Copy, got {other:?}"),
                    Err(e) => panic!("Expected success on first copy, got error: {e:?}"),
                }

                // Fragment must now be retrievable from repo B (R1/R2)
                store
                    .clone()
                    .get(repo_b, address, lore_storage::StoreMatch::MatchFull)
                    .await
                    .expect("Fragment should be accessible in repo B after copy");

                // Original fragment must still exist in repo A (R2)
                store
                    .clone()
                    .get(repo_a, address, lore_storage::StoreMatch::MatchFull)
                    .await
                    .expect("Fragment should still be accessible in repo A after copy");

                // Second copy call — idempotency (R6)
                let store_dyn: Arc<dyn ImmutableStore> = store.clone();
                match copy_message.handle(context_map, store_dyn).await {
                    Ok(LoreResponse::Copy(resp)) => {
                        assert_eq!(
                            resp,
                            CopyResponse::default(),
                            "Unexpected CopyResponse value on idempotent call"
                        );
                    }
                    Ok(other) => {
                        panic!("Expected LoreResponse::Copy on second call, got {other:?}")
                    }
                    Err(e) => {
                        panic!("Expected success on second (idempotent) copy, got error: {e:?}")
                    }
                }
            })
            .await;

        let _ = std::fs::remove_dir_all(&dir_cleanup);
    }

    /// R6 — two sequential Copy handler calls for the same source→dest pair both succeed.
    ///
    /// This is a focused idempotency test: after a successful copy, a second
    /// identical copy must also return `Ok(LoreResponse::Copy(_))`.
    #[tokio::test]
    async fn test_copy_idempotent_via_handler() {
        use lore_revision::fragment::generate_random;
        use lore_storage::ImmutableStore as ImmutableStoreTrait;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;

        let dir = generate_tempdir();
        let dir_cleanup = dir.clone();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution, async move {
                let store = lore_storage::LocalImmutableStore::new(
                    Some(dir),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repo_a: RepositoryId = random();
                let repo_b: RepositoryId = random();

                let (fragment, address, payload) = generate_random();

                store
                    .clone()
                    .put(repo_a, address, fragment, Some(payload), false)
                    .await
                    .expect("Failed to put fragment into repo A");

                store.clone().flush(true).await.expect("Failed to flush");

                let context_map = Arc::new(AttributeMap::default());
                context_map.insert(repo_b);

                let copy_message = Copy {
                    source_repository: repo_a,
                    source_address: address,
                    target_context: address.context,
                };

                for call_number in 1..=2 {
                    let store_dyn: Arc<dyn ImmutableStore> = store.clone();
                    match copy_message.handle(context_map.clone(), store_dyn).await {
                        Ok(LoreResponse::Copy(_)) => {}
                        Ok(other) => {
                            panic!("Call {call_number}: expected LoreResponse::Copy, got {other:?}")
                        }
                        Err(e) => panic!("Call {call_number}: expected success, got error: {e:?}"),
                    }
                }
            })
            .await;

        let _ = std::fs::remove_dir_all(&dir_cleanup);
    }

    /// R4 — Copy handler returns `FragmentNotFound` when the source fragment
    /// does not exist in the store.
    ///
    /// This tests that the handler maps `StoreError::NotFound` to
    /// `MessageHandleError::FragmentNotFound` correctly using a real store
    /// (not a mock), confirming the pipeline end-to-end.
    #[tokio::test]
    async fn test_copy_pipeline_not_found() {
        use lore_storage::local::immutable_store::ImmutableStoreSettings;

        let dir = generate_tempdir();
        let dir_cleanup = dir.clone();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution, async move {
                let store = lore_storage::LocalImmutableStore::new(
                    Some(dir),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repo_a: RepositoryId = random();
                let repo_b: RepositoryId = random();

                // Address that was never stored
                let nonexistent_address = Address {
                    hash: Hash::from([0xABu8; 32]),
                    context: random(),
                };

                let context_map = Arc::new(AttributeMap::default());
                context_map.insert(repo_b);

                let copy_message = Copy {
                    source_repository: repo_a,
                    source_address: nonexistent_address,
                    target_context: nonexistent_address.context,
                };

                let store_dyn: Arc<dyn ImmutableStore> = store;
                match copy_message.handle(context_map, store_dyn).await {
                    Err(MessageHandleError::FragmentNotFound) => {}
                    Err(e) => panic!("Expected FragmentNotFound, got {e:?}"),
                    Ok(_) => panic!("Expected FragmentNotFound error, got Ok"),
                }
            })
            .await;

        let _ = std::fs::remove_dir_all(&dir_cleanup);
    }

    /// R4 / R5 — Independent Copy handler calls fail or succeed independently;
    /// a failure on one item does not affect subsequent calls.
    ///
    /// Simulates the gRPC streaming continuity requirement (R4/R5) at the
    /// handler level: three sequential `handle()` calls where the middle one
    /// targets a fragment that does not exist.  Asserts `success_1`, `NOT_FOUND_2`,
    /// `success_3`, matching the expected stream behaviour.
    #[tokio::test]
    async fn test_copy_stream_continuity_not_found_in_middle() {
        use lore_revision::fragment::generate_random;
        use lore_storage::ImmutableStore as ImmutableStoreTrait;
        use lore_storage::local::immutable_store::ImmutableStoreSettings;

        let dir = generate_tempdir();
        let dir_cleanup = dir.clone();
        let execution = setup_test_execution();

        LORE_CONTEXT
            .scope(execution, async move {
                let store = lore_storage::LocalImmutableStore::new(
                    Some(dir),
                    ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let repo_a: RepositoryId = random();
                let repo_b: RepositoryId = random();

                // Prepare two real fragments (items 1 and 3)
                let (fragment1, address1, payload1) = generate_random();
                let (fragment2, address2, payload2) = generate_random();

                store
                    .clone()
                    .put(repo_a, address1, fragment1, Some(payload1), false)
                    .await
                    .expect("Failed to put fragment 1");
                store
                    .clone()
                    .put(repo_a, address2, fragment2, Some(payload2), false)
                    .await
                    .expect("Failed to put fragment 2");

                store.clone().flush(true).await.expect("Failed to flush");

                let context_map = Arc::new(AttributeMap::default());
                context_map.insert(repo_b);

                // Item 1: valid → expect success
                let msg1 = Copy {
                    source_repository: repo_a,
                    source_address: address1,
                    target_context: address1.context,
                };
                let store_dyn: Arc<dyn ImmutableStore> = store.clone();
                match msg1.handle(context_map.clone(), store_dyn).await {
                    Ok(LoreResponse::Copy(_)) => {}
                    other => panic!("Item 1: expected success, got {other:?}"),
                }

                // Item 2: non-existent fragment → expect FragmentNotFound
                let missing_address = Address {
                    hash: Hash::from([0xFFu8; 32]),
                    context: random(),
                };
                let msg2 = Copy {
                    source_repository: repo_a,
                    source_address: missing_address,
                    target_context: missing_address.context,
                };
                let store_dyn: Arc<dyn ImmutableStore> = store.clone();
                match msg2.handle(context_map.clone(), store_dyn).await {
                    Err(MessageHandleError::FragmentNotFound) => {}
                    other => panic!("Item 2: expected FragmentNotFound, got {other:?}"),
                }

                // Item 3: valid → expect success (stream continuity)
                let msg3 = Copy {
                    source_repository: repo_a,
                    source_address: address2,
                    target_context: address2.context,
                };
                let store_dyn: Arc<dyn ImmutableStore> = store.clone();
                match msg3.handle(context_map.clone(), store_dyn).await {
                    Ok(LoreResponse::Copy(_)) => {}
                    other => panic!("Item 3: expected success after previous error, got {other:?}"),
                }
            })
            .await;

        let _ = std::fs::remove_dir_all(&dir_cleanup);
    }
}
