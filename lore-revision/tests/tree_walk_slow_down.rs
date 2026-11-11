// SPDX-FileCopyrightText: 2026 LoreLab.io
// SPDX-License-Identifier: MIT

//! Reads of a revision's tree that the store refuses as `SlowDown`. Its own process, as it lowers
//! the process-wide store retry bound to one try, so the refusal reaches the reader at once.

#[path = "helper.rs"]
mod helper;

use std::sync::Arc;

use bytes::Bytes;
use helper::RepositoryContextCreationArgsExt;
use helper::in_memory;
use lore_base::error::SlowDown;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::VerifyResult;
use lore_revision::dependency::resolve::resolve_dependency_file_set;
use lore_revision::link::LinkFlags;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::tree;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::Tree;
use lore_revision::state::TreeWalk;
use lore_revision::state::allow_all_repositories;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;
use lore_transport::Connection;
use lore_transport::ProtocolError;

/// A remote serving what `source` stores, answering every get of `refused` as `SlowDown`.
struct RefusingRemote {
    source: Arc<RepositoryContext>,
    refused: Hash,
}

#[async_trait::async_trait]
impl lore_transport::Storage for RefusingRemote {
    async fn session_start(
        &self,
        _partition: Partition,
        _correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        Ok(1)
    }

    async fn session_stop(&self, _session_id: u32) -> Result<(), ProtocolError> {
        Ok(())
    }

    async fn get(
        &self,
        _session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        if address.hash == self.refused {
            return Err(ProtocolError::from(SlowDown));
        }
        let stored = self
            .source
            .immutable_store()
            .get(self.source.id, *address)
            .await
            .map_err(|err| ProtocolError::internal(err.to_string()))?;
        let payload = stored
            .payload
            .ok_or_else(|| ProtocolError::internal("the source holds no payload"))?;
        Ok((stored.fragment, payload))
    }

    async fn put(
        &self,
        _session_id: u32,
        _address: Address,
        _fragment: Fragment,
        _payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        Err(ProtocolError::internal("the remote takes no puts"))
    }

    async fn query(&self, _session_id: u32, _address: &[Address]) -> Result<Bytes, ProtocolError> {
        Err(ProtocolError::internal("the remote takes no queries"))
    }

    async fn verify(
        &self,
        _session_id: u32,
        _address: &Address,
        _heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        Err(ProtocolError::internal("the remote verifies nothing"))
    }

    async fn copy(
        &self,
        _session_id: u32,
        _source_partition: Partition,
        _source_address: Address,
        _target_context: Context,
    ) -> Result<(), ProtocolError> {
        Err(ProtocolError::internal("the remote takes no copies"))
    }
}

/// Runs `test` on an empty repository, with the store retry bound lowered to one try.
async fn with_repository(test: impl AsyncFnOnce(Arc<RepositoryContext>)) {
    let _ = lore_storage::STORE_RETRY_ATTEMPTS.set(1);
    let (immutable, mutable, execution) = helper::test_store_create()
        .await
        .expect("creating the stores");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        test(in_memory::repository(immutable, mutable)).await;
    }))
    .await;
}

/// Commits two files to `source`, then a change to one of them, answering the second revision and
/// its tree.
async fn commit_a_change(source: &Arc<RepositoryContext>) -> (Hash, Tree) {
    let main = in_memory::branch_id();
    let state = State::new();
    let changed = in_memory::file("changed.bin");
    let node = in_memory::add(&state, source.clone(), ROOT_NODE, changed, "changed.bin").await;
    let kept = in_memory::file("kept.bin");
    in_memory::add(&state, source.clone(), ROOT_NODE, kept, "kept.bin").await;
    let first = in_memory::commit(source, &state, Hash::default(), main).await;
    let state = in_memory::revision_state(source, first).await;
    let content = Address {
        hash: Hash::from_u64(0x100),
        context: Context::default(),
    };
    in_memory::modify(&state, source, node, content, NodeFlags::StagedModify).await;
    let second = in_memory::commit(source, &state, first, main).await;
    let tree = in_memory::revision_state(source, second)
        .await
        .tree(source.clone())
        .await
        .expect("the tree of the revision");
    (second, tree)
}

/// A repository with nothing stored, reading through a remote that serves what `source` stores
/// and refuses `refused` as `SlowDown`.
async fn refusing_reader(source: &Arc<RepositoryContext>, refused: Hash) -> Arc<RepositoryContext> {
    let (immutable, mutable, _) = helper::test_store_create()
        .await
        .expect("creating the stores");
    let remote = Arc::new(RefusingRemote {
        source: source.clone(),
        refused,
    });
    Arc::new(RepositoryContext::new(
        helper::default_repository_creation_args(immutable, mutable)
            .with_id(source.id)
            .with_remote(Ok(Connection::detached(vec![remote]))),
    ))
}

/// The walk of the tree of `revision`, `max_depth` levels deep or all of them for 0, by
/// [`refusing_reader`].
async fn walk_refusing(
    source: &Arc<RepositoryContext>,
    revision: Hash,
    refused: Hash,
    max_depth: usize,
    attribute: bool,
) -> Result<TreeWalk, StateError> {
    tree(
        refusing_reader(source, refused).await,
        revision,
        RelativePath::default(),
        max_depth,
        allow_all_repositories(),
        attribute,
    )
    .await
}

/// A file-history read refused as `SlowDown` fails an attributing walk with it. A walk that does
/// not attribute reads no file history.
#[tokio::test]
async fn a_refused_file_history_read_fails_an_attributing_walk() {
    with_repository(async |source| {
        let (revision, tree) = commit_a_change(&source).await;
        let refused = tree.hash_file_metadata;
        let attributing = walk_refusing(&source, revision, refused, 0, true).await;
        assert!(
            attributing.as_ref().is_err_and(|err| err.is_slow_down()),
            "{:?}",
            attributing.err()
        );
        let listing = walk_refusing(&source, revision, refused, 0, false)
            .await
            .expect("a walk that does not attribute");
        assert_eq!(listing.paths.len(), 2);
    })
    .await;
}

/// A delta read refused as `SlowDown` fails an attributing walk with it.
#[tokio::test]
async fn a_refused_delta_read_fails_an_attributing_walk() {
    with_repository(async |source| {
        let (revision, tree) = commit_a_change(&source).await;
        let attributing = walk_refusing(&source, revision, tree.hash_delta, 0, true).await;
        assert!(
            attributing.as_ref().is_err_and(|err| err.is_slow_down()),
            "{:?}",
            attributing.err()
        );
    })
    .await;
}

/// A link-list read refused as `SlowDown` fails a walk with it, attributing or not.
#[tokio::test]
async fn a_refused_link_list_read_fails_the_walk() {
    with_repository(async |source| {
        let target: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
        let pinned = Hash::from_u64(0x33);
        let link = Node {
            flags: NodeFlags::Link.bits(),
            mode: 0o755,
            name_hash: hash_string("vendor"),
            child: ROOT_NODE,
            address: Address {
                hash: pinned,
                context: target.into(),
            },
            ..Default::default()
        };
        let state = State::new();
        let node = in_memory::add(&state, source.clone(), ROOT_NODE, link, "vendor").await;
        state
            .link_add(
                source.clone(),
                target,
                in_memory::branch_id(),
                pinned,
                node,
                LinkFlags::NoFlags,
            )
            .await
            .expect("registering the link");
        let revision =
            in_memory::commit(&source, &state, Hash::default(), in_memory::branch_id()).await;
        let refused = in_memory::revision_state(&source, revision)
            .await
            .link_list_hash();
        for attribute in [false, true] {
            let walked = walk_refusing(&source, revision, refused, 1, attribute).await;
            assert!(
                walked.as_ref().is_err_and(|err| err.is_slow_down()),
                "{:?}",
                walked.err()
            );
        }
    })
    .await;
}

/// A node-block read refused as `SlowDown` fails the lookup of a dependency set's root file with
/// it, rather than reporting the file missing.
#[tokio::test]
async fn a_refused_node_block_read_fails_a_root_file_lookup() {
    with_repository(async |source| {
        let (revision, tree) = commit_a_change(&source).await;
        let reader = refusing_reader(&source, tree.hash_node).await;
        let state = State::deserialize(reader.clone(), revision)
            .await
            .expect("the revision state");
        let resolved =
            resolve_dependency_file_set(reader, state, &["kept.bin".to_string()], &[], false, 0)
                .await;
        assert!(
            resolved.as_ref().is_err_and(|err| err.is_slow_down()),
            "{:?}",
            resolved.err()
        );
    })
    .await;
}
