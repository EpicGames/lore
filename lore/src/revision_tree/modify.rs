// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_modify` — update a leaf node's `mode`, `size`, and
//! `address` while preserving its `file_id` (the `address.context` slot).
//! Non-leaf targets are rejected with `LORE_ERROR_CODE_INVALID_ARGUMENTS`.

use lore_base::types::Address;
use lore_macro::LoreArgs;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeModifyCompleteEventData;
use lore_revision::node::INVALID_NODE;
#[cfg(test)]
use lore_revision::node::Node;
#[cfg(test)]
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_modify`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(modify_impl)]
pub struct LoreRevisionTreeModifyArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Leaf node to update; non-leaf targets are rejected
    pub node_id: NodeID,
    /// New POSIX permission bits
    pub mode: u16,
    /// New content size in bytes
    pub size: u64,
    /// New content address; the existing `file_id` context is preserved
    pub address: Address,
}

fn emit_modify_complete(id: u64, node_id: NodeID, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeModifyComplete(LoreRevisionTreeModifyCompleteEventData {
        id,
        node_id,
        error_code,
    })
    .send();
}

/// Update a leaf node's `mode`, `size`, and content address.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_MODIFY_COMPLETE`
/// echoing the modified node id with `error_code = NONE`, before
/// `Complete {status: 0}`. Only leaf nodes (files) can be modified —
/// directories derive their hash and size from their children at commit, and
/// a link's target is changed by delete + add. The node's `file_id` (the
/// `address.context` slot) is preserved: the caller passes the new content
/// hash in `address.hash` and either the matching file id or a zero context;
/// a non-zero context that does not match the node's existing file id is
/// rejected with `INVALID_ARGUMENTS`.
pub async fn modify(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeModifyArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, modify_impl).await
}

async fn modify_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeModifyArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        modify,
        move |_| {
            emit_modify_complete(miss_id, INVALID_NODE, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeModifyArgs| {
            let id = args.id;
            match lore_revision::revision_tree::modify_file(
                internal.state(),
                internal.repository_context.clone(),
                args.node_id,
                args.mode,
                args.size,
                args.address,
            )
            .await
            {
                Ok(node_id) => {
                    emit_modify_complete(id, node_id, LoreErrorCode::None);
                    Ok(())
                }
                Err(error) => {
                    let error_code = if error.is_invalid_arguments() {
                        LoreErrorCode::InvalidArguments
                    } else {
                        LoreErrorCode::Internal
                    };
                    emit_modify_complete(id, INVALID_NODE, error_code);
                    Err(error)
                }
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
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
        ModifyComplete(u64, NodeID, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeModifyComplete(data) => {
                    Self::ModifyComplete(data.id, data.node_id, data.error_code)
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

    fn modify_outcome(events: &[CapturedEvent], id: u64) -> Option<(NodeID, LoreErrorCode)> {
        events.iter().find_map(|event| match event {
            CapturedEvent::ModifyComplete(event_id, node_id, error_code) if *event_id == id => {
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

    /// Add a node directly via `State::node_add` so modify has a target that
    /// is not marked staged-add (mirroring a node loaded from a parent
    /// revision). Returns the new node id.
    async fn add_node(
        handle: LoreRevisionTree,
        name: &str,
        flags: u16,
        address: Address,
    ) -> NodeID {
        let (state, repository_context) = handle_state(handle);
        let node = Node {
            flags,
            name_hash: hash_string(name),
            mode: 0o644,
            size: 10,
            address,
            ..Default::default()
        };
        state
            .node_add(repository_context, ROOT_NODE, node, name)
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
    async fn modify_updates_leaf_and_preserves_file_id() {
        let (handle, store_handle_id) =
            load_handle("modify-leaf", Partition::from([0x11u8; 16])).await;
        let file_id = Context::from([0x99u8; 16]);
        let node_id = add_node(
            handle,
            "doc.md",
            NodeFlags::File.bits(),
            Address {
                hash: Hash::from([0x42u8; 32]),
                context: file_id,
            },
        )
        .await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = modify(
            LoreGlobalArgs::default(),
            LoreRevisionTreeModifyArgs {
                id: 1,
                handle,
                node_id,
                mode: 0o600,
                size: 4321,
                address: Address {
                    hash: Hash::from([0x24u8; 32]),
                    // A zero context means "keep the node's file id".
                    context: Context::default(),
                },
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0, "modifying a leaf must succeed");
        let events = sink.lock().unwrap().clone();
        let (echoed, error_code) = modify_outcome(&events, 1).expect("ModifyComplete must fire");
        assert_eq!(error_code, LoreErrorCode::None);
        assert_eq!(echoed, node_id, "the modified node id must be echoed");
        assert!(events.contains(&CapturedEvent::Complete(0)));

        let (state, repository_context) = handle_state(handle);
        let node = state
            .node(repository_context, node_id)
            .await
            .expect("modified node must be readable");
        assert_eq!(node.address.hash, Hash::from([0x24u8; 32]));
        assert_eq!(node.address.context, file_id, "file id must be preserved");
        assert_eq!(node.mode, 0o600);
        assert_eq!(node.size, 4321);
        assert!(
            node.is_staged_modify(),
            "modified node must be marked StagedModify, flags {:x}",
            node.flags
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn modify_with_matching_context_succeeds_and_mismatch_fails() {
        let (handle, store_handle_id) =
            load_handle("modify-context", Partition::from([0x22u8; 16])).await;
        let file_id = Context::from([0x99u8; 16]);
        let node_id = add_node(
            handle,
            "doc.md",
            NodeFlags::File.bits(),
            Address {
                hash: Hash::from([0x42u8; 32]),
                context: file_id,
            },
        )
        .await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = modify(
            LoreGlobalArgs::default(),
            LoreRevisionTreeModifyArgs {
                id: 2,
                handle,
                node_id,
                mode: 0o644,
                size: 1,
                address: Address {
                    hash: Hash::from([0x25u8; 32]),
                    context: file_id,
                },
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "a matching file id must succeed");

        let status = modify(
            LoreGlobalArgs::default(),
            LoreRevisionTreeModifyArgs {
                id: 3,
                handle,
                node_id,
                mode: 0o644,
                size: 1,
                address: Address {
                    hash: Hash::from([0x26u8; 32]),
                    context: Context::from([0x13u8; 16]),
                },
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 1, "a mismatched file id must fail");
        let events = sink.lock().unwrap().clone();
        let (_, error_code) = modify_outcome(&events, 3).expect("ModifyComplete must fire");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn modify_directory_or_link_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("modify-nonleaf", Partition::from([0x33u8; 16])).await;
        let dir_id = add_node(
            handle,
            "docs",
            NodeFlags::NoFlags.bits(),
            Address::default(),
        )
        .await;
        let link_id = add_node(
            handle,
            "link",
            NodeFlags::Link.bits(),
            Address {
                hash: Hash::from([0xABu8; 32]),
                context: Context::from([0x11u8; 16]),
            },
        )
        .await;

        for (id, node_id) in [(4u64, dir_id), (5u64, link_id)] {
            let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let status = modify(
                LoreGlobalArgs::default(),
                LoreRevisionTreeModifyArgs {
                    id,
                    handle,
                    node_id,
                    mode: 0o644,
                    size: 1,
                    address: Address::default(),
                },
                make_callback(sink.clone()),
            )
            .await;
            assert_eq!(status, 1, "a non-leaf target must fail");
            let events = sink.lock().unwrap().clone();
            let (echoed, error_code) =
                modify_outcome(&events, id).expect("ModifyComplete must fire");
            assert_eq!(
                error_code,
                LoreErrorCode::InvalidArguments,
                "got {events:?}"
            );
            assert_eq!(echoed, INVALID_NODE);
        }

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn modify_unknown_node_or_root_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("modify-unknown", Partition::from([0x44u8; 16])).await;

        for (id, node_id) in [(6u64, ROOT_NODE), (7u64, INVALID_NODE), (8u64, 1_000_000)] {
            let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let status = modify(
                LoreGlobalArgs::default(),
                LoreRevisionTreeModifyArgs {
                    id,
                    handle,
                    node_id,
                    mode: 0o644,
                    size: 1,
                    address: Address::default(),
                },
                make_callback(sink.clone()),
            )
            .await;
            assert_eq!(status, 1, "node id {node_id} must fail");
            let events = sink.lock().unwrap().clone();
            let (_, error_code) = modify_outcome(&events, id).expect("ModifyComplete must fire");
            assert_eq!(
                error_code,
                LoreErrorCode::InvalidArguments,
                "got {events:?}"
            );
        }

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn modify_on_unknown_handle_emits_modify_complete_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = modify(
            LoreGlobalArgs::default(),
            LoreRevisionTreeModifyArgs {
                id: 9,
                handle: LoreRevisionTree::INVALID,
                node_id: 1,
                mode: 0o644,
                size: 1,
                address: Address::default(),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "modifying against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, error_code) = modify_outcome(&events, 9)
            .expect("a handle miss must still emit ModifyComplete carrying the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert_eq!(node_id, INVALID_NODE);
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }
}
