// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A view over a small revision tree, a record and a frozen snapshot, for the tests of what the
//! view answers.
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::modified_tree::ModifiedTree;
use lore_revision::fs::swfs::served_tree::ServedTree;
use lore_revision::fs::swfs::view::SwfsView;
use lore_revision::link::LinkFlags;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::allow_all_repositories;
use lore_revision::util::path::RelativePath;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::fragment_flags::FragmentFlags;

use super::fake_frozen::FakeFrozen;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::default_repository_creation_args;

/// What a revision holds at a path.
#[derive(Clone, Copy)]
pub enum Held<'a> {
    File(&'a [u8]),
    /// A file a staged revision deletes, which it keeps as a node flagged for it.
    StagedDeleteFile(&'a [u8]),
    Directory,
    /// A link node naming nothing.
    Link,
    /// A link to the node `source` of `repository`'s revision `pin`, listed in the state's links.
    LinkTo {
        repository: RepositoryId,
        pin: Hash,
        source: NodeID,
    },
}

pub struct Fixture {
    pub view: SwfsView,
    pub repository: Arc<RepositoryContext>,
}

pub fn address_of(content: &[u8]) -> Address {
    Address {
        hash: lore_storage::hash::hash_slice(content),
        context: Context::from([4; 16]),
    }
}

/// A view over a revision holding `revision`, a record holding `changes`, and a mount whose frozen
/// files are `frozen`. Parent directories of a path are created as needed, and a file's content
/// is in the immutable store.
pub async fn fixture(
    revision: &[(&str, Held<'_>)],
    changes: Vec<Change>,
    frozen: FakeFrozen,
    trusted: bool,
) -> Fixture {
    let repository = test_repository().await;
    let state = revision_state(&repository, revision).await;
    let record = Arc::new(ModifiedTree::new(repository.clone()));
    for change in changes {
        record.record(change).await;
    }
    record.set_trusted(trusted);
    Fixture {
        view: SwfsView::new(
            record,
            FrozenSnapshot::capture(Arc::new(frozen)).expect("capturing the snapshot"),
            Arc::new(ServedTree::new(repository.clone(), Some(state), Vec::new())),
        ),
        repository,
    }
}

/// A repository over fresh test stores.
pub async fn test_repository() -> Arc<RepositoryContext> {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Making test stores");
    Arc::new(RepositoryContext::new(default_repository_creation_args(
        immutable_store,
        mutable_store,
    )))
}

/// A repository over fresh test stores that can store states, as a link's pin needs.
pub async fn writable_test_repository() -> Arc<RepositoryContext> {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Making test stores");
    Arc::new(RepositoryContext::new_null_context(
        immutable_store,
        mutable_store,
        allow_all_repositories(),
    ))
}

/// A revision state holding `revision`. Parent directories of a path are created as needed, and
/// a file's content is in the immutable store.
pub async fn revision_state(
    repository: &Arc<RepositoryContext>,
    revision: &[(&str, Held<'_>)],
) -> Arc<State> {
    let state = State::new();
    for (path, held) in revision {
        let mut parent = ROOT_NODE;
        let segments: Vec<&str> = path.split('/').collect();
        for (index, segment) in segments.iter().enumerate() {
            let last = index + 1 == segments.len();
            let name_hash = lore_storage::hash::hash_string(segment);
            if let Ok(existing) = state
                .find_subnode(repository.clone(), parent, name_hash)
                .await
            {
                parent = existing;
                continue;
            }
            let node = match (last, held) {
                (true, Held::File(content)) => {
                    store_content(repository, content).await;
                    Node {
                        flags: NodeFlags::File.bits(),
                        name_hash,
                        address: address_of(content),
                        size: content.len() as u64,
                        ..Default::default()
                    }
                }
                (true, Held::StagedDeleteFile(content)) => {
                    store_content(repository, content).await;
                    Node {
                        flags: (NodeFlags::File | NodeFlags::StagedDelete).bits(),
                        name_hash,
                        address: address_of(content),
                        size: content.len() as u64,
                        ..Default::default()
                    }
                }
                (true, Held::Link) => Node {
                    flags: NodeFlags::Link.bits(),
                    name_hash,
                    ..Default::default()
                },
                (
                    true,
                    Held::LinkTo {
                        repository,
                        pin,
                        source,
                    },
                ) => Node {
                    flags: NodeFlags::Link.bits(),
                    name_hash,
                    address: Address {
                        hash: *pin,
                        context: Context::from(*repository),
                    },
                    child: *source,
                    ..Default::default()
                },
                _ => Node {
                    name_hash,
                    ..Default::default()
                },
            };
            parent = state
                .node_add(repository.clone(), parent, node, segment)
                .await
                .expect("adding a revision node");
            if last
                && let Held::LinkTo {
                    repository: linked,
                    pin,
                    ..
                } = held
            {
                state
                    .link_add(
                        repository.clone(),
                        *linked,
                        BranchId::default(),
                        *pin,
                        parent,
                        LinkFlags::NoFlags,
                    )
                    .await
                    .expect("listing the link");
            }
        }
    }
    state
}

/// `state` of `repository`, stored, by the signature a link pins it at.
pub async fn stored(repository: &Arc<RepositoryContext>, state: &Arc<State>) -> Hash {
    let token = repository
        .try_write_token()
        .expect("a test repository carries a write token");
    state
        .serialize(repository.clone(), token)
        .await
        .expect("storing the state")
}

async fn store_content(repository: &RepositoryContext, content: &[u8]) {
    repository
        .immutable_store()
        .put(
            repository.id,
            address_of(content),
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            },
            Some(Bytes::copy_from_slice(content)),
            false,
        )
        .await
        .expect("storing revision content");
}

pub async fn in_context<F: Future>(test: F) -> F::Output {
    let (_, _, execution) = test_store_create().await.expect("Making test stores");
    LORE_CONTEXT.scope(execution, test).await
}

pub fn node(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("a relative path")
}

pub fn written(path: &str) -> Change {
    Change::Written(path.into())
}

pub fn created(path: &str, directory: bool) -> Change {
    Change::Created {
        path: path.into(),
        directory,
    }
}

pub fn deleted(path: &str, directory: bool) -> Change {
    Change::Deleted {
        path: path.into(),
        directory,
    }
}

/// A filter holding `rules` as a `.loreignore` holds them.
pub fn ignoring(rules: &str) -> Arc<lore_revision::filter::Filter> {
    let ignore =
        lore_revision::filter::parse_filter(rules.as_bytes(), std::path::Path::new(".loreignore"))
            .expect("parsing rules");
    Arc::new(
        lore_revision::filter::from_instances(
            ignore,
            lore_revision::filter::FilterInstance::default(),
        )
        .expect("building filter"),
    )
}
