// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Fixtures and event readers the revision tree verbs' tests share: loading a handle, settling
//! and listing nodes, and reading the events a verb sends. The harness every dispatch wrapper's
//! tests share is `call::test_support`.

use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
use lore::revision_tree::list_children::list_children;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
use lore::revision_tree::node_info::node_info;
use lore::storage::handle as storage_handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::in_memory_for_tests;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::call::test_support::Recorded;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;

/// The handle id of the first `RevisionTreeLoaded` event in `events`.
pub(crate) fn loaded_handle_id(events: &[LoreEvent]) -> Option<u64> {
    events.iter().find_map(|event| match event {
        LoreEvent::RevisionTreeLoaded(data) => Some(data.handle_id),
        _ => None,
    })
}

/// The batch id and error code of every `RevisionTreeBatchComplete` event in `events`, in order.
pub(crate) fn batch_outcomes(events: &[LoreEvent]) -> Vec<(u64, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeBatchComplete(data) => Some((data.batch_id, data.error_code)),
            _ => None,
        })
        .collect()
}

/// The first `RevisionTreeNodeInfo` event in `events`.
pub(crate) fn node_info_event(events: &[LoreEvent]) -> Option<&LoreRevisionTreeNodeInfoEventData> {
    events.iter().find_map(|event| match event {
        LoreEvent::RevisionTreeNodeInfo(data) => Some(data),
        _ => None,
    })
}

/// The node id and error code of the first `RevisionTreeAddComplete` event for entry `id`.
pub(crate) fn add_outcome(events: &[LoreEvent], id: u64) -> Option<(NodeID, LoreErrorCode)> {
    events.iter().find_map(|event| match event {
        LoreEvent::RevisionTreeAddComplete(data) if data.entry_id == id => {
            Some((data.node_id, data.error_code))
        }
        _ => None,
    })
}

/// The node id of every `RevisionTreeAddComplete` event in `events` that reports no error, in
/// order.
pub(crate) fn added_nodes(events: &[LoreEvent]) -> Vec<NodeID> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeAddComplete(data) if data.error_code == LoreErrorCode::None => {
                Some(data.node_id)
            }
            _ => None,
        })
        .collect()
}

/// The handle of a revision tree loaded at `revision` of `repository` on the storage handle
/// `store`. Panics unless the load succeeds.
pub(crate) async fn load_tree_at(
    store: LoreStore,
    repository: Partition,
    revision: Hash,
) -> LoreRevisionTree {
    let (events, callback) = recording_callback();
    let status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store,
            repository,
            revision_hash: revision,
        },
        callback,
    )
    .await;
    assert_eq!(status, 0, "loading the revision tree must succeed");
    let handle_id =
        loaded_handle_id(&events.lock().unwrap()).expect("the load must emit RevisionTreeLoaded");
    LoreRevisionTree { handle_id }
}

/// [`load_tree_at`] the zero revision.
pub(crate) async fn load_tree(store: LoreStore, repository: Partition) -> LoreRevisionTree {
    load_tree_at(store, repository, Hash::default()).await
}

/// Loads a revision tree on a new in-memory store named `label`, and returns the handle with the
/// store's handle id.
pub(crate) async fn load_handle(label: &str, repository: Partition) -> (LoreRevisionTree, u64) {
    let store_handle = storage_handle::register(in_memory_for_tests(label).await);
    (
        load_tree(store_handle, repository).await,
        store_handle.handle_id,
    )
}

/// Unregisters the revision tree `handle` and the store handle `store_handle_id`.
pub(crate) fn release(handle: LoreRevisionTree, store_handle_id: u64) {
    rt_handle::unregister(handle);
    storage_handle::unregister(LoreStore {
        handle_id: store_handle_id,
    });
}

/// The state and repository context behind the revision tree `handle`.
pub(crate) fn handle_state(handle: LoreRevisionTree) -> (Arc<State>, Arc<RepositoryContext>) {
    let entry = rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered");
    (entry.state_for_tests(), entry.repository_context.clone())
}

/// Commit-free stand-in for a node the loaded revision holds: clears the staging flags an add
/// left on `node_ids`, which is what commit does to everything it writes.
pub(crate) async fn settle(handle: LoreRevisionTree, node_ids: &[NodeID]) {
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    for node_id in node_ids {
        let block_index = NodeBlock::index(*node_id);
        let block = internal
            .state_for_tests()
            .block(internal.repository_context.clone(), block_index)
            .await
            .expect("the block must be readable");
        let mut writer = block.write_for_tests();
        writer.node(Node::index(*node_id)).clear_all_change_flags();
    }
}

/// Every child the listing reports for `parent`, as `(node_id, staged_action)`.
pub(crate) async fn children_of(handle: LoreRevisionTree, parent: NodeID) -> Vec<(NodeID, u32)> {
    let (events, callback) = recording_callback();
    let status = list_children(
        LoreGlobalArgs::default(),
        LoreRevisionTreeListChildrenArgs {
            id: 5,
            handle,
            parent_node_id: parent,
        },
        callback,
    )
    .await;
    assert_eq!(status, 0, "listing children must succeed");
    take_recorded(&events)
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeChild(data) => Some((data.node_id, data.staged_action)),
            _ => None,
        })
        .collect()
}

/// The record `node_info` reports for `node_id`.
pub(crate) async fn fetch_node_info(
    handle: LoreRevisionTree,
    node_id: NodeID,
) -> LoreRevisionTreeNodeInfoEventData {
    let (events, callback) = recording_callback();
    node_info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodeInfoArgs {
            id: 7,
            handle,
            node_id,
        },
        callback,
    )
    .await;
    node_info_event(&take_recorded(&events))
        .expect("node_info must emit a record")
        .clone()
}

/// Asserts that `events` holds the entries' terminals, then the batch terminal, then `Complete`,
/// in that order. `is_entry` picks the entries' terminal events.
pub(crate) fn assert_entries_then_batch_then_complete(
    events: &Recorded,
    is_entry: fn(&LoreEvent) -> bool,
) {
    let batch_at = events
        .iter()
        .position(|event| matches!(event, LoreEvent::RevisionTreeBatchComplete(_)))
        .expect("the batch terminal must fire");
    let complete_at = events
        .iter()
        .position(|event| matches!(event, LoreEvent::Complete(_)))
        .expect("Complete must fire");
    let last_entry_at = events
        .iter()
        .rposition(is_entry)
        .expect("the entries must report");
    assert!(
        last_entry_at < batch_at && batch_at < complete_at,
        "order must be entries, then the batch terminal, then Complete: {events:?}"
    );
}
