// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::node_info::*;
use lore::storage::handle as storage_handle;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::InvalidArguments;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
use lore_revision::interface::LoreNodeType;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

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
    (entry.state_for_tests(), entry.repository_context.clone())
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
        name_hash: hash_string(name),
        ..Default::default()
    };
    state
        .node_add(repository, ROOT_NODE, node, name)
        .await
        .expect("node_add must succeed")
}

fn release(handle: LoreRevisionTree, store_handle_id: u64) {
    rt_handle::unregister(handle);
    storage_handle::unregister(lore::storage::handle::LoreStore {
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
            name_hash: hash_string("subdir"),
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
    let (handle, store_handle_id) = load_handle("ni-unknown", Partition::from([0x33u8; 16])).await;

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

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an invalid node id must fail"
    );
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
    assert!(events.contains(&CapturedEvent::Complete(InvalidArguments::FFI_CODE)));

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

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an unknown handle must fail"
    );
    let events = sink.lock().unwrap().clone();
    let data = node_info_event(&events)
        .expect("a handle miss must still emit the node info terminal carrying the id");
    assert_eq!(data.id, 4);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );
    assert!(events.contains(&CapturedEvent::Complete(InvalidArguments::FFI_CODE)));
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

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "a node id past any allocated block must fail"
    );
    let events = sink.lock().unwrap().clone();
    let data = node_info_event(&events)
        .expect("a failure must still emit the node info terminal carrying the id");
    assert_eq!(data.id, 5);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "an unreadable node id must report InvalidArguments, got {events:?}"
    );
    assert!(events.contains(&CapturedEvent::Complete(InvalidArguments::FFI_CODE)));

    release(handle, store_handle_id);
}

/// Nothing is added to the handle, so id 1 is an in-range but unallocated
/// slot, which reads back as a zeroed record with an empty name.
#[tokio::test]
async fn node_info_unallocated_node_returns_invalid_arguments() {
    let (handle, store_handle_id) =
        load_handle("ni-unallocated", Partition::from([0x88u8; 16])).await;

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

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an unallocated node id must fail"
    );
    let events = sink.lock().unwrap().clone();
    let data = node_info_event(&events)
        .expect("a failure must still emit the node info terminal carrying the id");
    assert_eq!(data.id, 6);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "a non-root node with an empty name must report InvalidArguments, got {events:?}"
    );
    assert!(events.contains(&CapturedEvent::Complete(InvalidArguments::FFI_CODE)));

    release(handle, store_handle_id);
}
