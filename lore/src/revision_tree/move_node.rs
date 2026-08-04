// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_move` — reparent and/or rename a node while
//! preserving its `file_id`, so the resulting revision graph records a
//! true move instead of a delete-plus-add pair. The Rust module is named
//! `move_node` because `move` is a reserved keyword; the C symbol stays
//! `lore_revision_tree_move`.

#[cfg(test)]
use std::sync::Arc;

use lore_macro::LoreArgs;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeMoveCompleteEventData;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
#[cfg(test)]
use lore_revision::node::Node;
#[cfg(test)]
use lore_revision::node::NodeBlock;
#[cfg(test)]
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
#[cfg(test)]
use lore_revision::node::ROOT_NODE;
#[cfg(test)]
use lore_revision::repository::RepositoryContext;
#[cfg(test)]
use lore_revision::state::State;
#[cfg(test)]
use lore_storage::hash::hash_string;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_move`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(move_impl)]
pub struct LoreRevisionTreeMoveArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Node to move; its `file_id` is preserved across the move
    pub node_id: NodeID,
    /// Parent node the moved node is reparented under
    pub destination_parent_id: NodeID,
    /// UTF-8 name the moved node takes at the destination
    pub dst_name: LoreString,
}

fn emit_move_complete(id: u64, node_id: NodeID, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeMoveComplete(LoreRevisionTreeMoveCompleteEventData {
        id,
        node_id,
        error_code,
    })
    .send();
}

/// Move a node between parents with optional rename (or rename within one).
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_MOVE_COMPLETE`
/// echoing the moved node id with `error_code = NONE`, before
/// `Complete {status: 0}`. The node's `file_id` (the `address.context` slot)
/// travels with it, so the revision graph records a true move instead of a
/// delete-plus-add pair. The root cannot be moved, the destination must be a
/// directory in the handle's own tree, a name that already exists at the
/// destination is rejected at call time, and a directory cannot be moved into
/// its own subtree — programmatic edits could otherwise construct a cycle no
/// filesystem would permit.
pub async fn move_node(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, move_impl).await
}

async fn move_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        move_node,
        move |_| {
            emit_move_complete(miss_id, INVALID_NODE, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeMoveArgs| {
            let id = args.id;
            match lore_revision::revision_tree::move_node(
                internal.state(),
                internal.repository_context.clone(),
                args.node_id,
                args.destination_parent_id,
                args.dst_name.as_bytes(),
            )
            .await
            {
                Ok(node_id) => {
                    emit_move_complete(id, node_id, LoreErrorCode::None);
                    Ok(())
                }
                Err(error) => {
                    let error_code = if error.is_invalid_arguments() {
                        LoreErrorCode::InvalidArguments
                    } else {
                        LoreErrorCode::Internal
                    };
                    emit_move_complete(id, INVALID_NODE, error_code);
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
        MoveComplete(u64, NodeID, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeMoveComplete(data) => {
                    Self::MoveComplete(data.id, data.node_id, data.error_code)
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

    fn move_outcome(events: &[CapturedEvent], id: u64) -> Option<(NodeID, LoreErrorCode)> {
        events.iter().find_map(|event| match event {
            CapturedEvent::MoveComplete(event_id, node_id, error_code) if *event_id == id => {
                Some((*node_id, *error_code))
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

    async fn run_move(
        handle: LoreRevisionTree,
        id: u64,
        node_id: NodeID,
        destination_parent_id: NodeID,
        dst_name: &str,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = move_node(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMoveArgs {
                id,
                handle,
                node_id,
                destination_parent_id,
                dst_name: LoreString::from_str(dst_name),
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
    async fn move_reparents_a_file_and_preserves_its_file_id() {
        let (handle, store_handle_id) =
            load_handle("move-file", Partition::from([0x11u8; 16])).await;
        let dir_id = add_node(handle, ROOT_NODE, "docs", false).await;
        let file_id = add_node(handle, ROOT_NODE, "doc.md", true).await;

        let (state, repository_context) = handle_state(handle);
        let file_id_before = state
            .node(repository_context.clone(), file_id)
            .await
            .expect("node must be readable")
            .address
            .context;

        let (status, events) = run_move(handle, 1, file_id, dir_id, "renamed.md").await;

        assert_eq!(status, 0, "moving a file must succeed, got {events:?}");
        let (echoed, error_code) = move_outcome(&events, 1).expect("MoveComplete must fire");
        assert_eq!(error_code, LoreErrorCode::None);
        assert_eq!(echoed, file_id, "the moved node id must be echoed");
        assert!(events.contains(&CapturedEvent::Complete(0)));

        let node = state
            .node(repository_context.clone(), file_id)
            .await
            .expect("moved node must be readable");
        assert_eq!(node.parent, dir_id, "the node must reparent");
        assert_eq!(
            node.address.context, file_id_before,
            "the file id must be preserved across the move"
        );
        let name = state
            .node_name_clone(repository_context.clone(), file_id)
            .await
            .expect("moved node must be named");
        assert_eq!(name, "renamed.md");
        assert_eq!(
            state
                .find_subnode(
                    repository_context.clone(),
                    dir_id,
                    hash_string("renamed.md")
                )
                .await
                .expect("the destination must resolve the new name"),
            file_id,
        );
        assert!(
            state
                .find_subnode(repository_context, ROOT_NODE, hash_string("doc.md"))
                .await
                .is_err(),
            "the old name must no longer resolve under the old parent"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn rename_within_the_same_parent_succeeds() {
        let (handle, store_handle_id) =
            load_handle("move-rename", Partition::from([0x22u8; 16])).await;
        let file_id = add_node(handle, ROOT_NODE, "doc.md", true).await;

        let (status, events) = run_move(handle, 2, file_id, ROOT_NODE, "renamed.md").await;

        assert_eq!(status, 0, "a rename must succeed, got {events:?}");
        let (state, repository_context) = handle_state(handle);
        let name = state
            .node_name_clone(repository_context, file_id)
            .await
            .expect("renamed node must be named");
        assert_eq!(name, "renamed.md");

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn move_into_own_subtree_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("move-cycle", Partition::from([0x33u8; 16])).await;
        let outer = add_node(handle, ROOT_NODE, "outer", false).await;
        let inner = add_node(handle, outer, "inner", false).await;

        let (status, events) = run_move(handle, 3, outer, inner, "outer").await;
        assert_eq!(status, 1, "a move into the moved subtree must fail");
        let (_, error_code) = move_outcome(&events, 3).expect("MoveComplete must fire");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );

        let (status, events) = run_move(handle, 4, outer, outer, "self").await;
        assert_eq!(status, 1, "a move into itself must fail, got {events:?}");

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn move_to_an_occupied_name_or_itself_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("move-occupied", Partition::from([0x44u8; 16])).await;
        let dir_id = add_node(handle, ROOT_NODE, "docs", false).await;
        let file_id = add_node(handle, ROOT_NODE, "doc.md", true).await;
        add_node(handle, dir_id, "taken.md", true).await;

        let (status, events) = run_move(handle, 5, file_id, dir_id, "taken.md").await;
        assert_eq!(status, 1, "an occupied destination name must fail");
        let (_, error_code) = move_outcome(&events, 5).expect("MoveComplete must fire");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );

        let (status, events) = run_move(handle, 6, file_id, ROOT_NODE, "doc.md").await;
        assert_eq!(status, 1, "a no-op move must fail, got {events:?}");

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn move_marks_the_subtree_staged_move() {
        let (handle, store_handle_id) =
            load_handle("move-marks", Partition::from([0x55u8; 16])).await;
        let dir_id = add_node(handle, ROOT_NODE, "docs", false).await;
        let child = add_node(handle, dir_id, "a.md", true).await;
        let dst_id = add_node(handle, ROOT_NODE, "dst", false).await;

        // Clear the staged bits the fixture adds left behind so the test
        // observes the move's own marking.
        let (state, repository_context) = handle_state(handle);
        for node_id in [dir_id, child, dst_id] {
            let block = state
                .block(repository_context.clone(), NodeBlock::index(node_id))
                .await
                .expect("block must load");
            let mut writer = block.write();
            writer.node(Node::index(node_id)).clear_all_change_flags();
        }

        let (status, events) = run_move(handle, 7, dir_id, dst_id, "docs").await;
        assert_eq!(status, 0, "moving a directory must succeed, got {events:?}");

        let moved = state
            .node(repository_context.clone(), dir_id)
            .await
            .expect("moved directory must be readable");
        assert!(
            moved.is_staged_move(),
            "the moved directory must be marked StagedMove, flags {:x}",
            moved.flags
        );
        let moved_child = state
            .node(repository_context, child)
            .await
            .expect("moved child must be readable");
        assert!(
            moved_child.is_staged_move(),
            "children of a moved directory must be marked StagedMove, flags {:x}",
            moved_child.flags
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn move_rejects_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("move-invalid", Partition::from([0x66u8; 16])).await;
        let file_id = add_node(handle, ROOT_NODE, "doc.md", true).await;
        let other_file = add_node(handle, ROOT_NODE, "other.md", true).await;

        // The root cannot be moved.
        let (status, _) = run_move(handle, 8, ROOT_NODE, ROOT_NODE, "root").await;
        assert_eq!(status, 1);
        // Unknown node.
        let (status, _) = run_move(handle, 9, 1_000_000, ROOT_NODE, "doc.md").await;
        assert_eq!(status, 1);
        // A file cannot be the destination parent.
        let (status, _) = run_move(handle, 10, file_id, other_file, "doc.md").await;
        assert_eq!(status, 1);
        // Names with separators are rejected.
        let (status, _) = run_move(handle, 11, file_id, ROOT_NODE, "a/b").await;
        assert_eq!(status, 1);
        // Empty names are rejected.
        let (status, _) = run_move(handle, 12, file_id, ROOT_NODE, "").await;
        assert_eq!(status, 1);

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn move_on_unknown_handle_emits_move_complete_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = move_node(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMoveArgs {
                id: 13,
                handle: LoreRevisionTree::INVALID,
                node_id: 1,
                destination_parent_id: ROOT_NODE,
                dst_name: LoreString::from_str("doc.md"),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "moving against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, error_code) = move_outcome(&events, 13)
            .expect("a handle miss must still emit MoveComplete carrying the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert_eq!(node_id, INVALID_NODE);
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }
}
