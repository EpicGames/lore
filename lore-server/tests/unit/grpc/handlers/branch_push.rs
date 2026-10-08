// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::State;
use lore_revision::util::request_tracker::StoreRequestTracker;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push::*;
use lore_server::grpc::server::RevisionListAcceleration;
use rand::random;
use tonic::Code;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::server::TcpConnectInfo;

use crate::store::test_support::test_store_create;

async fn create_test_branch(repository: &Arc<RepositoryContext>) -> BranchId {
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    let write_token = get_write_token();
    branch::create(
        repository.clone(),
        &write_token,
        branch_id,
        "test-branch",
        branch::default_category(),
        "creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("create branch");
    branch_id
}

async fn serialize_revision(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent_self: Hash,
    parent_other: Hash,
    revision_number: u64,
) -> Arc<State> {
    let write_token = get_write_token();
    let mut metadata = lore_revision::metadata::Metadata::new();
    metadata.set_branch(branch).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = State::new();
    state.set_parent_self(parent_self);
    if !parent_other.is_zero() {
        state.set_parent_other(parent_other);
    }
    state.set_revision_number(revision_number);
    state.set_metadata_hash(metadata_hash);
    state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");
    state
}

/// A revision whose metadata names `payload`, so the walk collects an address
/// it never reads.
async fn serialize_revision_naming_a_payload(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    payload: Address,
) -> Arc<State> {
    let write_token = get_write_token();
    let mut metadata = lore_revision::metadata::Metadata::new();
    metadata.set_branch(branch).expect("set branch");
    metadata
        .set_address("build-artifact", payload)
        .expect("set the payload address");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = State::new();
    state.set_parent_self(Hash::default());
    state.set_revision_number(1);
    state.set_metadata_hash(metadata_hash);
    state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");
    state
}

/// A revision holding one file, so its state references node and name
/// fragments the walk has to read.
async fn serialize_revision_with_a_file(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
) -> Arc<State> {
    let write_token = get_write_token();
    let mut metadata = lore_revision::metadata::Metadata::new();
    metadata.set_branch(branch).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = State::new();
    state.set_parent_self(Hash::default());
    state.set_revision_number(1);
    state.set_metadata_hash(metadata_hash);
    state
        .node_add(
            repository.clone(),
            ROOT_NODE,
            Node {
                flags: NodeFlags::File.bits(),
                name_hash: lore_storage::hash::hash_string("file.txt"),
                ..Default::default()
            },
            "file.txt",
        )
        .await
        .expect("node_add");
    state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");
    state
}

/// A merge revision naming both parents and holding one file, so a walk against
/// either parent reaches the same node and name fragments.
async fn serialize_merge_with_a_file(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent_self: Hash,
    parent_other: Hash,
) -> Arc<State> {
    let write_token = get_write_token();
    let mut metadata = lore_revision::metadata::Metadata::new();
    metadata.set_branch(branch).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = State::new();
    state.set_parent_self(parent_self);
    state.set_parent_other(parent_other);
    state.set_revision_number(3);
    state.set_metadata_hash(metadata_hash);
    state
        .node_add(
            repository.clone(),
            ROOT_NODE,
            Node {
                flags: NodeFlags::File.bits(),
                name_hash: lore_storage::hash::hash_string("merged.txt"),
                ..Default::default()
            },
            "merged.txt",
        )
        .await
        .expect("node_add");
    state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");
    state
}

/// Copy the fragment at `hash` alone, leaving everything it references absent
/// in `target`.
async fn hand_over_fragment(
    source: &Arc<dyn lore_storage::ImmutableStore>,
    target: &Arc<dyn lore_storage::ImmutableStore>,
    repository: RepositoryId,
    hash: Hash,
) {
    let address = Address::zero_context_hash(hash);
    let data = source
        .clone()
        .get(repository, address)
        .await
        .expect("read the fragment to hand over");
    target
        .clone()
        .put(repository, address, data.fragment, data.payload, false)
        .await
        .expect("hand over the fragment");
}

/// Push revisions `numbers`, chained from `parent`. Returns the pushed
/// signatures oldest-first.
async fn push_linear_revisions(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent: Hash,
    numbers: std::ops::RangeInclusive<u64>,
) -> Vec<Hash> {
    let mut parent = parent;
    let mut signatures = Vec::new();
    for number in numbers {
        let state = serialize_revision(repository, branch, parent, Hash::default(), number).await;
        parent = push(
            repository.clone(),
            branch,
            state.revision(),
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
        )
        .await
        .expect("push revision")
        .revision;
        signatures.push(parent);
    }
    signatures
}

/// Push a merge revision whose `parent_other` carries a much higher
/// revision number, so the branch's revision number jumps to
/// `other_revision_number + 1` and skips the numbers in between.
async fn push_jump_revision(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent: Hash,
    other_revision_number: u64,
) -> (Hash, u64) {
    let other = serialize_revision(
        repository,
        branch,
        Hash::default(),
        Hash::default(),
        other_revision_number,
    )
    .await;
    let state = serialize_revision(
        repository,
        branch,
        parent,
        other.revision(),
        0, /* rewritten */
    )
    .await;

    let result = push(
        repository.clone(),
        branch,
        state.revision(),
        true,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        RevisionListAcceleration::default(),
    )
    .await
    .expect("push jump revision");
    (result.revision, result.revision_number)
}

/// Read the revision sealed at `boundary`, or `None` when unsealed.
async fn load_step_key(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    boundary: u64,
) -> Option<Hash> {
    let (key, key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        boundary,
        DEFAULT_HISTORY_STEP_SIZE,
    );
    repository
        .clone()
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .ok()
        .filter(|revision| !revision.is_zero())
}

mod extract_client_ip {
    use super::*;

    #[test]
    fn use_x_forwarded_when_available() {
        let mut req = Request::new(());

        let xff_metadata_value: MetadataValue<_> = "10.0.0.1, 10.0.0.2".parse().unwrap();
        req.metadata_mut()
            .insert("x-forwarded-for", xff_metadata_value);

        // set remote address to make sure it's NOT used in presence of the XFF header
        let peer_addr = SocketAddr::from(([192, 168, 1, 42], 4242));
        req.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(peer_addr),
        });

        assert_eq!(
            extract_client_ip(&req),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)))
        );
    }

    #[test]
    fn dont_use_xff_when_it_contains_invalid_value() {
        let mut req = Request::new(());

        let xff_metadata_value: MetadataValue<_> = "10.0.0.lol, 10.0.0.wat".parse().unwrap();
        req.metadata_mut()
            .insert("x-forwarded-for", xff_metadata_value);

        let peer_addr = SocketAddr::from(([192, 168, 1, 42], 4242));
        req.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(peer_addr),
        });

        assert_eq!(
            extract_client_ip(&req),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)))
        );
    }

    #[test]
    fn still_uses_last_ip_when_xff_contains_invalid_value_in_chain() {
        let mut req = Request::new(());

        let xff_metadata_value: MetadataValue<_> =
            "10.0.0.lol, 10.0.0.wat, 10.0.0.42".parse().unwrap();
        req.metadata_mut()
            .insert("x-forwarded-for", xff_metadata_value);

        let peer_addr = SocketAddr::from(([192, 168, 1, 42], 4242));
        req.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(peer_addr),
        });

        assert_eq!(
            extract_client_ip(&req),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 42)))
        );
    }

    #[test]
    fn fallback_to_remote_addr() {
        let mut req = Request::new(());

        let peer_addr = SocketAddr::from(([192, 168, 1, 42], 31415));
        req.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(peer_addr),
        });

        assert_eq!(
            extract_client_ip(&req),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)))
        );
    }
}

mod push {
    use super::*;

    #[tokio::test]
    async fn push_unknown_revision_returns_not_found() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                repository_id,
            ));

            let write_token = get_write_token();
            branch::create(
                repository_context.clone(),
                &write_token,
                branch_id,
                "test-branch",
                branch::personal_category(),
                "test-creator",
                1,
                vec![],
                false,
                false,
            )
            .await
            .expect("Failed to create branch");

            // A hash with no corresponding state data in the immutable store
            let nonexistent_revision = random::<Hash>();

            let result = push(
                repository_context,
                branch_id,
                nonexistent_revision,
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await;

            let Err(status) = result else {
                panic!("an unknown revision cannot be pushed");
            };
            assert_eq!(status.code(), Code::NotFound);
            let error = lore_transport::ProtocolError::from(status);
            assert!(error.is_not_found(), "{error:?}");
        }))
        .await;
    }

    /// A fragment the walk cannot read is named as an address the caller
    /// reconstructs. Both detections share a code, so the message is what
    /// pins which one this reaches.
    #[tokio::test]
    async fn a_fragment_the_walk_cannot_read_names_its_address() {
        let repository_id = random::<RepositoryId>();

        let (peer_store, peer_mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        let (store, mutable_store, _) = test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let peer = Arc::new(RepositoryContext::new_server_context(
                peer_store.clone(),
                peer_mutable,
                repository_id,
            ));
            let repository = Arc::new(RepositoryContext::new_server_context(
                store.clone(),
                mutable_store,
                repository_id,
            ));

            let branch = create_test_branch(&repository).await;
            let state = serialize_revision_with_a_file(&peer, branch).await;

            hand_over_fragment(&peer_store, &store, repository_id, state.revision()).await;

            let Err(status) = push(
                repository,
                branch,
                state.revision(),
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            else {
                panic!("a revision missing its fragments cannot be pushed");
            };

            assert_eq!(status.code(), Code::FailedPrecondition);
            assert!(
                status
                    .message()
                    .starts_with("Failed to collect new fragments"),
                "{}",
                status.message()
            );
            let error = lore_transport::ProtocolError::from(status);
            assert!(error.is_address_not_found(), "{error:?}");
        }))
        .await;
    }

    /// A fragment the store answers as absent is named the same way, so the
    /// two paths that detect it report one condition.
    ///
    /// The revision and the blob holding its metadata are both handed over,
    /// since the walk reads both. What stays absent is the payload that
    /// metadata names, which the walk collects without reading, so the store
    /// query is what detects it.
    #[tokio::test]
    async fn a_fragment_the_store_reports_absent_names_its_address() {
        let repository_id = random::<RepositoryId>();

        let (peer_store, peer_mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        let (store, mutable_store, _) = test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let peer = Arc::new(RepositoryContext::new_server_context(
                peer_store.clone(),
                peer_mutable,
                repository_id,
            ));
            let repository = Arc::new(RepositoryContext::new_server_context(
                store.clone(),
                mutable_store,
                repository_id,
            ));

            let branch = create_test_branch(&repository).await;
            let payload = Address::zero_context_hash(Hash::from([0xabu8; 32]));
            let state = serialize_revision_naming_a_payload(&peer, branch, payload).await;
            hand_over_fragment(&peer_store, &store, repository_id, state.revision()).await;
            hand_over_fragment(&peer_store, &store, repository_id, state.metadata_hash()).await;

            let Err(status) = push(
                repository,
                branch,
                state.revision(),
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            else {
                panic!("a revision missing a payload its metadata names cannot be pushed");
            };

            assert_eq!(status.code(), Code::FailedPrecondition);
            assert_eq!(
                status.message(),
                format!("Missing fragment '{payload}'"),
                "the absent payload has to be the fragment named"
            );
            let error = lore_transport::ProtocolError::from(status);
            assert!(error.is_address_not_found(), "{error:?}");
        }))
        .await;
    }

    /// A merge whose second parent the store cannot answer for is refused as the
    /// missing fragment it is.
    ///
    /// The merge and the blob holding its metadata are handed over, so what stays
    /// absent is the line the merge joins. That parent carries what the merge is
    /// verified against, so it is read rather than only queried, and the read has to
    /// report the address as a missing fragment like the query does.
    #[tokio::test]
    async fn a_merge_missing_its_other_parent_names_that_address() {
        let repository_id = random::<RepositoryId>();

        let (peer_store, peer_mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        let (store, mutable_store, _) = test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let peer = Arc::new(RepositoryContext::new_server_context(
                peer_store.clone(),
                peer_mutable,
                repository_id,
            ));
            let repository = Arc::new(RepositoryContext::new_server_context(
                store.clone(),
                mutable_store,
                repository_id,
            ));

            let branch = create_test_branch(&repository).await;
            let other =
                serialize_revision(&peer, branch, Hash::default(), Hash::default(), 1).await;
            let merge =
                serialize_revision(&peer, branch, Hash::default(), other.revision(), 2).await;

            hand_over_fragment(&peer_store, &store, repository_id, merge.revision()).await;
            hand_over_fragment(&peer_store, &store, repository_id, merge.metadata_hash()).await;

            let Err(status) = push(
                repository,
                branch,
                merge.revision(),
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            else {
                panic!("a merge missing the line it joins cannot be pushed");
            };

            assert_eq!(status.code(), Code::FailedPrecondition);
            assert_eq!(
                status.message(),
                format!(
                    "Missing fragment '{}'",
                    Address::zero_context_hash(other.revision())
                ),
                "the absent parent has to be the fragment named"
            );
            let error = lore_transport::ProtocolError::from(status);
            assert!(error.is_address_not_found(), "{error:?}");
        }))
        .await;
    }

    /// A merge is verified against its second parent as well as its first, so a
    /// second parent the store cannot walk refuses the push.
    ///
    /// That parent and the blob holding its metadata are handed over, leaving the tree
    /// it names absent. The merge itself holds no file, so collecting it against its
    /// first parent reads nothing of that tree: what reaches it is the collection
    /// against the second parent, which is the one this covers.
    #[tokio::test]
    async fn a_merge_is_verified_against_the_tree_of_its_other_parent() {
        let repository_id = random::<RepositoryId>();

        let (peer_store, peer_mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        let (store, mutable_store, _) = test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let peer = Arc::new(RepositoryContext::new_server_context(
                peer_store.clone(),
                peer_mutable,
                repository_id,
            ));
            let repository = Arc::new(RepositoryContext::new_server_context(
                store.clone(),
                mutable_store,
                repository_id,
            ));

            let branch = create_test_branch(&repository).await;
            let other = serialize_revision_with_a_file(&peer, branch).await;
            let merge =
                serialize_revision(&peer, branch, Hash::default(), other.revision(), 2).await;

            for hash in [
                merge.revision(),
                merge.metadata_hash(),
                other.revision(),
                other.metadata_hash(),
            ] {
                hand_over_fragment(&peer_store, &store, repository_id, hash).await;
            }

            let Err(status) = push(
                repository,
                branch,
                merge.revision(),
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
            )
            .await
            else {
                panic!("a merge whose other parent cannot be walked cannot be pushed");
            };

            assert_eq!(status.code(), Code::FailedPrecondition);
            assert!(
                status
                    .message()
                    .starts_with("Failed to collect new fragments"),
                "{}",
                status.message()
            );
            let error = lore_transport::ProtocolError::from(status);
            assert!(error.is_address_not_found(), "{error:?}");
        }))
        .await;
    }

    #[tokio::test]
    async fn linear_history_seals_a_boundary_only_once_the_head_moves_past_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;

            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=100).await;

            // Revision 100 is the head, so segment 100 is still the open one.
            assert_eq!(load_step_key(&repository, branch, 100).await, None);

            push_linear_revisions(&repository, branch, chain[99], 101..=101).await;

            // Now the head has moved past 100, sealing it with revision 100.
            assert_eq!(
                load_step_key(&repository, branch, 100).await,
                Some(chain[99])
            );
            // Nothing above the head may be sealed.
            assert_eq!(load_step_key(&repository, branch, 200).await, None);
        }))
        .await;
    }

    #[tokio::test]
    async fn linear_history_seals_each_boundary_with_its_own_highest_revision() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;

            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=250).await;

            assert_eq!(
                load_step_key(&repository, branch, 100).await,
                Some(chain[99])
            );
            assert_eq!(
                load_step_key(&repository, branch, 200).await,
                Some(chain[199])
            );
            // Segment 300 holds the head at 250 and stays open.
            assert_eq!(load_step_key(&repository, branch, 300).await, None);
        }))
        .await;
    }

    /// A jump seals the boundaries between the two revisions and no
    /// others. The segment the new revision lands in stays open, since the
    /// revisions above it do not exist yet.
    #[tokio::test]
    async fn jump_seals_the_crossed_boundary_and_not_the_one_it_landed_in() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;

            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=99).await;
            let (_, revision_number) =
                push_jump_revision(&repository, branch, chain[98], 104).await;
            assert_eq!(revision_number, 105);

            // Boundary 100 is the only one crossed, answered by revision 99.
            assert_eq!(
                load_step_key(&repository, branch, 100).await,
                Some(chain[98])
            );
            // Segment 200 contains the new head at 105 and is still open.
            assert_eq!(load_step_key(&repository, branch, 200).await, None);
            assert_eq!(load_step_key(&repository, branch, 300).await, None);
        }))
        .await;
    }

    #[tokio::test]
    async fn jump_seals_every_boundary_it_skipped_over() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;

            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=150).await;
            assert_eq!(
                load_step_key(&repository, branch, 100).await,
                Some(chain[99])
            );

            let (_, revision_number) =
                push_jump_revision(&repository, branch, chain[149], 399).await;
            assert_eq!(revision_number, 400);

            // 150 -> 400 skips 200 and 300; both are answered by revision 150,
            // the highest revision numbered at or below them.
            assert_eq!(
                load_step_key(&repository, branch, 200).await,
                Some(chain[149])
            );
            assert_eq!(
                load_step_key(&repository, branch, 300).await,
                Some(chain[149])
            );
            // The boundary already sealed before the jump is left alone.
            assert_eq!(
                load_step_key(&repository, branch, 100).await,
                Some(chain[99])
            );
            // Segment 400 holds the new head, and 500 was never reached.
            assert_eq!(load_step_key(&repository, branch, 400).await, None);
            assert_eq!(load_step_key(&repository, branch, 500).await, None);
        }))
        .await;
    }
}

mod collect_new_addresses {
    use super::*;

    /// The walks of both parents join into one ascending set holding no address
    /// twice.
    ///
    /// Both walks reach the same state, so everything the merge adds is new against
    /// each parent and arrives at the join twice. The caller splits the join into
    /// batches from its end and queries the store with each, so an address held
    /// twice is a fragment queried twice.
    #[tokio::test]
    async fn a_fragment_both_parent_walks_answer_is_joined_once_in_order() {
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                repository_id,
            ));

            let branch = create_test_branch(&repository).await;
            let parent =
                serialize_revision(&repository, branch, Hash::default(), Hash::default(), 1).await;
            let other =
                serialize_revision(&repository, branch, Hash::default(), Hash::default(), 2).await;
            let merge = serialize_merge_with_a_file(
                &repository,
                branch,
                parent.revision(),
                other.revision(),
            )
            .await;

            // Both walks have to answer something in common, or the join has
            // nothing to collapse and what follows holds vacuously.
            let against_parent = state::collect_new_fragments(
                repository.clone(),
                parent.clone(),
                merge.clone(),
                true,
                Arc::new(StoreRequestTracker::default()),
            )
            .await
            .expect("collect against the first parent");
            let against_other = state::collect_new_fragments(
                repository.clone(),
                other.clone(),
                merge.clone(),
                true,
                Arc::new(StoreRequestTracker::default()),
            )
            .await
            .expect("collect against the second parent");
            let shared = against_parent
                .iter()
                .filter(|address| against_other.contains(address))
                .count();
            assert!(shared > 0, "the two walks have to overlap");

            let collected = collect_new_addresses(
                repository.clone(),
                parent,
                Some(other.clone()),
                merge.clone(),
                Arc::new(StoreRequestTracker::default()),
            )
            .await
            .expect("collect against both parents");

            assert!(
                collected.is_sorted(),
                "the join has to be ascending: {collected:?}"
            );
            let mut once = collected.clone();
            once.dedup();
            assert_eq!(
                collected, once,
                "an address both walks answer has to be held once"
            );
            assert!(
                collected.contains(&Address::zero_context_hash(other.revision())),
                "the second parent has to be named among the fragments"
            );
        }))
        .await;
    }

    /// A walk that fails is the answer, even where the walk beside it succeeded.
    ///
    /// Both walks are in flight at once, so the one against the second parent
    /// settles against a store that holds the parent but not the tree under it,
    /// while the one against the first parent has everything it reads. The failure
    /// has to reach the caller as the missing fragment it is rather than be dropped
    /// for the half that answered.
    #[tokio::test]
    async fn a_walk_that_cannot_read_its_parent_fails_the_whole_join() {
        let repository_id = random::<RepositoryId>();

        let (peer_store, peer_mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        let (store, mutable_store, _) = test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let peer = Arc::new(RepositoryContext::new_server_context(
                peer_store.clone(),
                peer_mutable,
                repository_id,
            ));
            let repository = Arc::new(RepositoryContext::new_server_context(
                store.clone(),
                mutable_store,
                repository_id,
            ));

            let branch = create_test_branch(&repository).await;

            // The second parent is written to the peer and only its own two
            // fragments handed over, leaving the tree it names unreadable here.
            let other = serialize_revision_with_a_file(&peer, branch).await;
            hand_over_fragment(&peer_store, &store, repository_id, other.revision()).await;
            hand_over_fragment(&peer_store, &store, repository_id, other.metadata_hash()).await;
            let other = State::deserialize(repository.clone(), other.revision())
                .await
                .expect("deserialize the handed-over parent");

            let parent =
                serialize_revision(&repository, branch, Hash::default(), Hash::default(), 1).await;
            let merge = serialize_merge_with_a_file(
                &repository,
                branch,
                parent.revision(),
                other.revision(),
            )
            .await;

            // The walk against the first parent reads only what is here, so its
            // success is what leaves the failure beside it the only one to report.
            state::collect_new_fragments(
                repository.clone(),
                parent.clone(),
                merge.clone(),
                true,
                Arc::new(StoreRequestTracker::default()),
            )
            .await
            .expect("the first parent has to be walkable");

            let Err(status) = collect_new_addresses(
                repository.clone(),
                parent,
                Some(other),
                merge,
                Arc::new(StoreRequestTracker::default()),
            )
            .await
            else {
                panic!("a parent whose tree cannot be read cannot be collected against");
            };

            assert_eq!(status.code(), Code::FailedPrecondition);
            assert!(
                status
                    .message()
                    .starts_with("Failed to collect new fragments"),
                "{}",
                status.message()
            );
            let error = lore_transport::ProtocolError::from(status);
            assert!(error.is_address_not_found(), "{error:?}");
        }))
        .await;
    }
}
