// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_node_path` — reconstruct the full UTF-8 path for a
//! `NodeID` by walking parent pointers. Iteration costs scale with depth;
//! per-child listings deliberately skip this work to keep their memory flat.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeNodePathEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_node_path`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(node_path_impl)]
pub struct LoreRevisionTreeNodePathArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to read from
    pub handle: LoreRevisionTree,
    /// Node whose full UTF-8 path is reconstructed by walking parents
    pub node_id: NodeID,
}

#[error_set]
enum NodePathError {
    InvalidArguments,
}

impl EventError for NodePathError {
    fn translated(&self) -> LoreError {
        match self {
            NodePathError::InvalidArguments(_) => LoreError::InvalidArguments,
            NodePathError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn invalid(reason: &str) -> NodePathError {
    NodePathError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// Emit the id-carrying terminal for a failed `node_path`: an empty path plus
/// the populated `error_code`.
fn emit_node_path_error(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeNodePath(LoreRevisionTreeNodePathEventData {
        id,
        error_code,
        ..Default::default()
    })
    .send();
}

/// Reconstruct the full UTF-8 path for a node id by walking parent pointers.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_NODE_PATH` carrying
/// the path from the root to the node plus the `(repository, revision)` it was
/// reconstructed in (the handle's own — `node_path` does not follow links), and
/// `error_code = NONE`, before `Complete {status: 0}`. The root resolves to the
/// empty path; every non-root node has a non-empty name, so a node id that
/// resolves to an empty path (e.g. an unallocated slot) is rejected with
/// `error_code = INVALID_ARGUMENTS` rather than returning a bogus empty path. An
/// invalid or unknown node id likewise completes with
/// `error_code = INVALID_ARGUMENTS`. The verb materializes no bytes to disk.
pub async fn node_path(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, node_path_impl).await
}

async fn node_path_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        node_path,
        move || {
            emit_node_path_error(miss_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeNodePathArgs| {
            let id = args.id;
            let node_id = args.node_id;

            if !node_id.is_valid_or_root_node_id() {
                emit_node_path_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is invalid"));
            }

            // An unresolvable id is a bad argument; `State::node_path` does not
            // distinguish an out-of-range id from a read failure, so both collapse to
            // `InvalidArguments` (as in `list_children`).
            let Ok(path) = internal
                .state
                .node_path(internal.repository_context.clone(), node_id)
                .await
            else {
                emit_node_path_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is unknown"));
            };

            // Every non-root node has a non-empty name, so an empty path means the
            // id landed on an unallocated slot rather than a real node.
            if node_id != ROOT_NODE && path.is_empty() {
                emit_node_path_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id does not resolve to a named node"));
            }

            LoreEvent::RevisionTreeNodePath(LoreRevisionTreeNodePathEventData {
                id,
                repository: internal.repository,
                revision: internal.state.revision(),
                path: LoreString::from(path.as_str()),
                error_code: LoreErrorCode::None,
            })
            .send();
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;

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
        NodePath(Box<LoreRevisionTreeNodePathEventData>),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeNodePath(data) => Self::NodePath(Box::new(data.clone())),
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn node_path_event(events: &[CapturedEvent]) -> Option<LoreRevisionTreeNodePathEventData> {
        events.iter().find_map(|event| match event {
            CapturedEvent::NodePath(data) => Some((**data).clone()),
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
        (entry.state.clone(), entry.repository_context.clone())
    }

    /// Add a node under `parent` and return its id. `is_file` chooses a file vs
    /// directory; both can themselves parent further children.
    async fn add_under(
        handle: LoreRevisionTree,
        parent: NodeID,
        name: &str,
        is_file: bool,
    ) -> NodeID {
        let (state, repository) = handle_state(handle);
        let flags = if is_file { NodeFlags::File.bits() } else { 0 };
        let node = Node {
            flags,
            ..Default::default()
        };
        state
            .node_add(repository, parent, node, name)
            .await
            .expect("node_add must succeed")
    }

    fn release(handle: LoreRevisionTree, store_handle_id: u64) {
        rt_handle::unregister(handle);
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn node_path_walks_parents_to_root() {
        let partition = Partition::from([0x11u8; 16]);
        let (handle, store_handle_id) = load_handle("np-walk", partition).await;
        let dir_id = add_under(handle, ROOT_NODE, "a", false).await;
        let file_id = add_under(handle, dir_id, "b", true).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 1,
                handle,
                node_id: file_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events).expect("node path event must fire");
        assert_eq!(data.id, 1);
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(data.repository, partition, "got {events:?}");
        assert_eq!(
            data.revision,
            Hash::default(),
            "the path is reconstructed in the handle's loaded revision, got {events:?}"
        );
        assert_eq!(
            data.path.as_str(),
            "a/b",
            "the path walks parents from the root, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(0)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_path_single_component_under_root() {
        let (handle, store_handle_id) =
            load_handle("np-single", Partition::from([0x66u8; 16])).await;
        let file_id = add_under(handle, ROOT_NODE, "file", true).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 7,
                handle,
                node_id: file_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events).expect("node path event must fire");
        assert_eq!(
            data.path.as_str(),
            "file",
            "a child of the root has a single-component path, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_path_deep_path_walks_all_levels() {
        let (handle, store_handle_id) = load_handle("np-deep", Partition::from([0x77u8; 16])).await;
        let a = add_under(handle, ROOT_NODE, "a", false).await;
        let b = add_under(handle, a, "b", false).await;
        let c = add_under(handle, b, "c", true).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 8,
                handle,
                node_id: c,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events).expect("node path event must fire");
        assert_eq!(
            data.path.as_str(),
            "a/b/c",
            "the path walks every level in order, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_path_root_returns_empty() {
        let (handle, store_handle_id) = load_handle("np-root", Partition::from([0x22u8; 16])).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 2,
                handle,
                node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events).expect("node path event must fire");
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(
            data.path.as_str(),
            "",
            "the root resolves to the empty path, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_path_unknown_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("np-unknown", Partition::from([0x33u8; 16])).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 3,
                handle,
                node_id: INVALID_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "an invalid node id must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events)
            .expect("a failure must still emit the node path terminal carrying the id");
        assert_eq!(data.id, 3);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_path_nonexistent_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("np-nonexistent", Partition::from([0x44u8; 16])).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 5,
                handle,
                node_id: 1_000_000,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "a node id past any allocated block must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events)
            .expect("a failure must still emit the node path terminal carrying the id");
        assert_eq!(data.id, 5);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "an unknown node id must report InvalidArguments, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_path_unallocated_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("np-unallocated", Partition::from([0x55u8; 16])).await;

        // No nodes added: id 1 is an in-range but unallocated slot, which would
        // otherwise reconstruct to an empty (root-looking) path.
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 6,
                handle,
                node_id: 1,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "an unallocated node id must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events)
            .expect("a failure must still emit the node path terminal carrying the id");
        assert_eq!(data.id, 6);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "a non-root node resolving to an empty path must report InvalidArguments, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_path_on_unknown_handle_emits_terminal_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = node_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodePathArgs {
                id: 4,
                handle: LoreRevisionTree::INVALID,
                node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_path_event(&events)
            .expect("a handle miss must still emit the node path terminal carrying the id");
        assert_eq!(data.id, 4);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }
}
