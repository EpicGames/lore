// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_delete` — mark a node and its transitive children as
//! deleted within the handle's in-progress revision. Subsequent reads in the
//! same handle do not observe the deleted subtree.

#[cfg(test)]
use std::sync::Arc;

use lore_macro::LoreArgs;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeDeleteCompleteEventData;
#[cfg(test)]
use lore_revision::node::NodeDelta;
#[cfg(test)]
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
#[cfg(test)]
use lore_revision::repository::RepositoryContext;
#[cfg(test)]
use lore_revision::state::State;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_delete`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(delete_impl)]
pub struct LoreRevisionTreeDeleteArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Subtree root to mark deleted, including its transitive children
    pub node_id: NodeID,
}

fn emit_delete_complete(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeDeleteComplete(LoreRevisionTreeDeleteCompleteEventData {
        id,
        error_code,
    })
    .send();
}

/// Mark a node and its transitive children as deleted.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_DELETE_COMPLETE`
/// with `error_code = NONE`, before `Complete {status: 0}`. The subtree is
/// removed from the handle's in-memory state immediately: subsequent reads on
/// the same handle do not observe it, and its delta entries are carried on
/// the handle until `commit` folds them into the new revision's delta block.
/// The root cannot be deleted. A link node deletes as a single node — its
/// subtree lives in the link target's tree, which this handle cannot mutate.
pub async fn delete(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeDeleteArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, delete_impl).await
}

async fn delete_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeDeleteArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        delete,
        move |_| {
            emit_delete_complete(miss_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeDeleteArgs| {
            let id = args.id;
            match lore_revision::revision_tree::delete_node(
                internal.state(),
                internal.repository_context.clone(),
                args.node_id,
            )
            .await
            {
                Ok(delta) => {
                    internal.pending_delta.write().extend(delta);
                    emit_delete_complete(id, LoreErrorCode::None);
                    Ok(())
                }
                Err(error) => {
                    let error_code = if error.is_invalid_arguments() {
                        LoreErrorCode::InvalidArguments
                    } else {
                        LoreErrorCode::Internal
                    };
                    emit_delete_complete(id, error_code);
                    Err(error)
                }
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::change::FileAction;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::Node;
    use lore_revision::node::ROOT_NODE;
    use lore_storage::hash::hash_string;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error(u32),
        Complete(i32),
        RevisionTreeLoaded(u64),
        DeleteComplete(u64, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeDeleteComplete(data) => {
                    Self::DeleteComplete(data.id, data.error_code)
                }
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn delete_outcome(events: &[CapturedEvent], id: u64) -> Option<LoreErrorCode> {
        events.iter().find_map(|event| match event {
            CapturedEvent::DeleteComplete(event_id, error_code) if *event_id == id => {
                Some(*error_code)
            }
            _ => None,
        })
    }

    async fn load_handle(label: &str, repository: Partition) -> (LoreRevisionTree, u64) {
        let store = in_memory_for_tests(label).await;
        let store_handle = storage_handle::register(store);
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository,
                revision_hash: Hash::default(),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "load fixture must succeed");
        let id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                CapturedEvent::RevisionTreeLoaded(id) => Some(*id),
                _ => None,
            })
            .expect("load fixture must emit RevisionTreeLoaded");
        (LoreRevisionTree { handle_id: id }, store_handle.handle_id)
    }

    fn handle_state(handle: LoreRevisionTree) -> (Arc<State>, Arc<RepositoryContext>) {
        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        (entry.state(), entry.repository_context.clone())
    }

    fn pending_delta(handle: LoreRevisionTree) -> Vec<NodeDelta> {
        rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered")
            .pending_delta
            .read()
            .clone()
    }

    async fn add_node(
        handle: LoreRevisionTree,
        parent: NodeID,
        name: &str,
        is_file: bool,
    ) -> NodeID {
        let (state, repository_context) = handle_state(handle);
        let node = Node {
            flags: if is_file { NodeFlags::File.bits() } else { 0 },
            name_hash: hash_string(name),
            address: Address {
                hash: Hash::from([0x42u8; 32]),
                context: Context::from([0x11u8; 16]),
            },
            ..Default::default()
        };
        state
            .node_add(repository_context, parent, node, name)
            .await
            .expect("node_add must succeed")
    }

    async fn run_delete(
        handle: LoreRevisionTree,
        id: u64,
        node_id: NodeID,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = delete(
            LoreGlobalArgs::default(),
            LoreRevisionTreeDeleteArgs {
                id,
                handle,
                node_id,
            },
            make_callback(sink.clone()),
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    fn release(handle: LoreRevisionTree, store_handle_id: u64) {
        rt_handle::unregister(handle);
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn delete_file_removes_it_from_reads_and_records_a_delete_delta() {
        let (handle, store_handle_id) =
            load_handle("delete-file", Partition::from([0x11u8; 16])).await;
        let node_id = add_node(handle, ROOT_NODE, "doc.md", true).await;

        let (status, events) = run_delete(handle, 1, node_id).await;

        assert_eq!(status, 0, "deleting a file must succeed, got {events:?}");
        assert_eq!(delete_outcome(&events, 1), Some(LoreErrorCode::None));
        assert!(events.contains(&CapturedEvent::Complete(0)));

        let (state, repository_context) = handle_state(handle);
        let result = state
            .find_subnode(repository_context.clone(), ROOT_NODE, hash_string("doc.md"))
            .await;
        assert!(
            result.is_err(),
            "a deleted node must not be observable by reads"
        );

        let deltas = pending_delta(handle);
        assert_eq!(deltas.len(), 1, "one delta entry per deleted node");
        assert_eq!(deltas[0].node, node_id);
        assert_eq!(deltas[0].action, FileAction::Delete as u16);

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn delete_directory_removes_the_subtree_recursively() {
        let (handle, store_handle_id) =
            load_handle("delete-dir", Partition::from([0x22u8; 16])).await;
        let dir_id = add_node(handle, ROOT_NODE, "docs", false).await;
        let child_a = add_node(handle, dir_id, "a.md", true).await;
        let child_b = add_node(handle, dir_id, "b.md", true).await;
        let keeper = add_node(handle, ROOT_NODE, "keep.md", true).await;

        let (status, events) = run_delete(handle, 2, dir_id).await;

        assert_eq!(
            status, 0,
            "deleting a directory must succeed, got {events:?}"
        );
        assert_eq!(delete_outcome(&events, 2), Some(LoreErrorCode::None));

        let (state, repository_context) = handle_state(handle);
        assert!(
            state
                .find_subnode(repository_context.clone(), ROOT_NODE, hash_string("docs"))
                .await
                .is_err(),
            "the deleted directory must not be observable"
        );
        assert_eq!(
            state
                .find_subnode(
                    repository_context.clone(),
                    ROOT_NODE,
                    hash_string("keep.md")
                )
                .await
                .expect("the sibling must survive"),
            keeper,
        );

        let deltas = pending_delta(handle);
        let deleted: Vec<NodeID> = deltas.iter().map(|delta| delta.node).collect();
        assert_eq!(deltas.len(), 3, "one delta entry per deleted node");
        for expected in [dir_id, child_a, child_b] {
            assert!(
                deleted.contains(&expected),
                "delta must record node {expected}, got {deleted:?}"
            );
        }
        assert!(
            deltas
                .iter()
                .all(|delta| delta.action == FileAction::Delete as u16),
            "every recorded entry must carry the delete action"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn delete_marks_the_parent_chain_staged_for_the_commit_rehash() {
        let (handle, store_handle_id) =
            load_handle("delete-parent-staged", Partition::from([0x33u8; 16])).await;
        let dir_id = add_node(handle, ROOT_NODE, "docs", false).await;
        let child = add_node(handle, dir_id, "a.md", true).await;

        // Clear the staged bits the fixture adds left behind so the test
        // observes the delete's own propagation.
        let (state, repository_context) = handle_state(handle);
        for node_id in [dir_id, child] {
            let block = state
                .block(
                    repository_context.clone(),
                    lore_revision::node::NodeBlock::index(node_id),
                )
                .await
                .expect("block must load");
            let mut writer = block.write();
            writer.node(Node::index(node_id)).clear_all_change_flags();
        }

        let (status, _events) = run_delete(handle, 3, child).await;
        assert_eq!(status, 0);

        let dir_node = state
            .node(repository_context, dir_id)
            .await
            .expect("parent must remain readable");
        assert!(
            dir_node.is_staged(),
            "the parent directory must be staged so commit rehashes it, flags {:x}",
            dir_node.flags
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn delete_root_or_invalid_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("delete-invalid", Partition::from([0x44u8; 16])).await;

        for (id, node_id) in [(4u64, ROOT_NODE), (5u64, INVALID_NODE), (6u64, 1_000_000)] {
            let (status, events) = run_delete(handle, id, node_id).await;
            assert_eq!(status, 1, "node id {node_id} must fail");
            assert_eq!(
                delete_outcome(&events, id),
                Some(LoreErrorCode::InvalidArguments),
                "got {events:?}"
            );
        }
        assert!(
            pending_delta(handle).is_empty(),
            "failed deletes must not record delta entries"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn delete_twice_fails_the_second_time() {
        let (handle, store_handle_id) =
            load_handle("delete-twice", Partition::from([0x55u8; 16])).await;
        let node_id = add_node(handle, ROOT_NODE, "doc.md", true).await;

        let (status, _events) = run_delete(handle, 7, node_id).await;
        assert_eq!(status, 0, "first delete must succeed");

        let (status, events) = run_delete(handle, 8, node_id).await;
        assert_eq!(status, 1, "second delete of the same node must fail");
        assert_eq!(
            delete_outcome(&events, 8),
            Some(LoreErrorCode::InvalidArguments),
            "got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn delete_on_unknown_handle_emits_delete_complete_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = delete(
            LoreGlobalArgs::default(),
            LoreRevisionTreeDeleteArgs {
                id: 9,
                handle: LoreRevisionTree::INVALID,
                node_id: 1,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "deleting against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        assert_eq!(
            delete_outcome(&events, 9),
            Some(LoreErrorCode::InvalidArguments),
            "a handle miss must still emit DeleteComplete carrying the caller id, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }
}
