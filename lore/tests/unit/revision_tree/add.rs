// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::add::*;
use lore::revision_tree::call::EntryFailure;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
use lore::revision_tree::list_children::list_children;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
use lore::revision_tree::node_info::node_info;
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
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;

/// Call-level id every test batch is submitted under, distinct from the
/// per-entry ids so the two cannot be confused in an assertion.
const CALL_ID: u64 = 900;

/// Children seeded under one parent before the snapshot collision check runs,
/// enough that the snapshot has to be ordered to be searchable.
const SNAPSHOT_SEED_CHILDREN: usize = 16;

#[derive(Debug, Clone, PartialEq)]
enum CapturedEvent {
    Complete(i32),
    RevisionTreeLoaded(u64),
    AddComplete(u64, NodeID, LoreErrorCode),
    BatchComplete(u64, LoreErrorCode),
    NodeInfo(Box<LoreRevisionTreeNodeInfoEventData>),
    Child(u64, NodeID, String),
    Other(u32),
}

impl CapturedEvent {
    fn from_event(event: &LoreEvent) -> Self {
        match event {
            LoreEvent::Complete(data) => Self::Complete(data.status),
            LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
            LoreEvent::RevisionTreeAddComplete(data) => {
                Self::AddComplete(data.entry_id, data.node_id, data.error_code)
            }
            LoreEvent::RevisionTreeBatchComplete(data) => {
                Self::BatchComplete(data.batch_id, data.error_code)
            }
            LoreEvent::RevisionTreeNodeInfo(data) => Self::NodeInfo(Box::new(data.clone())),
            LoreEvent::RevisionTreeChild(data) => {
                Self::Child(data.id, data.node_id, data.name.as_str().to_string())
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

fn add_outcome(events: &[CapturedEvent], id: u64) -> Option<(NodeID, LoreErrorCode)> {
    events.iter().find_map(|event| match event {
        CapturedEvent::AddComplete(event_id, node_id, error_code) if *event_id == id => {
            Some((*node_id, *error_code))
        }
        _ => None,
    })
}

/// Every batch terminal in emission order, so a test can pin that exactly one
/// fired and what it carried.
fn batch_outcomes(events: &[CapturedEvent]) -> Vec<(u64, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            CapturedEvent::BatchComplete(id, code) => Some((*id, *code)),
            _ => None,
        })
        .collect()
}

fn node_info_event(events: &[CapturedEvent]) -> Option<LoreRevisionTreeNodeInfoEventData> {
    events.iter().find_map(|event| match event {
        CapturedEvent::NodeInfo(data) => Some((**data).clone()),
        _ => None,
    })
}

/// An entry adding `name` under `parent_node_id`.
fn entry(
    entry_id: u64,
    parent_node_id: NodeID,
    name: &str,
    kind: LoreNodeType,
) -> LoreRevisionTreeAddEntry {
    LoreRevisionTreeAddEntry {
        entry_id,
        parent_node_id,
        parent_entry_index: 0,
        name: LoreString::from_str(name),
        kind: kind as u32,
        mode: 0o644,
        size: 0,
        address: Address::default(),
    }
}

/// An entry adding `name` under the node the entry at `parent_entry_index`
/// creates.
fn nested_entry(
    id: u64,
    parent_entry_index: u32,
    name: &str,
    kind: LoreNodeType,
) -> LoreRevisionTreeAddEntry {
    LoreRevisionTreeAddEntry {
        parent_node_id: INVALID_NODE,
        parent_entry_index,
        ..entry(id, ROOT_NODE, name, kind)
    }
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

fn release(handle: LoreRevisionTree, store_handle_id: u64) {
    rt_handle::unregister(handle);
    storage_handle::unregister(lore::storage::handle::LoreStore {
        handle_id: store_handle_id,
    });
}

async fn run_add(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeAddEntry>,
) -> (i32, Vec<CapturedEvent>) {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: CALL_ID,
            handle,
            entries: LoreArray::from_vec(entries),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    (status, events)
}

async fn fetch_node_info(handle: LoreRevisionTree, id: u64, node_id: NodeID) -> Vec<CapturedEvent> {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    node_info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodeInfoArgs {
            id,
            handle,
            node_id,
        },
        make_callback(sink.clone()),
    )
    .await;
    sink.lock().unwrap().clone()
}

async fn list(handle: LoreRevisionTree, id: u64, parent_node_id: NodeID) -> Vec<CapturedEvent> {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    list_children(
        LoreGlobalArgs::default(),
        LoreRevisionTreeListChildrenArgs {
            id,
            handle,
            parent_node_id,
        },
        make_callback(sink.clone()),
    )
    .await;
    sink.lock().unwrap().clone()
}

fn child_names(events: &[CapturedEvent]) -> Vec<String> {
    let mut names: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            CapturedEvent::Child(_, _, name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn add_single_entry_round_trips_through_node_info() {
    let partition = Partition::from([0x11u8; 16]);
    let (handle, store_handle_id) = load_handle("add-single", partition).await;
    let address = Address {
        hash: Hash::from([0x42u8; 32]),
        context: Context::from([0x99u8; 16]),
    };

    let (status, events) = run_add(
        handle,
        vec![LoreRevisionTreeAddEntry {
            mode: 0o644,
            size: 1234,
            address,
            ..entry(1, ROOT_NODE, "doc.md", LoreNodeType::File)
        }],
    )
    .await;

    assert_eq!(status, 0, "a one-entry batch must succeed");
    let (node_id, error_code) = add_outcome(&events, 1).expect("AddComplete must fire");
    assert_eq!(error_code, LoreErrorCode::None, "got {events:?}");
    assert_ne!(node_id, INVALID_NODE, "got {events:?}");

    let info_events = fetch_node_info(handle, 2, node_id).await;
    let data = node_info_event(&info_events).expect("node info must fire");
    assert_eq!(data.name.as_str(), "doc.md");
    assert_eq!(data.parent_id, ROOT_NODE);
    assert_eq!(data.kind, LoreNodeType::File as u32);
    assert_eq!(data.size, 1234);
    assert_eq!(
        data.address, address,
        "a supplied address must cross unchanged, got {info_events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_builds_a_subtree_from_entry_parent_references() {
    let partition = Partition::from([0x22u8; 16]);
    let (handle, store_handle_id) = load_handle("add-subtree", partition).await;

    let (status, events) = run_add(
        handle,
        vec![
            entry(1, ROOT_NODE, "a", LoreNodeType::Directory),
            nested_entry(2, 0, "b", LoreNodeType::Directory),
            nested_entry(3, 1, "c.txt", LoreNodeType::File),
        ],
    )
    .await;

    assert_eq!(status, 0, "building a subtree must succeed, got {events:?}");
    let (a, _) = add_outcome(&events, 1).expect("entry 1 must complete");
    let (b, _) = add_outcome(&events, 2).expect("entry 2 must complete");
    let (c, _) = add_outcome(&events, 3).expect("entry 3 must complete");

    let b_info = fetch_node_info(handle, 4, b).await;
    assert_eq!(
        node_info_event(&b_info).expect("node info").parent_id,
        a,
        "b must hang off a, got {b_info:?}"
    );
    let c_info = fetch_node_info(handle, 5, c).await;
    assert_eq!(
        node_info_event(&c_info).expect("node info").parent_id,
        b,
        "c.txt must hang off b, got {c_info:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_rejects_the_whole_batch_and_creates_nothing() {
    let partition = Partition::from([0x33u8; 16]);
    let (handle, store_handle_id) = load_handle("add-atomic", partition).await;

    let (status, events) = run_add(
        handle,
        vec![
            entry(1, ROOT_NODE, "good", LoreNodeType::File),
            entry(2, ROOT_NODE, "", LoreNodeType::File),
        ],
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "a batch with an invalid entry must fail"
    );
    assert_eq!(
        add_outcome(&events, 2)
            .expect("the offending entry must report")
            .1,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );

    let listed = list(handle, 3, ROOT_NODE).await;
    assert!(
        child_names(&listed).is_empty(),
        "a rejected batch must create nothing, got {listed:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_rejects_duplicate_names_within_one_batch() {
    let partition = Partition::from([0x44u8; 16]);
    let (handle, store_handle_id) = load_handle("add-dup-batch", partition).await;

    let (status, events) = run_add(
        handle,
        vec![
            entry(1, ROOT_NODE, "dup", LoreNodeType::File),
            entry(2, ROOT_NODE, "DUP", LoreNodeType::File),
        ],
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "a case-variant duplicate within a batch must fail"
    );
    assert_eq!(
        add_outcome(&events, 2)
            .expect("the second entry must report")
            .1,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_rejects_a_name_already_in_the_tree() {
    let partition = Partition::from([0x55u8; 16]);
    let (handle, store_handle_id) = load_handle("add-dup-tree", partition).await;

    let first = run_add(handle, vec![entry(1, ROOT_NODE, "dup", LoreNodeType::File)]).await;
    assert_eq!(first.0, 0);

    let (status, events) =
        run_add(handle, vec![entry(2, ROOT_NODE, "dup", LoreNodeType::File)]).await;
    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "colliding with an existing child must fail"
    );
    assert_eq!(
        add_outcome(&events, 2).expect("AddComplete must fire").1,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_rejects_forward_and_non_directory_parent_references() {
    let partition = Partition::from([0x66u8; 16]);
    let (handle, store_handle_id) = load_handle("add-badref", partition).await;

    let forward = run_add(
        handle,
        vec![
            nested_entry(1, 1, "early", LoreNodeType::File),
            entry(2, ROOT_NODE, "later", LoreNodeType::Directory),
        ],
    )
    .await;
    assert_eq!(
        forward.0,
        InvalidArguments::FFI_CODE,
        "a forward parent reference must fail"
    );
    assert_eq!(
        add_outcome(&forward.1, 1).expect("AddComplete must fire").1,
        LoreErrorCode::InvalidArguments
    );

    let leaf_parent = run_add(
        handle,
        vec![
            entry(3, ROOT_NODE, "file", LoreNodeType::File),
            nested_entry(4, 0, "child", LoreNodeType::File),
        ],
    )
    .await;
    assert_eq!(
        leaf_parent.0,
        InvalidArguments::FFI_CODE,
        "parenting onto a file entry must fail"
    );
    assert_eq!(
        add_outcome(&leaf_parent.1, 4)
            .expect("AddComplete must fire")
            .1,
        LoreErrorCode::InvalidArguments
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_rejects_bad_kinds_and_unknown_parents() {
    let partition = Partition::from([0x77u8; 16]);
    let (handle, store_handle_id) = load_handle("add-bad", partition).await;

    let bad_kind = run_add(
        handle,
        vec![LoreRevisionTreeAddEntry {
            kind: 99,
            ..entry(1, ROOT_NODE, "thing", LoreNodeType::File)
        }],
    )
    .await;
    assert_eq!(
        bad_kind.0,
        InvalidArguments::FFI_CODE,
        "an unsupported kind must fail"
    );
    assert_eq!(
        add_outcome(&bad_kind.1, 1)
            .expect("AddComplete must fire")
            .1,
        LoreErrorCode::InvalidArguments
    );

    let unknown = run_add(
        handle,
        vec![entry(2, 1_000_000, "orphan", LoreNodeType::File)],
    )
    .await;
    assert_eq!(
        unknown.0,
        InvalidArguments::FFI_CODE,
        "an unknown parent must fail"
    );
    assert_eq!(
        add_outcome(&unknown.1, 2).expect("AddComplete must fire").1,
        LoreErrorCode::InvalidArguments
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_generates_a_file_id_only_for_files_missing_one() {
    let partition = Partition::from([0x88u8; 16]);
    let (handle, store_handle_id) = load_handle("add-file-id", partition).await;

    let (status, events) = run_add(
        handle,
        vec![
            entry(1, ROOT_NODE, "a.txt", LoreNodeType::File),
            entry(2, ROOT_NODE, "b.txt", LoreNodeType::File),
            entry(3, ROOT_NODE, "dir", LoreNodeType::Directory),
            entry(4, ROOT_NODE, "link", LoreNodeType::Link),
        ],
    )
    .await;
    assert_eq!(status, 0, "got {events:?}");

    let mut file_ids = Vec::new();
    for (event_id, next) in [(1u64, 10u64), (2, 11)] {
        let (node_id, _) = add_outcome(&events, event_id).expect("AddComplete must fire");
        let info = fetch_node_info(handle, next, node_id).await;
        let data = node_info_event(&info).expect("node info must fire");
        assert_ne!(
            data.file_id,
            Context::default(),
            "a file added without a file id must be assigned one, got {info:?}"
        );
        assert_eq!(
            data.address.hash,
            Hash::default(),
            "generating a file id must not disturb the content hash, got {info:?}"
        );
        file_ids.push(data.file_id);
    }
    assert_ne!(
        file_ids[0], file_ids[1],
        "each generated file id must be distinct"
    );

    for (event_id, next) in [(3u64, 12u64), (4, 13)] {
        let (node_id, _) = add_outcome(&events, event_id).expect("AddComplete must fire");
        let info = fetch_node_info(handle, next, node_id).await;
        let data = node_info_event(&info).expect("node info must fire");
        assert_eq!(
            data.file_id,
            Context::default(),
            "only files are assigned a file id, got {info:?}"
        );
    }

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_rejects_a_repeated_caller_id() {
    let partition = Partition::from([0xaau8; 16]);
    let (handle, store_handle_id) = load_handle("add-field-checks", partition).await;

    let shared_id = run_add(
        handle,
        vec![
            entry(2, ROOT_NODE, "first", LoreNodeType::File),
            entry(2, ROOT_NODE, "second", LoreNodeType::File),
        ],
    )
    .await;
    assert_eq!(
        shared_id.0,
        InvalidArguments::FFI_CODE,
        "two entries sharing a caller id must fail"
    );
    assert_eq!(
        add_outcome(&shared_id.1, 2)
            .expect("AddComplete must fire")
            .1,
        LoreErrorCode::InvalidArguments
    );

    let listed = list(handle, 5, ROOT_NODE).await;
    assert!(
        child_names(&listed).is_empty(),
        "no rejected batch may leave a node behind, got {listed:?}"
    );

    release(handle, store_handle_id);
}

/// An `entry_id` of zero says the entry is not being correlated, so several
/// entries may share it while any other repeated id is still a mistake.
#[tokio::test]
async fn add_accepts_repeated_zero_caller_ids() {
    let partition = Partition::from([0xacu8; 16]);
    let (handle, store_handle_id) = load_handle("add-zero-ids", partition).await;

    let (status, events) = run_add(
        handle,
        vec![
            entry(0, ROOT_NODE, "first", LoreNodeType::File),
            entry(0, ROOT_NODE, "second", LoreNodeType::File),
        ],
    )
    .await;

    assert_eq!(status, 0, "repeated zero ids must be accepted, {events:?}");
    let listed = list(handle, 5, ROOT_NODE).await;
    assert_eq!(
        child_names(&listed),
        vec!["first".to_string(), "second".to_string()],
        "got {listed:?}"
    );

    release(handle, store_handle_id);
}

/// Fields a kind does not carry are dropped rather than rejected, so the
/// stored node reports the normalised values and not what was passed.
#[tokio::test]
async fn add_normalizes_fields_a_kind_does_not_carry() {
    let partition = Partition::from([0xadu8; 16]);
    let (handle, store_handle_id) = load_handle("add-normalize", partition).await;
    let address = Address {
        hash: Hash::from([0x5au8; 32]),
        context: Context::from([0x5bu8; 16]),
    };

    let (status, events) = run_add(
        handle,
        vec![
            LoreRevisionTreeAddEntry {
                size: 512,
                address,
                ..entry(1, ROOT_NODE, "dir", LoreNodeType::Directory)
            },
            LoreRevisionTreeAddEntry {
                size: 999,
                address,
                ..entry(2, ROOT_NODE, "link", LoreNodeType::Link)
            },
        ],
    )
    .await;
    assert_eq!(status, 0, "normalised fields must not fail, got {events:?}");

    let (directory, _) = add_outcome(&events, 1).expect("AddComplete must fire");
    let info = fetch_node_info(handle, 10, directory).await;
    let data = node_info_event(&info).expect("node info must fire");
    assert_eq!(data.size, 0, "a directory stores no size, got {info:?}");
    assert_eq!(
        data.address,
        Address::default(),
        "a directory stores no address, got {info:?}"
    );

    let (link, _) = add_outcome(&events, 2).expect("AddComplete must fire");
    let info = fetch_node_info(handle, 11, link).await;
    let data = node_info_event(&info).expect("node info must fire");
    assert_eq!(data.size, 0, "a link stores no size, got {info:?}");
    assert_eq!(
        data.address, address,
        "a link keeps its target address, got {info:?}"
    );

    release(handle, store_handle_id);
}

/// A deleted node keeps its name and carries neither the file nor the link
/// flag, so it reads back as an ordinary directory. Without a check of its
/// own it is accepted as a parent, and the child is orphaned as soon as the
/// allocator hands the freed slot out again.
#[tokio::test]
async fn add_rejects_a_parent_that_has_been_deleted() {
    let partition = Partition::from([0xbeu8; 16]);
    let (handle, store_handle_id) = load_handle("add-deleted-parent", partition).await;

    let (status, events) = run_add(
        handle,
        vec![entry(1, ROOT_NODE, "doomed", LoreNodeType::Directory)],
    )
    .await;
    assert_eq!(status, 0, "got {events:?}");
    let (doomed, _) = add_outcome(&events, 1).expect("AddComplete must fire");

    {
        let guard = rt_handle::RevisionTreeGuard::enter(handle).expect("handle must resolve");
        let internal = guard.internal_clone();
        let block_index = NodeBlock::index(doomed);
        let block = internal
            .state_for_tests()
            .block(internal.repository_context.clone(), block_index)
            .await
            .expect("the parent block must be readable");
        block
            .write_for_tests()
            .discard_node(block_index, Node::index(doomed));
    }

    let (status, events) =
        run_add(handle, vec![entry(2, doomed, "child", LoreNodeType::File)]).await;
    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "a deleted parent must be rejected, got {events:?}"
    );
    assert_eq!(
        add_outcome(&events, 2).expect("AddComplete must fire").1,
        LoreErrorCode::InvalidArguments,
        "a deleted parent is a bad argument, not an apply failure, got {events:?}"
    );

    release(handle, store_handle_id);
}

/// A parent taking several entries has its child names collected in one walk
/// instead of a lookup per entry, so the collision check runs against that
/// snapshot rather than against the tree.
#[tokio::test]
async fn add_rejects_a_tree_collision_when_one_parent_takes_several_entries() {
    let partition = Partition::from([0xbbu8; 16]);
    let (handle, store_handle_id) = load_handle("add-dup-snapshot", partition).await;

    let seeded: Vec<String> = (0..SNAPSHOT_SEED_CHILDREN)
        .map(|index| format!("seed-{index:02}"))
        .collect();
    let seed = run_add(
        handle,
        seeded
            .iter()
            .enumerate()
            .map(|(index, name)| entry(index as u64 + 1, ROOT_NODE, name, LoreNodeType::File))
            .collect(),
    )
    .await;
    assert_eq!(seed.0, 0, "got {:?}", seed.1);

    for (index, name) in seeded.iter().enumerate() {
        let (status, events) = run_add(
            handle,
            vec![
                entry(100, ROOT_NODE, "fresh", LoreNodeType::File),
                entry(101, ROOT_NODE, &name.to_uppercase(), LoreNodeType::File),
            ],
        )
        .await;
        assert_eq!(
            status,
            InvalidArguments::FFI_CODE,
            "colliding with existing child {index} must fail, got {events:?}"
        );
        assert_eq!(
            add_outcome(&events, 101).expect("AddComplete must fire").1,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
    }

    let listed = list(handle, 5, ROOT_NODE).await;
    assert_eq!(
        child_names(&listed),
        seeded,
        "no rejected batch may leave a node behind, got {listed:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn add_with_no_entries_succeeds() {
    let partition = Partition::from([0x99u8; 16]);
    let (handle, store_handle_id) = load_handle("add-empty", partition).await;

    let (status, events) = run_add(handle, Vec::new()).await;

    assert_eq!(status, 0, "an empty batch must succeed, got {events:?}");

    release(handle, store_handle_id);
}

/// An unknown handle is the call's failure, not any entry's, so it reports on
/// the batch terminal alone and no entry is left looking as though it was
/// individually rejected.
#[tokio::test]
async fn add_on_unknown_handle_reports_only_the_batch_terminal() {
    let (status, events) = run_add(
        LoreRevisionTree::INVALID,
        vec![
            entry(7, ROOT_NODE, "x", LoreNodeType::File),
            entry(8, ROOT_NODE, "y", LoreNodeType::File),
        ],
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an unknown handle must fail"
    );
    for id in [7u64, 8] {
        assert!(
            add_outcome(&events, id).is_none(),
            "entry {id} must not report on a handle miss, got {events:?}"
        );
    }
    assert!(
        events.contains(&CapturedEvent::BatchComplete(
            CALL_ID,
            LoreErrorCode::InvalidArguments
        )),
        "the batch terminal must carry the call id, got {events:?}"
    );
    assert!(events.contains(&CapturedEvent::Complete(InvalidArguments::FFI_CODE)));
}

/// The batch terminal fires once on every path, so a caller can wait on it
/// whether the call succeeded or failed.
#[tokio::test]
async fn add_reports_the_batch_terminal_on_success_and_rejection() {
    let partition = Partition::from([0xaeu8; 16]);
    let (handle, store_handle_id) = load_handle("add-batch-terminal", partition).await;

    let (status, events) =
        run_add(handle, vec![entry(1, ROOT_NODE, "ok", LoreNodeType::File)]).await;
    assert_eq!(status, 0, "got {events:?}");
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::None)],
        "got {events:?}"
    );

    let (status, events) = run_add(handle, vec![entry(2, ROOT_NODE, "", LoreNodeType::File)]).await;
    assert_eq!(status, InvalidArguments::FFI_CODE, "got {events:?}");
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
        "a rejected batch reports the call outcome too, got {events:?}"
    );

    let (status, events) = run_add(handle, Vec::new()).await;
    assert_eq!(status, 0, "got {events:?}");
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::None)],
        "an empty batch still reports, got {events:?}"
    );

    release(handle, store_handle_id);
}

/// A batch's failures hold nothing per entry until one fails, so a batch that fails nothing
/// allocates nothing for them. An entry whose parent entry created no node fails as the parent
/// did: as noted, or `Internal` where the parent reported nothing, as the entries of an apply task
/// that died. The call reports the strongest failure noted, and a task lost counts as `Internal`.
#[test]
fn an_entry_below_a_failed_entry_fails_as_its_parent_did() {
    let mut failures = Failures::new(4);
    assert_eq!(
        failures.entries.capacity(),
        0,
        "nothing held before a failure"
    );
    assert_eq!(
        failures.of(0),
        EntryFailure::Internal,
        "an entry that reported nothing"
    );

    failures.note(1, EntryFailure::SlowedDown);
    assert_eq!(failures.of(1), EntryFailure::SlowedDown);
    assert_eq!(failures.of(2), EntryFailure::Internal);
    assert_eq!(failures.strongest, EntryFailure::SlowedDown);

    failures.note(2, failures.of(1));
    assert_eq!(
        failures.of(2),
        EntryFailure::SlowedDown,
        "a child of a slowed entry"
    );
    assert_eq!(failures.strongest, EntryFailure::SlowedDown);

    failures.note(3, EntryFailure::Internal);
    assert_eq!(failures.strongest, EntryFailure::Internal);

    let mut lost = Failures::new(2);
    lost.note(0, EntryFailure::SlowedDown);
    lost.lost();
    assert_eq!(lost.strongest, EntryFailure::Internal, "a task that died");
}
