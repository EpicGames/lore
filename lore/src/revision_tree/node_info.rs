// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_node_info` — fetch the per-node record for a single
//! `NodeID`. The record is uniform across every node, including the root;
//! revision-level metadata is served separately by `lore_revision_tree_info`.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
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

/// Arguments for `lore_revision_tree_node_info`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(node_info_impl)]
pub struct LoreRevisionTreeNodeInfoArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to read from
    pub handle: LoreRevisionTree,
    /// Node whose record is fetched
    pub node_id: NodeID,
}

#[error_set]
enum NodeInfoError {
    InvalidArguments,
}

impl EventError for NodeInfoError {
    fn translated(&self) -> LoreError {
        match self {
            NodeInfoError::InvalidArguments(_) => LoreError::InvalidArguments,
            NodeInfoError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn invalid(reason: &str) -> NodeInfoError {
    NodeInfoError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// Emit the id-carrying terminal for a failed `node_info`: a record with a
/// zeroed body and the populated `error_code`.
fn emit_node_info_error(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeNodeInfo(LoreRevisionTreeNodeInfoEventData {
        id,
        node_id: INVALID_NODE,
        parent_id: INVALID_NODE,
        error_code,
        ..Default::default()
    })
    .send();
}

/// Fetch the per-node record for a single node id.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_NODE_INFO` carrying
/// the node's name, kind, mode, size, address, preserved `file_id`, the
/// `(repository, revision)` it belongs to (the handle's own — `node_info` does
/// not follow links, so a link id reports the link node itself), and
/// `error_code = NONE`, before `Complete {status: 0}`. The record is uniform
/// across every node, including the root (which reports an empty name without a
/// name-table read); revision-level metadata is served by
/// `lore_revision_tree_info`, not here. An invalid or unknown node id completes
/// with `error_code = INVALID_ARGUMENTS`; a name-table read failure on a
/// non-root node completes with `error_code = INTERNAL`. The verb materializes
/// no bytes to disk.
///
/// Node ids are opaque values issued by the API. An id the API never issued
/// that happens to land on an unallocated slot of an existing block reads back
/// as a zeroed record with an empty name; since every non-root node has a
/// non-empty name, such an id is rejected with `INVALID_ARGUMENTS` rather than
/// reported as a bogus record (consistent with `node_path`).
pub async fn node_info(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, node_info_impl).await
}

async fn node_info_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        node_info,
        move || {
            emit_node_info_error(miss_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeNodeInfoArgs| {
            let id = args.id;
            let node_id = args.node_id;

            if !node_id.is_valid_or_root_node_id() {
                emit_node_info_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is invalid"));
            }

            let Ok(node) = internal
                .state
                .node(internal.repository_context.clone(), node_id)
                .await
            else {
                emit_node_info_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is unknown"));
            };

            let name = if node_id == ROOT_NODE {
                String::new()
            } else {
                match internal
                    .state
                    .node_name_clone(internal.repository_context.clone(), node_id)
                    .await
                {
                    Ok(name) => name,
                    Err(error) => {
                        emit_node_info_error(id, LoreErrorCode::Internal);
                        return Err(NodeInfoError::internal_with_context(
                            error,
                            "State::node_name_clone",
                        ));
                    }
                }
            };

            // Every non-root node has a non-empty name, so an empty name means the
            // id landed on an unallocated slot rather than a real node.
            if node_id != ROOT_NODE && name.is_empty() {
                emit_node_info_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id does not resolve to a named node"));
            }

            let kind = if node.is_file() {
                LoreNodeType::File as u32
            } else if node.is_link() {
                LoreNodeType::Link as u32
            } else {
                LoreNodeType::Directory as u32
            };

            LoreEvent::RevisionTreeNodeInfo(LoreRevisionTreeNodeInfoEventData {
                id,
                node_id,
                repository: internal.repository,
                revision: internal.state.revision(),
                name: LoreString::from(name.as_str()),
                parent_id: node.parent,
                kind,
                mode: node.mode,
                size: node.size,
                address: node.address,
                file_id: node.address.context,
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

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::interface::LoreNodeType;
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
        NodeInfo(Box<LoreRevisionTreeNodeInfoEventData>),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeNodeInfo(data) => Self::NodeInfo(Box::new(data.clone())),
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn node_info_event(events: &[CapturedEvent]) -> Option<LoreRevisionTreeNodeInfoEventData> {
        events.iter().find_map(|event| match event {
            CapturedEvent::NodeInfo(data) => Some((**data).clone()),
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

    /// Add a file under root with explicit metadata so the record fields can be
    /// verified. Returns the new node id.
    async fn add_file(
        handle: LoreRevisionTree,
        name: &str,
        mode: u16,
        size: u64,
        address: Address,
    ) -> NodeID {
        let (state, repository) = handle_state(handle);
        let node = Node {
            flags: NodeFlags::File.bits(),
            mode,
            size,
            address,
            ..Default::default()
        };
        state
            .node_add(repository, ROOT_NODE, node, name)
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
    async fn node_info_returns_full_record_for_internal_node() {
        let partition = Partition::from([0x11u8; 16]);
        let (handle, store_handle_id) = load_handle("ni-internal", partition).await;
        let address = Address {
            hash: Hash::from([0x42u8; 32]),
            context: Context::from([0x99u8; 16]),
        };
        let node_id = add_file(handle, "doc.md", 0o644, 1234, address).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 1,
                handle,
                node_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = node_info_event(&events).expect("node info event must fire");
        assert_eq!(data.id, 1);
        assert_eq!(data.node_id, node_id);
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(data.repository, partition, "got {events:?}");
        assert_eq!(
            data.revision,
            Hash::default(),
            "an uncommitted handle reports its loaded revision, got {events:?}"
        );
        assert_eq!(data.name.as_str(), "doc.md");
        assert_eq!(data.parent_id, ROOT_NODE);
        assert_eq!(data.kind, LoreNodeType::File as u32);
        assert_eq!(data.mode, 0o644, "got {events:?}");
        assert_eq!(data.size, 1234, "got {events:?}");
        assert_eq!(data.address, address, "got {events:?}");
        assert_eq!(
            data.file_id,
            Context::from([0x99u8; 16]),
            "file_id is the node's address context, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(0)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_info_for_root_returns_a_uniform_directory_record() {
        let partition = Partition::from([0x22u8; 16]);
        let (handle, store_handle_id) = load_handle("ni-root", partition).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 2,
                handle,
                node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = node_info_event(&events).expect("node info event must fire");
        assert_eq!(data.id, 2);
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(data.node_id, ROOT_NODE);
        assert_eq!(data.repository, partition, "got {events:?}");
        assert_eq!(
            data.kind,
            LoreNodeType::Directory as u32,
            "the root is a directory, got {events:?}"
        );
        assert_eq!(
            data.name.as_str(),
            "",
            "the root reports an empty name, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(0)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_info_returns_a_directory_record_for_a_subdirectory() {
        let partition = Partition::from([0x77u8; 16]);
        let (handle, store_handle_id) = load_handle("ni-dir", partition).await;
        let dir_id = {
            let (state, repository) = handle_state(handle);
            let node = Node {
                flags: 0,
                ..Default::default()
            };
            state
                .node_add(repository, ROOT_NODE, node, "subdir")
                .await
                .expect("node_add must succeed")
        };

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 6,
                handle,
                node_id: dir_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = node_info_event(&events).expect("node info event must fire");
        assert_eq!(data.id, 6);
        assert_eq!(data.node_id, dir_id);
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(
            data.kind,
            LoreNodeType::Directory as u32,
            "a non-file/non-link node is a directory, got {events:?}"
        );
        assert_eq!(data.name.as_str(), "subdir");
        assert_eq!(data.parent_id, ROOT_NODE);
        assert_eq!(
            data.revision,
            Hash::default(),
            "the node belongs to the handle's loaded revision, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_info_unknown_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("ni-unknown", Partition::from([0x33u8; 16])).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 3,
                handle,
                node_id: INVALID_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "an invalid node id must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_info_event(&events)
            .expect("a failure must still emit the node info terminal carrying the id");
        assert_eq!(data.id, 3);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert_eq!(data.node_id, INVALID_NODE);
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_info_on_unknown_handle_emits_node_info_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 4,
                handle: LoreRevisionTree::INVALID,
                node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_info_event(&events)
            .expect("a handle miss must still emit the node info terminal carrying the id");
        assert_eq!(data.id, 4);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }

    #[tokio::test]
    async fn node_info_nonexistent_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("ni-nonexistent", Partition::from([0x44u8; 16])).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 5,
                handle,
                node_id: 1_000_000,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "a node id past any allocated block must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_info_event(&events)
            .expect("a failure must still emit the node info terminal carrying the id");
        assert_eq!(data.id, 5);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "an unreadable node id must report InvalidArguments, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn node_info_unallocated_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("ni-unallocated", Partition::from([0x88u8; 16])).await;

        // No nodes added: id 1 is an in-range but unallocated slot, which reads
        // back as a zeroed record with an empty name.
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = node_info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeNodeInfoArgs {
                id: 6,
                handle,
                node_id: 1,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "an unallocated node id must fail");
        let events = sink.lock().unwrap().clone();
        let data = node_info_event(&events)
            .expect("a failure must still emit the node info terminal carrying the id");
        assert_eq!(data.id, 6);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "a non-root node with an empty name must report InvalidArguments, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }
}
