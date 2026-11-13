// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::node_path::*;
use lore_base::error::InvalidArguments;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeNodePathEventData;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_storage::hash::hash_string;

use crate::call::test_support::complete_statuses;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;
use crate::revision_tree::support::handle_state;
use crate::revision_tree::support::load_handle;
use crate::revision_tree::support::release;

fn node_path_event(events: &[LoreEvent]) -> Option<&LoreRevisionTreeNodePathEventData> {
    events.iter().find_map(|event| match event {
        LoreEvent::RevisionTreeNodePath(data) => Some(data),
        _ => None,
    })
}

/// Add a node under `parent` and return its id. `is_file` chooses a file vs
/// directory; both can themselves parent further children.
async fn add_under(handle: LoreRevisionTree, parent: NodeID, name: &str, is_file: bool) -> NodeID {
    let (state, repository) = handle_state(handle);
    let flags = if is_file { NodeFlags::File.bits() } else { 0 };
    let node = Node {
        flags,
        name_hash: hash_string(name),
        ..Default::default()
    };
    state
        .node_add(repository, parent, node, name)
        .await
        .expect("node_add must succeed")
}

#[tokio::test]
async fn node_path_walks_parents_to_root() {
    let partition = Partition::from([0x11u8; 16]);
    let (handle, store_handle_id) = load_handle("np-walk", partition).await;
    let dir_id = add_under(handle, ROOT_NODE, "a", false).await;
    let file_id = add_under(handle, dir_id, "b", true).await;

    let (sink, callback) = recording_callback();
    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 1,
            handle,
            node_id: file_id,
        },
        callback,
    )
    .await;

    assert_eq!(status, 0);
    let events = take_recorded(&sink);
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
    assert!(complete_statuses(&events).contains(&0));

    release(handle, store_handle_id);
}

#[tokio::test]
async fn node_path_single_component_under_root() {
    let (handle, store_handle_id) = load_handle("np-single", Partition::from([0x66u8; 16])).await;
    let file_id = add_under(handle, ROOT_NODE, "file", true).await;

    let (sink, callback) = recording_callback();
    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 7,
            handle,
            node_id: file_id,
        },
        callback,
    )
    .await;

    assert_eq!(status, 0);
    let events = take_recorded(&sink);
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

    let (sink, callback) = recording_callback();
    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 8,
            handle,
            node_id: c,
        },
        callback,
    )
    .await;

    assert_eq!(status, 0);
    let events = take_recorded(&sink);
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

    let (sink, callback) = recording_callback();
    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 2,
            handle,
            node_id: ROOT_NODE,
        },
        callback,
    )
    .await;

    assert_eq!(status, 0);
    let events = take_recorded(&sink);
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
    let (handle, store_handle_id) = load_handle("np-unknown", Partition::from([0x33u8; 16])).await;

    let (sink, callback) = recording_callback();
    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 3,
            handle,
            node_id: INVALID_NODE,
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an invalid node id must fail"
    );
    let events = take_recorded(&sink);
    let data = node_path_event(&events)
        .expect("a failure must still emit the node path terminal carrying the id");
    assert_eq!(data.id, 3);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );
    assert!(complete_statuses(&events).contains(&InvalidArguments::FFI_CODE));

    release(handle, store_handle_id);
}

#[tokio::test]
async fn node_path_nonexistent_node_returns_invalid_arguments() {
    let (handle, store_handle_id) =
        load_handle("np-nonexistent", Partition::from([0x44u8; 16])).await;

    let (sink, callback) = recording_callback();
    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 5,
            handle,
            node_id: 1_000_000,
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "a node id past any allocated block must fail"
    );
    let events = take_recorded(&sink);
    let data = node_path_event(&events)
        .expect("a failure must still emit the node path terminal carrying the id");
    assert_eq!(data.id, 5);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "an unknown node id must report InvalidArguments, got {events:?}"
    );
    assert!(complete_statuses(&events).contains(&InvalidArguments::FFI_CODE));

    release(handle, store_handle_id);
}

/// Nothing is added to the handle, so id 1 is an in-range but unallocated
/// slot, which would otherwise reconstruct to an empty (root-looking) path.
#[tokio::test]
async fn node_path_unallocated_node_returns_invalid_arguments() {
    let (handle, store_handle_id) =
        load_handle("np-unallocated", Partition::from([0x55u8; 16])).await;

    let (sink, callback) = recording_callback();
    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 6,
            handle,
            node_id: 1,
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an unallocated node id must fail"
    );
    let events = take_recorded(&sink);
    let data = node_path_event(&events)
        .expect("a failure must still emit the node path terminal carrying the id");
    assert_eq!(data.id, 6);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "a non-root node resolving to an empty path must report InvalidArguments, got {events:?}"
    );
    assert!(complete_statuses(&events).contains(&InvalidArguments::FFI_CODE));

    release(handle, store_handle_id);
}

#[tokio::test]
async fn node_path_on_unknown_handle_emits_terminal_with_invalid_arguments() {
    let (sink, callback) = recording_callback();

    let status = node_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodePathArgs {
            id: 4,
            handle: LoreRevisionTree::INVALID,
            node_id: ROOT_NODE,
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an unknown handle must fail"
    );
    let events = take_recorded(&sink);
    let data = node_path_event(&events)
        .expect("a handle miss must still emit the node path terminal carrying the id");
    assert_eq!(data.id, 4);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );
    assert!(complete_statuses(&events).contains(&InvalidArguments::FFI_CODE));
}
