// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::commit::commit_in_memory_revision;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::Mark;
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
    let state = new_revision(
        repository,
        branch,
        parent_self,
        parent_other,
        revision_number,
    )
    .await;
    state
        .serialize(repository.clone(), &get_write_token())
        .await
        .expect("serialize state");
    state
}

/// An unserialized revision on `parent_self`, and on `parent_other` when it is not zero.
async fn new_revision(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent_self: Hash,
    parent_other: Hash,
    revision_number: u64,
) -> Arc<State> {
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
    serialize_revision_adding_a_file(
        repository,
        branch,
        Hash::default(),
        Hash::default(),
        1,
        "file.txt",
    )
    .await
}

/// A revision on `parent_self`, and on `parent_other` when it is not zero, that adds the file
/// `name`.
async fn serialize_revision_adding_a_file(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent_self: Hash,
    parent_other: Hash,
    revision_number: u64,
    name: &str,
) -> Arc<State> {
    let state = new_revision(
        repository,
        branch,
        parent_self,
        parent_other,
        revision_number,
    )
    .await;
    state
        .node_add(
            repository.clone(),
            ROOT_NODE,
            Node {
                flags: NodeFlags::File.bits(),
                name_hash: lore_storage::hash::hash_string(name),
                ..Default::default()
            },
            name,
        )
        .await
        .expect("node_add");
    state
        .serialize(repository.clone(), &get_write_token())
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
    serialize_revision_adding_a_file(
        repository,
        branch,
        parent_self,
        parent_other,
        3,
        "merged.txt",
    )
    .await
}

/// Stages `node` as an add of `name` below `parent`.
async fn add_staged(
    repository: &Arc<RepositoryContext>,
    state: &State,
    parent: NodeID,
    node: Node,
    name: &str,
) -> NodeID {
    let node_id = state
        .node_add(repository.clone(), parent, node, name)
        .await
        .expect("add the node");
    state
        .node_mark(
            repository,
            node_id,
            Mark::staged_and_dirty(NodeFlags::StagedAdd, NodeFlags::DirtyAdd),
        )
        .await
        .expect("stage the add");
    node_id
}

/// A change a revision [`commit_files`] commits makes.
#[derive(Clone, Copy)]
enum Change {
    /// Adds a file holding its path, at most one directory deep, with its directory. The file's
    /// identity derives from its path, so two revisions adding a file at one path add the same
    /// file.
    Add(&'static str),
    /// Deletes the file at the path.
    Delete(&'static str),
    /// Moves the file at the first path to the second, in a directory that exists.
    Move(&'static str, &'static str),
}

/// The node at `path` of `state`, which exists.
async fn node_at(repository: &Arc<RepositoryContext>, state: &State, path: &str) -> NodeID {
    state
        .find_node_link(repository.clone(), path)
        .await
        .expect("find the node")
        .node
}

/// Adds the file at `path` to `state`, as [`Change::Add`] describes.
async fn add_file(repository: &Arc<RepositoryContext>, state: &State, path: &str) {
    let mut parent = ROOT_NODE;
    let mut name = path;
    if let Some((directory, file)) = path.split_once('/') {
        name = file;
        parent = match state.find_node_link(repository.clone(), directory).await {
            Ok(link) if link.is_valid() => link.node,
            _ => {
                let node = Node {
                    mode: 0o755,
                    name_hash: lore_storage::hash::hash_string(directory),
                    ..Default::default()
                };
                add_staged(repository, state, ROOT_NODE, node, directory).await
            }
        };
    }
    let file = Context::from(uuid::Uuid::from_u64_pair(
        lore_storage::hash::hash_string(path),
        0,
    ));
    let address = lore_revision::immutable::write(
        repository.clone(),
        file,
        bytes::Bytes::copy_from_slice(path.as_bytes()),
        lore_storage::WriteOptions::default(),
    )
    .await
    .expect("write the file content");
    let node = Node {
        flags: NodeFlags::File.bits(),
        mode: 0o644,
        size: path.len() as u64,
        address,
        name_hash: lore_storage::hash::hash_string(name),
        ..Default::default()
    };
    add_staged(repository, state, parent, node, name).await;
}

/// Commits on `branch` a revision on `parent`, or a first revision where it is zero, that makes
/// `changes`, and moves the branch's latest to it.
async fn commit_files(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent: Hash,
    changes: &[Change],
) -> Hash {
    let state = if parent.is_zero() {
        State::new()
    } else {
        State::deserialize(repository.clone(), parent)
            .await
            .expect("load the parent")
    };
    for change in changes {
        match *change {
            Change::Add(path) => add_file(repository, &state, path).await,
            Change::Delete(path) => {
                let node = node_at(repository, &state, path).await;
                state
                    .node_mark(
                        repository,
                        node,
                        Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
                    )
                    .await
                    .expect("delete the file");
            }
            Change::Move(from, to) => {
                let (directory, name) = to.rsplit_once('/').unwrap_or(("", to));
                let directory = node_at(repository, &state, directory).await;
                let node = node_at(repository, &state, from).await;
                let permit = state.moves_permit().await.expect("the moves permit");
                state
                    .node_move_staged(
                        repository.clone(),
                        node,
                        directory,
                        (name, lore_storage::hash::hash_string(name)),
                        &permit,
                        false,
                    )
                    .await
                    .expect("move the file");
            }
        }
    }
    let mut metadata = lore_revision::metadata::Metadata::new();
    metadata.set_branch(branch).expect("set branch");
    commit_in_memory_revision(
        repository.clone(),
        &get_write_token(),
        state,
        metadata,
        parent,
        branch,
    )
    .await
    .expect("commit the revision")
}

/// [`commit_files`] leaving the latest of `branch` where it is: a revision a client committed on
/// `parent` and has yet to push.
async fn commit_unpushed(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    parent: Hash,
    changes: &[Change],
) -> Hash {
    let latest = branch::load_latest(repository.clone(), branch)
        .await
        .expect("load the latest revision");
    try_store_latest(repository.clone(), branch, latest, parent)
        .await
        .expect("move the latest revision to the parent");
    let revision = commit_files(repository, branch, parent, changes).await;
    try_store_latest(repository.clone(), branch, revision, latest)
        .await
        .expect("move the latest revision back");
    revision
}

/// The path of every node of the revision `revision` below the root, each failing the test where
/// it carries a staged or a dirty flag.
async fn stored_paths(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
) -> std::collections::BTreeSet<String> {
    let state = State::deserialize(repository.clone(), revision)
        .await
        .expect("load the revision");
    let mut paths = std::collections::BTreeSet::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((parent, prefix)) = pending.pop() {
        for child in state
            .node_children(repository.clone(), parent)
            .await
            .expect("list the children")
        {
            let node = state
                .node(repository.clone(), child)
                .await
                .expect("read a child");
            let name = state
                .node_name_clone(repository.clone(), child)
                .await
                .expect("read a name");
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            assert!(
                !node.is_staged() && !node.is_dirty(),
                "{path} is stored with flags {:#x}",
                node.flags
            );
            if node.is_directory() {
                pending.push((child, path.clone()));
            }
            paths.insert(path);
        }
    }
    paths
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
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
            &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        RevisionListAcceleration::default(),
        &mut PushReport::new(PushHandler::Legacy, Default::default()),
    )
    .await
    .expect("push jump revision");
    (result.revision, result.revision_number)
}

/// The time `summary` counts across every phase.
fn phase_total(summary: &PushSummary) -> std::time::Duration {
    PushPhase::ALL
        .into_iter()
        .filter_map(|phase| summary.phase_duration(phase))
        .sum()
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
    use lore_revision::node::NodeFileMetadata;
    use lore_revision::node::NodeFileMetadataBlock;
    use lore_revision::node::node_to_file_metadata;

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
                crate::util::test_support::PUSH_ANY,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
                crate::util::test_support::PUSH_ANY,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
                crate::util::test_support::PUSH_ANY,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
                crate::util::test_support::PUSH_ANY,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut PushReport::new(PushHandler::Legacy, Default::default()),
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
                crate::util::test_support::PUSH_ANY,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut PushReport::new(PushHandler::Legacy, Default::default()),
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

    #[tokio::test]
    async fn a_push_onto_the_head_stores_it_in_one_attempt_without_merging() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;
            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=1).await;
            let state = serialize_revision(&repository, branch, chain[0], Hash::default(), 2).await;

            let mut report = PushReport::new(PushHandler::Legacy, Default::default());
            let result = push(
                repository.clone(),
                branch,
                state.revision(),
                crate::util::test_support::PUSH_ANY,
                false,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut report,
            )
            .await
            .expect("push onto the head");
            assert!(result.success);
            assert!(!result.fast_forward_merged);

            let summary = report.summary();
            assert_eq!(summary.store_latest_attempts, 1);
            assert_eq!(summary.fast_forward_merge_changes, None);
            assert_eq!(summary.phase, PushPhase::Commit);
            assert_eq!(summary.phase_duration(PushPhase::FastForwardMerge), None);
            assert!(summary.num_lookups > 0, "the fragments are verified");
            // The walk and the existence checks run inside the verify phase.
            let verify = summary
                .phase_duration(PushPhase::Verify)
                .expect("the push verifies");
            let collect = summary.verify_collect.expect("the walk is timed");
            let exist = summary
                .verify_exist
                .expect("the existence checks are timed");
            assert!(
                collect + exist <= verify,
                "{collect:?} + {exist:?} within {verify:?}"
            );
            let commit = summary
                .phase_duration(PushPhase::Commit)
                .expect("the head is stored");
            assert!(commit > std::time::Duration::ZERO);
            assert_eq!(phase_total(&summary), summary.elapsed);
        }))
        .await;
    }

    /// The summary reports one metadata read for each node whose file metadata changes in the
    /// pushed revision. It also reports the peak number of metadata reads in flight: one, since all
    /// changed nodes are in one file metadata block.
    #[tokio::test]
    async fn a_push_reports_the_metadata_blobs_it_reads() {
        const CHANGED: u64 = 4;

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;
            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=1).await;

            let state = new_revision(&repository, branch, chain[0], Hash::default(), 2).await;
            let mut nodes = vec![];
            for index in 0..CHANGED {
                let name = format!("dir-{index}");
                let node = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            name_hash: lore_storage::hash::hash_string(&name),
                            ..Default::default()
                        },
                        &name,
                    )
                    .await
                    .expect("Failed to add a node");
                nodes.push(node);
            }
            let block_index = NodeFileMetadataBlock::index(node_to_file_metadata(nodes[0]));
            let metadata_block = state
                .block_file_metadata(repository.clone(), block_index)
                .await
                .expect("Failed to read the file metadata block");
            for (index, &node) in nodes.iter().enumerate() {
                let metadata_node = node_to_file_metadata(node);
                assert_eq!(
                    NodeFileMetadataBlock::index(metadata_node),
                    block_index,
                    "Every node must share one file metadata block"
                );
                let mut metadata = lore_revision::metadata::Metadata::new();
                metadata
                    .set_string("index", &index.to_string())
                    .expect("Failed to set the metadata value");
                let hash = metadata
                    .serialize(repository.clone())
                    .await
                    .expect("Failed to serialize the metadata");
                metadata_block
                    .write()
                    .node(NodeFileMetadata::index(metadata_node))
                    .metadata = hash;
            }
            metadata_block.write().mark_dirty();
            state.block_file_metadata_modified(metadata_block, block_index);
            state.mark_dirty();
            state
                .serialize(repository.clone(), &get_write_token())
                .await
                .expect("serialize state");

            let mut report = PushReport::new(PushHandler::Legacy, Default::default());
            let result = push(
                repository.clone(),
                branch,
                state.revision(),
                crate::util::test_support::PUSH_ANY,
                false,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut report,
            )
            .await
            .expect("push with changed file metadata");
            assert!(result.success);

            let summary = report.summary();
            assert_eq!(summary.num_metadata_reads, CHANGED);
            assert_eq!(summary.peak_metadata_reads_in_flight, 1);
        }))
        .await;
    }

    #[tokio::test]
    async fn a_fast_forward_merge_reports_its_changes_and_the_swap_that_stored_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;
            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=2).await;

            // Rooted at revision 1 while the head is at 2, so the push has to merge the one
            // file it adds.
            let divergent = serialize_revision_adding_a_file(
                &repository,
                branch,
                chain[0],
                Hash::default(),
                2,
                "merged.txt",
            )
            .await;

            let mut report = PushReport::new(PushHandler::Legacy, Default::default());
            let result = push(
                repository.clone(),
                branch,
                divergent.revision(),
                crate::util::test_support::PUSH_ANY,
                false,
                true,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut report,
            )
            .await
            .expect("fast-forward merge");
            assert!(result.fast_forward_merged);

            let summary = report.summary();
            assert_eq!(summary.store_latest_attempts, 1);
            assert_eq!(summary.fast_forward_merge_changes, Some(1));
            assert_eq!(summary.phase, PushPhase::Commit);
            let merge = summary
                .phase_duration(PushPhase::FastForwardMerge)
                .expect("the merge is timed");
            assert!(merge > std::time::Duration::ZERO);
            assert_eq!(phase_total(&summary), summary.elapsed);
        }))
        .await;
    }

    /// Pushes `revision` to `branch`, merging it on the server where the branch moved past its
    /// parent.
    async fn push_merging(
        repository: &Arc<RepositoryContext>,
        branch: BranchId,
        revision: Hash,
    ) -> PushResult {
        push(
            repository.clone(),
            branch,
            revision,
            crate::util::test_support::PUSH_ANY,
            false,
            true,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
            &mut PushReport::new(PushHandler::Legacy, Default::default()),
        )
        .await
        .expect("push with a fast-forward merge")
    }

    /// A fast-forward merge stores the tree a commit of the pushed changes on the latest revision
    /// stores: no node staged, the deleted file gone, and every directory hashed over what it holds.
    #[tokio::test]
    async fn a_fast_forward_merge_stores_the_tree_a_commit_stores() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;
            let base = commit_files(
                &repository,
                branch,
                Hash::default(),
                &[Change::Add("dir/a.txt"), Change::Add("dir/b.txt")],
            )
            .await;
            let latest = commit_files(&repository, branch, base, &[Change::Add("other.txt")]).await;
            let pushed = [Change::Add("dir/c.txt"), Change::Delete("dir/b.txt")];
            let incoming = commit_unpushed(&repository, branch, base, &pushed).await;
            let expected = commit_unpushed(&repository, branch, latest, &pushed).await;

            let result = push_merging(&repository, branch, incoming).await;
            assert!(result.fast_forward_merged);

            assert_eq!(
                stored_paths(&repository, result.revision).await,
                stored_paths(&repository, expected).await
            );
            let mut roots = Vec::new();
            for revision in [result.revision, expected] {
                let state = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("load the revision");
                roots.push(
                    state
                        .tree(repository.clone())
                        .await
                        .expect("read the tree")
                        .hash_root,
                );
            }
            assert_eq!(roots[0], roots[1]);
        }))
        .await;
    }

    /// A push moving a file that a fast-forward merge at the latest revision deleted is declined:
    /// the merge revision's parents and branch reach the 3-way diff as a client's would.
    #[tokio::test]
    async fn a_push_moving_a_file_a_merge_at_latest_deleted_is_declined() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;
            let base = commit_files(
                &repository,
                branch,
                Hash::default(),
                &[Change::Add("dir/a.txt"), Change::Add("other.txt")],
            )
            .await;
            let deleting =
                commit_unpushed(&repository, branch, base, &[Change::Delete("dir/a.txt")]).await;
            commit_files(&repository, branch, base, &[Change::Add("more.txt")]).await;
            assert!(
                push_merging(&repository, branch, deleting)
                    .await
                    .fast_forward_merged
            );
            let moving = commit_unpushed(
                &repository,
                branch,
                base,
                &[Change::Move("dir/a.txt", "dir/z.txt")],
            )
            .await;

            assert!(!push_merging(&repository, branch, moving).await.success);
        }))
        .await;
    }

    #[tokio::test]
    async fn a_push_onto_a_moved_head_without_a_merge_is_declined() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                random::<RepositoryId>(),
            ));
            let branch = create_test_branch(&repository).await;
            let chain = push_linear_revisions(&repository, branch, Hash::default(), 1..=2).await;
            let stale = serialize_revision_adding_a_file(
                &repository,
                branch,
                chain[0],
                Hash::default(),
                2,
                "stale.txt",
            )
            .await;

            let mut report = PushReport::new(PushHandler::Legacy, Default::default());
            let result = push(
                repository.clone(),
                branch,
                stale.revision(),
                crate::util::test_support::PUSH_ANY,
                false,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &mut report,
            )
            .await
            .expect("a declined push is answered, not failed");
            assert!(!result.success);
            assert_eq!(result.revision, chain[1]);

            report.finish();
            assert_eq!(report.summary().outcome, PushOutcome::Declined);
        }))
        .await;
    }
}

mod push_outcome_of {
    use super::*;

    #[test]
    fn a_push_that_returned_is_declined_ahead_of_errored_and_completed_otherwise() {
        assert_eq!(PushOutcome::of(true, true, true), PushOutcome::Declined);
        assert_eq!(PushOutcome::of(true, true, false), PushOutcome::Declined);
        assert_eq!(PushOutcome::of(true, false, true), PushOutcome::Error);
        assert_eq!(PushOutcome::of(true, false, false), PushOutcome::Completed);
    }

    #[test]
    fn a_push_dropped_before_it_returned_is_cancelled_whatever_it_had_set() {
        assert_eq!(PushOutcome::of(false, false, false), PushOutcome::Cancelled);
        assert_eq!(PushOutcome::of(false, true, true), PushOutcome::Cancelled);
    }
}

mod summary {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_phase_entered_again_accumulates_and_time_after_finishing_is_not_counted() {
        let mut report = PushReport::new(PushHandler::V1, Default::default());
        tokio::time::advance(Duration::from_secs(1)).await;
        // A merge whose swap loses goes back to merging onto the new head.
        report.enter(PushPhase::FastForwardMerge);
        tokio::time::advance(Duration::from_secs(2)).await;
        report.enter(PushPhase::Commit);
        tokio::time::advance(Duration::from_secs(1)).await;
        report.enter(PushPhase::FastForwardMerge);
        tokio::time::advance(Duration::from_secs(3)).await;
        report.enter(PushPhase::Commit);
        tokio::time::advance(Duration::from_secs(1)).await;
        report.finish();
        tokio::time::advance(Duration::from_secs(5)).await;

        let summary = report.summary();
        assert_eq!(summary.outcome, PushOutcome::Completed);
        assert_eq!(summary.elapsed, Duration::from_secs(8));
        assert_eq!(summary.phase, PushPhase::Commit);
        assert_eq!(
            summary.phase_duration(PushPhase::Prepare),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            summary.phase_duration(PushPhase::FastForwardMerge),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            summary.phase_duration(PushPhase::Commit),
            Some(Duration::from_secs(2))
        );
        assert_eq!(summary.phase_duration(PushPhase::Verify), None);
    }

    /// A report dropped without finishing is how a cancelled push is recorded, so its
    /// snapshot names the phase it was cut off in and counts that phase up to the drop.
    #[tokio::test(start_paused = true)]
    async fn an_unfinished_report_counts_its_running_phase_up_to_the_snapshot() {
        let mut report = PushReport::new(PushHandler::Legacy, Default::default());
        tokio::time::advance(Duration::from_secs(1)).await;
        report.enter(PushPhase::Verify);
        tokio::time::advance(Duration::from_secs(7)).await;

        let summary = report.summary();
        assert_eq!(summary.outcome, PushOutcome::Cancelled);
        assert_eq!(summary.elapsed, Duration::from_secs(8));
        assert_eq!(summary.phase, PushPhase::Verify);
        assert_eq!(
            summary.phase_duration(PushPhase::Verify),
            Some(Duration::from_secs(7))
        );
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

mod push_protected_action {
    use lore_base::types::Hash;
    use lore_server::hooks::HookDispatcher;
    use lore_telemetry::InstrumentProvider;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use opentelemetry::KeyValue;

    use super::*;
    use crate::grpc::push_protected::PUSH_PROTECTED;
    use crate::grpc::push_protected::TIERS;
    use crate::grpc::push_protected::Tier;
    use crate::notification::testing::MockNotificationSender;

    struct TestInstrumentProvider;

    impl InstrumentProvider for TestInstrumentProvider {
        fn namespace(&self) -> &'static str {
            "test"
        }
        fn labels(&self) -> &[KeyValue] {
            &[]
        }
    }

    async fn push_to_protected_branch(
        tier: Tier,
        actions: &'static [&'static str],
        service_account: bool,
    ) -> Result<(), tonic::Status> {
        push_to_branch(tier, actions, service_account, true).await
    }

    async fn push_to_branch(
        tier: Tier,
        actions: &'static [&'static str],
        service_account: bool,
        protected: bool,
    ) -> Result<(), tonic::Status> {
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_pushed()
            .returning(|_, _, _, _, _| ());
        let notification_sender = Arc::new(notification_sender);

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let main = create_test_branch(&repository).await;
            if protected {
                branch::protect(repository.clone(), main)
                    .await
                    .expect("should protect");
            }
            let state =
                serialize_revision(&repository, main, Hash::default(), Hash::default(), 1).await;

            let mut request = tonic::Request::new(lore_proto::BranchPushRequest {
                branch: main.into(),
                revision: state.revision().into(),
                force: false,
                fast_forward_merge: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository_id.data()),
            );
            let mut request = tier.with_token(request, repository_id, actions);
            if service_account {
                // Replaces the claims with a token whose only privilege is the removed claim.
                request
                    .extensions_mut()
                    .insert(lore_server::auth::jwt::AuthorizationToken {
                        user_id: "service-bot".into(),
                        is_service_account: Some(true),
                        ..Default::default()
                    });
            }

            let hook_dispatcher = HookDispatcher::empty();
            handler(
                request,
                tier.authorizer(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &TestInstrumentProvider,
            )
            .await
            .map(|_| ())
        }))
        .await
    }

    #[tokio::test]
    async fn the_action_bypasses_protection() {
        for tier in TIERS {
            push_to_protected_branch(tier, &[PUSH_PROTECTED], false)
                .await
                .unwrap_or_else(|err| panic!("{tier:?}: holder of the action should push: {err}"));
        }
    }

    #[tokio::test]
    async fn without_the_action_a_protected_branch_is_denied() {
        for tier in TIERS {
            let err = push_to_protected_branch(tier, &["read", "write"], false)
                .await
                .expect_err("a caller without the action must not push");
            assert_eq!(err.code(), Code::PermissionDenied, "{tier:?}");
        }
    }

    #[tokio::test]
    async fn the_service_account_claim_no_longer_bypasses_protection() {
        for tier in TIERS {
            let err = push_to_protected_branch(tier, &[], true)
                .await
                .expect_err("the claim alone must not waive protection");
            assert_eq!(err.code(), Code::PermissionDenied, "{tier:?}");
        }
    }

    /// An unprotected branch needs `write`, and only `write`: `push-protected`
    /// answers for protected branches. Tier 1 is left out, since it grants
    /// `write` to every token.
    #[tokio::test]
    async fn an_unprotected_branch_needs_write() {
        for tier in [Tier::Resource, Tier::AuthClient] {
            push_to_branch(tier, &["write"], false, false)
                .await
                .unwrap_or_else(|err| panic!("{tier:?}: `write` should push: {err}"));
            for actions in [&["read"][..], &[PUSH_PROTECTED][..]] {
                let err = push_to_branch(tier, actions, false, false)
                    .await
                    .expect_err("an unprotected push needs `write`");
                assert_eq!(err.code(), Code::PermissionDenied, "{tier:?} {actions:?}");
            }
        }
    }
}
