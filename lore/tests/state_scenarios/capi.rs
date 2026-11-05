// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The C API's revision-tree verbs, called as a caller of `lore` calls them, on a handle loaded
//! over in-memory stores, with each call's events recorded.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::delete::LoreRevisionTreeDeleteArgs;
use lore::revision_tree::delete::LoreRevisionTreeDeleteEntry;
use lore::revision_tree::handle as tree_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
use lore::revision_tree::move_node::LoreRevisionTreeMoveArgs;
use lore::revision_tree::move_node::LoreRevisionTreeMoveEntry;
use lore::storage::handle as store_handle;
use lore::storage::handle::LoreStore;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

/// A call's status before it returns, which a dropped call keeps.
pub const PENDING: i32 = i32::MIN;

/// The node at `path` of `nodes`, the root at `""`.
pub fn id_of(nodes: &[(&'static str, NodeID)], path: &str) -> NodeID {
    if path.is_empty() {
        return ROOT_NODE;
    }
    nodes
        .iter()
        .find(|(p, _)| *p == path)
        .map_or_else(|| panic!("no node made for '{path}'"), |(_, id)| *id)
}

/// What a call reported, in the order it reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reported {
    /// The call's end, with its status and, on failure, its error's message.
    Complete(i32, String),
    Loaded(u64),
    Add(u64, NodeID, LoreErrorCode),
    Delete(u64, u64, LoreErrorCode),
    Move(u64, NodeID, LoreErrorCode),
    Modify(u64, NodeID, LoreErrorCode),
    Batch(u64, LoreErrorCode),
    /// A listed child: its id, kind and staged action.
    Child(NodeID, u32, u32),
    /// A node's info: its id, kind and staged action.
    NodeInfo(NodeID, u32, u32),
    /// A commit's outcome.
    Commit(LoreErrorCode),
    Other(u32),
}

impl Reported {
    fn of(event: &LoreEvent) -> Self {
        match event {
            LoreEvent::Complete(data) => {
                Self::Complete(data.status, data.error.message.as_str().to_string())
            }
            LoreEvent::RevisionTreeLoaded(data) => Self::Loaded(data.handle_id),
            LoreEvent::RevisionTreeAddComplete(data) => {
                Self::Add(data.entry_id, data.node_id, data.error_code)
            }
            LoreEvent::RevisionTreeDeleteComplete(data) => {
                Self::Delete(data.entry_id, data.node_count, data.error_code)
            }
            LoreEvent::RevisionTreeMoveComplete(data) => {
                Self::Move(data.entry_id, data.node_id, data.error_code)
            }
            LoreEvent::RevisionTreeModifyComplete(data) => {
                Self::Modify(data.entry_id, data.node_id, data.error_code)
            }
            LoreEvent::RevisionTreeBatchComplete(data) => {
                Self::Batch(data.batch_id, data.error_code)
            }
            LoreEvent::RevisionTreeChild(data) => {
                Self::Child(data.node_id, data.kind, data.staged_action)
            }
            LoreEvent::RevisionTreeNodeInfo(data) => {
                Self::NodeInfo(data.node_id, data.kind, data.staged_action)
            }
            LoreEvent::RevisionTreeCommitComplete(data) => Self::Commit(data.error_code),
            other => Self::Other(other.discriminant()),
        }
    }
}

/// The events of one call, as they arrive.
#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<Vec<Reported>>>);

impl Events {
    fn callback(&self) -> LoreEventCallback {
        let sink = self.0.clone();
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().expect("the events").push(Reported::of(event));
        }))
    }

    /// Every event so far.
    pub fn all(&self) -> Vec<Reported> {
        self.0.lock().expect("the events").clone()
    }
}

/// A loaded revision-tree handle and its storage handle, both released when dropped.
pub struct Handle {
    pub tree: LoreRevisionTree,
    store: LoreStore,
}

impl Drop for Handle {
    fn drop(&mut self) {
        tree_handle::unregister(self.tree);
        store_handle::unregister(self.store);
    }
}

impl Handle {
    /// An empty tree loaded over in-memory stores, in a repository of its own: the storage
    /// layer's in-flight writes are shared by partition across stores, so two handles of one
    /// partition committing one block at once would each wait on the other's store.
    pub async fn load() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut repository = [0x18u8; 16];
        repository[..8].copy_from_slice(&NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        let store = store_handle::register(
            lore::storage::store::in_memory_for_tests("state-scenarios").await,
        );
        let events = Events::default();
        let status = lore::revision_tree::load::load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store,
                repository: Partition::from(repository),
                revision_hash: Hash::default(),
            },
            events.callback(),
        )
        .await;
        assert_eq!(status, 0, "the tree loads");
        let handle_id = events
            .all()
            .iter()
            .find_map(|event| match event {
                Reported::Loaded(handle_id) => Some(*handle_id),
                _ => None,
            })
            .expect("the load reports its handle");
        Self {
            tree: LoreRevisionTree { handle_id },
            store,
        }
    }

    /// The handle's tree, for a check run with no call in flight.
    pub fn state(&self) -> Arc<State> {
        tree_handle::lookup(self.tree)
            .expect("the handle resolves")
            .state_for_tests()
    }

    /// The handle's repository.
    pub fn repository(&self) -> Arc<RepositoryContext> {
        tree_handle::lookup(self.tree)
            .expect("the handle resolves")
            .repository_context
            .clone()
    }

    /// Adds `name` under `parent` in a call of its own, which must succeed.
    pub async fn seed(&self, parent: NodeID, name: &str, kind: LoreNodeType) -> NodeID {
        let events = Events::default();
        let status = add(self.tree, parent, name, kind, &events).await;
        assert_eq!(status, 0, "seeding '{name}' succeeds: {:?}", events.all());
        added(&events).unwrap_or_else(|| panic!("seeding '{name}' reports its node"))
    }

    /// Adds `paths` in order, directories ending in `/`, each in a call of its own, which must
    /// succeed, a parent before its children; answers each node by its path without the `/`.
    pub async fn seed_paths(&self, paths: &[&'static str]) -> Vec<(&'static str, NodeID)> {
        let mut nodes = Vec::with_capacity(paths.len());
        for &path in paths {
            let kind = if path.ends_with('/') {
                LoreNodeType::Directory
            } else {
                LoreNodeType::File
            };
            let path = path.trim_end_matches('/');
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            let parent = id_of(&nodes, parent);
            nodes.push((path, self.seed(parent, name, kind).await));
        }
        nodes
    }

    /// A stand-in for nodes the loaded revision holds: the change flags their adds left cleared,
    /// as a commit clears them.
    pub async fn settle(&self, nodes: &[NodeID]) {
        let state = self.state();
        let repository = self.repository();
        for &node in nodes {
            let block_index = NodeBlock::index(node);
            let block = state
                .block(repository.clone(), block_index)
                .await
                .expect("the node's block");
            block
                .write_for_tests()
                .node(Node::index(node))
                .clear_all_change_flags();
        }
    }
}

/// One add call of one entry. A wave of several entries groups them through a `HashMap`, whose
/// order differs from run to run, so a scheduled scenario would not replay: scenarios add one
/// entry per call.
pub async fn add(
    tree: LoreRevisionTree,
    parent: NodeID,
    name: &str,
    kind: LoreNodeType,
    events: &Events,
) -> i32 {
    lore::revision_tree::add::add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: 1,
            handle: tree,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
                entry_id: 1,
                parent_node_id: parent,
                parent_entry_index: 0,
                name: LoreString::from_str(name),
                kind: kind as u32,
                mode: 0o644,
                size: 10,
                address: Address {
                    hash: Hash::from_u64(lore_storage::hash::hash_string(name)),
                    ..Address::default()
                },
            }]),
        },
        events.callback(),
    )
    .await
}

/// The node an add call reported for its entry, if it succeeded.
pub fn added(events: &Events) -> Option<NodeID> {
    events.all().iter().find_map(|event| match event {
        Reported::Add(_, node, LoreErrorCode::None) => Some(*node),
        _ => None,
    })
}

/// One delete call of `nodes`, an entry each.
pub async fn delete(tree: LoreRevisionTree, nodes: &[NodeID], events: &Events) -> i32 {
    let entries = nodes
        .iter()
        .enumerate()
        .map(|(index, &node_id)| LoreRevisionTreeDeleteEntry {
            entry_id: index as u64 + 1,
            node_id,
        })
        .collect();
    lore::revision_tree::delete::delete(
        LoreGlobalArgs::default(),
        LoreRevisionTreeDeleteArgs {
            batch_id: 2,
            handle: tree,
            entries: LoreArray::from_vec(entries),
        },
        events.callback(),
    )
    .await
}

/// One move call of one entry.
pub async fn move_node(
    tree: LoreRevisionTree,
    node: NodeID,
    destination: NodeID,
    name: &str,
    events: &Events,
) -> i32 {
    lore::revision_tree::move_node::move_node(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMoveArgs {
            batch_id: 3,
            handle: tree,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeMoveEntry {
                entry_id: 1,
                node_id: node,
                destination_parent_id: destination,
                dst_name: LoreString::from_str(name),
            }]),
        },
        events.callback(),
    )
    .await
}

/// One modify call of one entry.
pub async fn modify(tree: LoreRevisionTree, node: NodeID, events: &Events) -> i32 {
    lore::revision_tree::modify::modify(
        LoreGlobalArgs::default(),
        LoreRevisionTreeModifyArgs {
            batch_id: 4,
            handle: tree,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeModifyEntry {
                entry_id: 1,
                node_id: node,
                mode: 0o600,
                size: 20,
                address: Address {
                    hash: Hash::from_u64(0x4d4f_4449_4659),
                    ..Address::default()
                },
            }]),
        },
        events.callback(),
    )
    .await
}

/// One listing call of `parent`'s children.
pub async fn list_children(tree: LoreRevisionTree, parent: NodeID, events: &Events) -> i32 {
    lore::revision_tree::list_children::list_children(
        LoreGlobalArgs::default(),
        lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs {
            id: 5,
            handle: tree,
            parent_node_id: parent,
        },
        events.callback(),
    )
    .await
}

/// One info call of `node`.
pub async fn node_info(tree: LoreRevisionTree, node: NodeID, events: &Events) -> i32 {
    lore::revision_tree::node_info::node_info(
        LoreGlobalArgs::default(),
        lore::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs {
            id: 6,
            handle: tree,
            node_id: node,
        },
        events.callback(),
    )
    .await
}

/// Names the branch the tree's commits go to, in a call of its own, which must succeed.
pub async fn set_branch(tree: LoreRevisionTree) {
    let events = Events::default();
    let status = lore::revision_tree::metadata_set::metadata_set(
        LoreGlobalArgs::default(),
        lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs {
            batch_id: 7,
            handle: tree,
            entries: LoreArray::from_vec(vec![
                lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry {
                    entry_id: 1,
                    key: LoreString::from_str(lore_revision::metadata::BRANCH),
                    value: lore_revision::interface::LoreMetadata::Context(
                        lore_base::types::Context::from([0x5au8; 16]),
                    ),
                },
            ]),
        },
        events.callback(),
    )
    .await;
    assert_eq!(status, 0, "setting the branch succeeds: {:?}", events.all());
}

/// One commit call.
pub async fn commit(tree: LoreRevisionTree, events: &Events) -> i32 {
    lore::revision_tree::commit::commit(
        LoreGlobalArgs::default(),
        lore::revision_tree::commit::LoreRevisionTreeCommitArgs {
            id: 8,
            handle: tree,
            options: lore::revision_tree::commit::LoreRevisionTreeCommitOptions::default(),
        },
        events.callback(),
    )
    .await
}
