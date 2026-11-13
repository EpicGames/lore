// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

use lore::call::setup_execution;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::add::add;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::resolve_path::*;
use lore_base::error::InvalidArguments;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::RepositoryId;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryWriteToken;
use lore_storage::hash::hash_string;

use crate::call::test_support::complete_statuses;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;
use crate::revision_tree::support::add_outcome;
use crate::revision_tree::support::handle_state;
use crate::revision_tree::support::load_handle;
use crate::revision_tree::support::release;

fn resolve_outcome(
    events: &[LoreEvent],
    id: u64,
) -> Option<(NodeID, RepositoryId, Hash, LoreErrorCode)> {
    events.iter().find_map(|event| match event {
        LoreEvent::RevisionTreeResolvePathComplete(data) if data.id == id => Some((
            data.node_id,
            data.repository,
            data.revision,
            data.error_code,
        )),
        _ => None,
    })
}

/// Add a link node under root targeting `(repository, revision, target_node)`.
/// Sets `name_hash` so `find_node_link` resolves it by name.
async fn add_link(
    handle: LoreRevisionTree,
    name: &str,
    repository: Partition,
    revision: Hash,
    target_node: NodeID,
) -> NodeID {
    LORE_CONTEXT
        .scope(
            setup_execution(LoreGlobalArgs::default(), None),
            async move {
                let (state, repository_context) = handle_state(handle);
                let node = Node {
                    flags: NodeFlags::Link.bits(),
                    name_hash: hash_string(name),
                    child: target_node,
                    address: Address {
                        hash: revision,
                        context: Context::from(repository),
                    },
                    ..Default::default()
                };
                state
                    .node_add(repository_context, ROOT_NODE, node, name)
                    .await
                    .expect("node_add must succeed")
            },
        )
        .await
}

/// Add `child_name` under root, then serialize the state to a committed
/// revision a link can point at. Returns the revision hash.
///
/// Serializing reads through the store, so it runs in an execution context of
/// its own: every other call in these tests gets one from the dispatcher.
async fn seal_target(handle: LoreRevisionTree, child_name: &str) -> Hash {
    let (state, repository_context) = handle_state(handle);
    let child = Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string(child_name),
        ..Default::default()
    };
    state
        .node_add(repository_context.clone(), ROOT_NODE, child, child_name)
        .await
        .expect("node_add child must succeed");
    let token = RepositoryWriteToken::acquire(Path::new("link-target")).await;
    LORE_CONTEXT
        .scope(
            setup_execution(LoreGlobalArgs::default(), None),
            async move {
                state
                    .serialize(repository_context, &token)
                    .await
                    .expect("serialize must succeed")
            },
        )
        .await
}

#[tokio::test]
async fn resolve_empty_path_returns_root() {
    let (handle, store_handle_id) =
        load_handle("resolve-empty", Partition::from([0x11u8; 16])).await;
    let (sink, callback) = recording_callback();

    let status = resolve_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeResolvePathArgs {
            id: 7,
            handle,
            path: LoreString::default(),
        },
        callback,
    )
    .await;

    assert_eq!(status, 0, "resolving the empty path must succeed");
    let events = take_recorded(&sink);
    let (node_id, repository, _revision, error_code) =
        resolve_outcome(&events, 7).expect("ResolvePathComplete must fire");
    assert_eq!(
        node_id, ROOT_NODE,
        "empty path must resolve to the root node, got {events:?}"
    );
    assert_eq!(error_code, LoreErrorCode::None);
    assert_eq!(
        repository,
        Partition::from([0x11u8; 16]),
        "root resolves in the handle's repository, got {events:?}"
    );
    let complete_pos = events
        .iter()
        .position(|event| matches!(event, LoreEvent::Complete(_)))
        .expect("Complete must fire");
    let resolve_pos = events
        .iter()
        .position(|event| matches!(event, LoreEvent::RevisionTreeResolvePathComplete(..)))
        .expect("ResolvePathComplete must fire");
    assert!(
        resolve_pos < complete_pos,
        "ResolvePathComplete must fire before Complete, got {events:?}"
    );

    release(handle, store_handle_id);
}

/// Build `a/b/c.txt` in one batch add. Returns the node ids of `a`, `a/b`
/// and `a/b/c.txt`.
async fn build_nested_tree(handle: LoreRevisionTree) -> (NodeID, NodeID, NodeID) {
    let directory = |entry_id: u64, parent_entry_index: u32, name: &str, nested: bool| {
        LoreRevisionTreeAddEntry {
            entry_id,
            parent_node_id: if nested { INVALID_NODE } else { ROOT_NODE },
            parent_entry_index,
            name: LoreString::from_str(name),
            kind: LoreNodeType::Directory as u32,
            mode: 0o755,
            size: 0,
            address: Address::default(),
        }
    };
    let entries = vec![
        directory(1, 0, "a", false),
        directory(2, 0, "b", true),
        LoreRevisionTreeAddEntry {
            kind: LoreNodeType::File as u32,
            mode: 0o644,
            size: 12,
            ..directory(3, 1, "c.txt", true)
        },
    ];

    let (sink, callback) = recording_callback();
    let status = add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: 100,
            handle,
            entries: LoreArray::from_vec(entries),
        },
        callback,
    )
    .await;
    let events = take_recorded(&sink);
    assert_eq!(status, 0, "building the fixture tree must succeed");

    let added = |id: u64| {
        let (node_id, error_code) = add_outcome(&events, id)
            .unwrap_or_else(|| panic!("entry {id} must emit AddComplete, got {events:?}"));
        assert_eq!(error_code, LoreErrorCode::None, "entry {id} must succeed");
        node_id
    };
    (added(1), added(2), added(3))
}

/// Each level is asserted separately: resolving only the leaf would pass even
/// if the walk skipped or double-consumed an intermediate component.
#[tokio::test]
async fn resolve_existing_path_returns_node_id() {
    let repository = Partition::from([0x99u8; 16]);
    let (handle, store_handle_id) = load_handle("resolve-existing", repository).await;
    let (a, b, c) = build_nested_tree(handle).await;

    for (id, path, expected) in [(20u64, "a", a), (21, "a/b", b), (22, "a/b/c.txt", c)] {
        let (sink, callback) = recording_callback();
        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id,
                handle,
                path: LoreString::from_str(path),
            },
            callback,
        )
        .await;

        assert_eq!(status, 0, "resolving {path} must succeed");
        let events = take_recorded(&sink);
        let (node_id, resolved_repository, _revision, error_code) = resolve_outcome(&events, id)
            .unwrap_or_else(|| panic!("ResolvePathComplete must fire for {path}"));
        assert_eq!(error_code, LoreErrorCode::None, "got {events:?}");
        assert_eq!(
            node_id, expected,
            "{path} must resolve to the node the add reported, got {events:?}"
        );
        assert_eq!(
            resolved_repository, repository,
            "a link-free path resolves in the handle's repository, got {events:?}"
        );
    }

    release(handle, store_handle_id);
}

#[tokio::test]
async fn resolve_path_below_a_leaf_returns_invalid_arguments() {
    let (handle, store_handle_id) =
        load_handle("resolve-below-leaf", Partition::from([0xAAu8; 16])).await;
    build_nested_tree(handle).await;

    let (sink, callback) = recording_callback();
    let status = resolve_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeResolvePathArgs {
            id: 23,
            handle,
            path: LoreString::from_str("a/b/c.txt/deeper"),
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "descending through a file must fail"
    );
    let events = take_recorded(&sink);
    let (node_id, _repository, _revision, error_code) =
        resolve_outcome(&events, 23).expect("ResolvePathComplete must fire");
    assert_eq!(
        error_code,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );
    assert_eq!(node_id, INVALID_NODE);

    release(handle, store_handle_id);
}

#[tokio::test]
async fn resolve_missing_path_returns_invalid_arguments() {
    let (handle, store_handle_id) =
        load_handle("resolve-missing", Partition::from([0x22u8; 16])).await;
    let (sink, callback) = recording_callback();

    let status = resolve_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeResolvePathArgs {
            id: 8,
            handle,
            path: LoreString::from_str("no/such/path"),
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "resolving a missing path must fail"
    );
    let events = take_recorded(&sink);
    let (node_id, _repository, _revision, error_code) =
        resolve_outcome(&events, 8).expect("ResolvePathComplete must fire for the caller id");
    assert_eq!(
        error_code,
        LoreErrorCode::InvalidArguments,
        "a missing path must report InvalidArguments, got {events:?}"
    );
    assert_eq!(
        node_id, INVALID_NODE,
        "a failed resolve must report the invalid-node sentinel, got {events:?}"
    );
    assert!(
        complete_statuses(&events).contains(&InvalidArguments::FFI_CODE),
        "missing path must complete with status=InvalidArguments, got {events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn resolve_path_on_unknown_handle_emits_resolve_complete_with_invalid_arguments() {
    let (sink, callback) = recording_callback();

    let status = resolve_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeResolvePathArgs {
            id: 10,
            handle: LoreRevisionTree::INVALID,
            path: LoreString::default(),
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "resolving against an unknown handle must fail"
    );
    let events = take_recorded(&sink);
    let (node_id, _repository, _revision, error_code) = resolve_outcome(&events, 10)
        .expect("a handle miss must still emit ResolvePathComplete carrying the caller id");
    assert_eq!(
        error_code,
        LoreErrorCode::InvalidArguments,
        "a handle miss must report InvalidArguments, got {events:?}"
    );
    assert_eq!(
        node_id, INVALID_NODE,
        "a handle miss must report the invalid-node sentinel, got {events:?}"
    );
    assert!(
        complete_statuses(&events).contains(&InvalidArguments::FFI_CODE),
        "a handle miss must complete with status=InvalidArguments, got {events:?}"
    );
}

#[tokio::test]
async fn resolve_path_to_a_link_node_returns_the_link_node() {
    let repository = Partition::from([0x77u8; 16]);
    let (handle, store_handle_id) = load_handle("resolve-link-node", repository).await;
    let link_id = add_link(
        handle,
        "link",
        repository,
        Hash::from([0xABu8; 32]),
        ROOT_NODE,
    )
    .await;

    let (sink, callback) = recording_callback();
    let status = resolve_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeResolvePathArgs {
            id: 11,
            handle,
            path: LoreString::from_str("link"),
        },
        callback,
    )
    .await;

    assert_eq!(status, 0, "resolving to a link node must succeed");
    let events = take_recorded(&sink);
    let (node_id, resolved_repository, _revision, error_code) =
        resolve_outcome(&events, 11).expect("ResolvePathComplete must fire");
    assert_eq!(error_code, LoreErrorCode::None);
    assert_eq!(
        node_id, link_id,
        "must resolve to the link node itself, not its target, got {events:?}"
    );
    assert_eq!(
        resolved_repository, repository,
        "the link node lives in the handle's repository, got {events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn resolve_path_through_a_link_returns_the_target_repository_and_revision() {
    let repository = Partition::from([0x88u8; 16]);
    let (handle, store_handle_id) = load_handle("resolve-link-through", repository).await;
    let target_revision = seal_target(handle, "doc").await;
    add_link(handle, "link", repository, target_revision, ROOT_NODE).await;

    let (sink, callback) = recording_callback();
    let status = resolve_path(
        LoreGlobalArgs::default(),
        LoreRevisionTreeResolvePathArgs {
            id: 12,
            handle,
            path: LoreString::from_str("link/doc"),
        },
        callback,
    )
    .await;

    assert_eq!(status, 0, "resolving through a link must succeed");
    let events = take_recorded(&sink);
    let (node_id, resolved_repository, resolved_revision, error_code) =
        resolve_outcome(&events, 12).expect("ResolvePathComplete must fire");
    assert_eq!(error_code, LoreErrorCode::None);
    assert_ne!(
        node_id, INVALID_NODE,
        "must resolve to the target's node, got {events:?}"
    );
    assert_eq!(
        resolved_repository, repository,
        "must report the link target's repository, got {events:?}"
    );
    assert_eq!(
        resolved_revision, target_revision,
        "must report the link target's revision, got {events:?}"
    );

    release(handle, store_handle_id);
}
