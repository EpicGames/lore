// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::add::add;
use lore::revision_tree::call::revision_tree_call;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::handle::RevisionTreeInternal;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::modify::*;
use lore::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
use lore::revision_tree::node_info::node_info;
use lore::storage::handle as storage_handle;
use lore::storage::store::in_memory_for_tests;
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

/// Files seeded and then rewritten in one call, enough that the apply phase
/// spreads them over more than one task on any ordinary machine.
const BATCH_FILES: usize = 32;

#[derive(Debug, Clone, PartialEq)]
enum CapturedEvent {
    Complete(i32, String),
    RevisionTreeLoaded(u64),
    AddComplete(u64, NodeID, LoreErrorCode),
    ModifyComplete(u64, NodeID, LoreErrorCode),
    BatchComplete(u64, LoreErrorCode),
    NodeInfo(Box<LoreRevisionTreeNodeInfoEventData>),
    Other(u32),
}

impl CapturedEvent {
    fn from_event(event: &LoreEvent) -> Self {
        match event {
            LoreEvent::Complete(data) => {
                Self::Complete(data.status, data.error.message.as_str().to_string())
            }
            LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
            LoreEvent::RevisionTreeAddComplete(data) => {
                Self::AddComplete(data.entry_id, data.node_id, data.error_code)
            }
            LoreEvent::RevisionTreeModifyComplete(data) => {
                Self::ModifyComplete(data.entry_id, data.node_id, data.error_code)
            }
            LoreEvent::RevisionTreeBatchComplete(data) => {
                Self::BatchComplete(data.batch_id, data.error_code)
            }
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

fn modify_outcome(events: &[CapturedEvent], id: u64) -> Option<(NodeID, LoreErrorCode)> {
    events.iter().find_map(|event| match event {
        CapturedEvent::ModifyComplete(event_id, node_id, error_code) if *event_id == id => {
            Some((*node_id, *error_code))
        }
        _ => None,
    })
}

fn modify_outcomes(events: &[CapturedEvent]) -> Vec<(u64, NodeID, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            CapturedEvent::ModifyComplete(id, node_id, code) => Some((*id, *node_id, *code)),
            _ => None,
        })
        .collect()
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

/// The rejection reason the call completed with, which is the only place the
/// offending entry's batch index and the rule it broke are reported — the
/// per-entry terminal carries an error code alone.
fn rejection_reason(events: &[CapturedEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            CapturedEvent::Complete(_, message) => Some(message.clone()),
            _ => None,
        })
        .expect("the call must complete")
}

fn node_info_event(events: &[CapturedEvent]) -> Option<LoreRevisionTreeNodeInfoEventData> {
    events.iter().find_map(|event| match event {
        CapturedEvent::NodeInfo(data) => Some((**data).clone()),
        _ => None,
    })
}

fn address(hash: u64, context: Context) -> Address {
    Address {
        hash: Hash::from_u64(hash),
        context,
    }
}

fn file_id() -> Context {
    Context::from(uuid::Uuid::now_v7())
}

fn entry(entry_id: u64, node_id: NodeID) -> LoreRevisionTreeModifyEntry {
    LoreRevisionTreeModifyEntry {
        entry_id,
        node_id,
        mode: 0o600,
        size: 4096,
        address: address(2, Context::default()),
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

/// Seed one node of `kind` under the root and return its node id.
async fn seed(
    handle: LoreRevisionTree,
    name: &str,
    kind: LoreNodeType,
    address: Address,
) -> NodeID {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: 1,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
                entry_id: 1,
                parent_node_id: ROOT_NODE,
                parent_entry_index: 0,
                name: LoreString::from_str(name),
                kind: kind as u32,
                mode: 0o644,
                size: 10,
                address,
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    assert_eq!(status, 0, "seeding {name} must succeed");
    let events = sink.lock().unwrap().clone();
    events
        .iter()
        .find_map(|event| match event {
            CapturedEvent::AddComplete(_, node_id, LoreErrorCode::None) => Some(*node_id),
            _ => None,
        })
        .expect("seeding must report a node id")
}

async fn run_modify(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeModifyEntry>,
) -> (i32, Vec<CapturedEvent>) {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = modify(
        LoreGlobalArgs::default(),
        LoreRevisionTreeModifyArgs {
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

async fn fetch_node_info(
    handle: LoreRevisionTree,
    node_id: NodeID,
) -> LoreRevisionTreeNodeInfoEventData {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    node_info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodeInfoArgs {
            id: 7,
            handle,
            node_id,
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    node_info_event(&events).expect("node_info must emit a record")
}

/// The common edit: new content at an existing path. Mode, size and content
/// hash take the new values, the file id stays put because the caller
/// supplied none, and the terminal echoes the node so a caller can chain.
#[tokio::test]
async fn modify_rewrites_a_file_and_keeps_its_file_id() {
    let partition = Partition::from([0x21u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-rewrite", partition).await;

    let original_file_id = file_id();
    let node_id = seed(
        handle,
        "data.bin",
        LoreNodeType::File,
        address(1, original_file_id),
    )
    .await;

    let (status, events) = run_modify(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "modifying a file must succeed");
    assert_eq!(
        modify_outcome(&events, 10),
        Some((node_id, LoreErrorCode::None)),
        "the terminal must echo the modified node"
    );

    let info = fetch_node_info(handle, node_id).await;
    assert_eq!(info.mode, 0o600, "mode must take the new value");
    assert_eq!(info.size, 4096, "size must take the new value");
    assert_eq!(
        info.address.hash,
        Hash::from_u64(2),
        "the supplied content address must cross unchanged"
    );
    assert_eq!(
        info.file_id, original_file_id,
        "a zero context must preserve the file id rather than clear it"
    );
    release(handle, store_handle_id);
}

/// A caller that does supply a file id is recording a different identity for
/// the path, and gets it.
#[tokio::test]
async fn modify_takes_a_supplied_file_id() {
    let partition = Partition::from([0x22u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-file-id", partition).await;

    let node_id = seed(
        handle,
        "data.bin",
        LoreNodeType::File,
        address(1, file_id()),
    )
    .await;
    let replacement = file_id();

    let (status, _) = run_modify(
        handle,
        vec![LoreRevisionTreeModifyEntry {
            address: address(2, replacement),
            ..entry(10, node_id)
        }],
    )
    .await;
    assert_eq!(status, 0, "modifying a file must succeed");

    let info = fetch_node_info(handle, node_id).await;
    assert_eq!(
        info.file_id, replacement,
        "a supplied file id must replace the existing one"
    );
    release(handle, store_handle_id);
}

/// A directory's size and address are derived at commit and a link's address
/// is its target, so neither holds content a caller can rewrite.
#[tokio::test]
async fn modify_rejects_a_directory_and_a_link() {
    let partition = Partition::from([0x23u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-kinds", partition).await;

    let directory = seed(handle, "dir", LoreNodeType::Directory, Address::default()).await;
    let link = seed(handle, "link", LoreNodeType::Link, address(3, file_id())).await;

    for (node_id, kind) in [(directory, "directory"), (link, "link")] {
        let before = fetch_node_info(handle, node_id).await;
        let (status, events) = run_modify(handle, vec![entry(10, node_id)]).await;
        assert_ne!(status, 0, "a {kind} must not be modifiable");
        assert_eq!(
            modify_outcome(&events, 10),
            Some((INVALID_NODE, LoreErrorCode::InvalidArguments)),
            "a {kind} must be rejected as a bad argument"
        );
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("only a file carries content"),
            "a {kind} must be refused for its kind, got {reason:?}"
        );
        let after = fetch_node_info(handle, node_id).await;
        assert_eq!(
            (after.mode, after.size, after.address),
            (before.mode, before.size, before.address),
            "a refused modify must leave the {kind} untouched"
        );
    }
    release(handle, store_handle_id);
}

/// The sentinel names no node, so the tree cannot be read for it at all.
#[tokio::test]
async fn modify_rejects_an_unknown_node() {
    let partition = Partition::from([0x24u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-unknown", partition).await;

    let (status, events) = run_modify(handle, vec![entry(10, INVALID_NODE)]).await;
    assert_ne!(status, 0, "an unknown node must not be modifiable");
    assert_eq!(
        modify_outcome(&events, 10),
        Some((INVALID_NODE, LoreErrorCode::InvalidArguments)),
        "an unknown node must be rejected as a bad argument"
    );
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("entry 0: node id is unknown"),
        "the reason must name the offending entry's batch index, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// A node id inside the tree's blocks but on a slot the allocator never
/// handed out reads back zeroed, which is an ordinary directory. Without its
/// own check it would be refused as the wrong kind, hiding a caller passing a
/// node id it made up.
#[tokio::test]
async fn modify_rejects_a_node_id_on_an_unallocated_slot() {
    let partition = Partition::from([0x2cu8; 16]);
    let (handle, store_handle_id) = load_handle("modify-unallocated", partition).await;

    seed(
        handle,
        "data.bin",
        LoreNodeType::File,
        address(1, file_id()),
    )
    .await;

    let (status, events) = run_modify(handle, vec![entry(10, 400)]).await;
    assert_ne!(status, 0, "an unallocated slot must not be modifiable");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("does not resolve to a named node"),
        "an unallocated slot must be refused as unnamed, not as the wrong kind, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// Discarding clears the file flag, so the kind check would refuse a deleted
/// node too — but as the wrong kind, which sends a caller looking for a type
/// error instead of a node that is gone.
#[tokio::test]
async fn modify_rejects_a_deleted_node() {
    let partition = Partition::from([0x25u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-deleted", partition).await;

    let node_id = seed(
        handle,
        "data.bin",
        LoreNodeType::File,
        address(1, file_id()),
    )
    .await;

    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    let block_index = NodeBlock::index(node_id);
    let block = internal
        .state_for_tests()
        .block(internal.repository_context.clone(), block_index)
        .await
        .expect("the block must be readable");
    block
        .write_for_tests()
        .discard_node(block_index, Node::index(node_id));

    let (status, events) = run_modify(handle, vec![entry(10, node_id)]).await;
    assert_ne!(status, 0, "a deleted node must not be modifiable");
    assert_eq!(
        modify_outcome(&events, 10),
        Some((INVALID_NODE, LoreErrorCode::InvalidArguments)),
        "a deleted node must be rejected as a bad argument"
    );
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("has been deleted"),
        "a deleted node must be refused as deleted, not as the wrong kind, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// Two edits to one node in a single call do not say which value the caller
/// meant, so the batch rejects rather than picking one.
#[tokio::test]
async fn modify_rejects_a_repeated_node_id() {
    let partition = Partition::from([0x26u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-repeat-node", partition).await;

    let node_id = seed(
        handle,
        "data.bin",
        LoreNodeType::File,
        address(1, file_id()),
    )
    .await;
    let before = fetch_node_info(handle, node_id).await;

    let (status, events) = run_modify(handle, vec![entry(10, node_id), entry(11, node_id)]).await;
    assert_ne!(status, 0, "one node named twice must reject the batch");
    assert_eq!(
        modify_outcomes(&events),
        vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
        "only the repeating entry reports; the first was never applied"
    );
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("two entries modify one node"),
        "the repeat must be refused as a repeat, got {reason:?}"
    );

    let after = fetch_node_info(handle, node_id).await;
    assert_eq!(
        (after.mode, after.size, after.address),
        (before.mode, before.size, before.address),
        "a rejected batch must leave every field unchanged"
    );
    release(handle, store_handle_id);
}

/// A repeated non-zero `entry_id` would make a reported id ambiguous, so it
/// rejects; a repeated zero is an explicit opt-out and does not.
#[tokio::test]
async fn modify_rejects_a_repeated_caller_id_but_accepts_repeated_zeros() {
    let partition = Partition::from([0x27u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-repeat-id", partition).await;

    let first = seed(handle, "a.bin", LoreNodeType::File, address(1, file_id())).await;
    let second = seed(handle, "b.bin", LoreNodeType::File, address(1, file_id())).await;

    let (status, events) = run_modify(handle, vec![entry(10, first), entry(10, second)]).await;
    assert_ne!(status, 0, "a repeated non-zero caller id must reject");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("two entries share one caller id"),
        "the repeat must be refused as a shared id, got {reason:?}"
    );

    let (status, events) = run_modify(handle, vec![entry(0, first), entry(0, second)]).await;
    assert_eq!(status, 0, "repeated zero caller ids must be accepted");
    assert_eq!(
        modify_outcomes(&events).len(),
        2,
        "both entries must report under the shared zero id"
    );
    release(handle, store_handle_id);
}

/// Validation runs over the whole batch before anything is written, so a bad
/// entry anywhere in it leaves every target as it was.
#[tokio::test]
async fn modify_rejects_the_whole_batch_and_changes_nothing() {
    let partition = Partition::from([0x28u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-atomic", partition).await;

    let good = seed(handle, "a.bin", LoreNodeType::File, address(1, file_id())).await;
    let before = fetch_node_info(handle, good).await;

    let (status, events) = run_modify(handle, vec![entry(10, good), entry(11, 400)]).await;
    assert_ne!(status, 0, "one bad entry must reject the batch");
    assert_eq!(
        modify_outcomes(&events),
        vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
        "only the offending entry reports; the valid one was never attempted"
    );

    let after = fetch_node_info(handle, good).await;
    assert_eq!(
        (after.mode, after.size, after.address),
        (before.mode, before.size, before.address),
        "the valid entry's target must be untouched"
    );

    let (status, _) = run_modify(handle, vec![entry(12, good)]).await;
    assert_eq!(status, 0, "the handle must stay usable after a rejection");
    release(handle, store_handle_id);
}

/// The batch spreads over several tasks, so this also covers that every entry
/// reports and lands regardless of which task ran it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modify_rewrites_many_files_in_one_call() {
    let partition = Partition::from([0x29u8; 16]);
    let (handle, store_handle_id) = load_handle("modify-many", partition).await;

    let mut nodes = Vec::with_capacity(BATCH_FILES);
    for index in 0..BATCH_FILES {
        nodes.push(
            seed(
                handle,
                &format!("file{index}.bin"),
                LoreNodeType::File,
                address(1, file_id()),
            )
            .await,
        );
    }

    let entries: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(index, node_id)| LoreRevisionTreeModifyEntry {
            size: index as u64,
            address: address(100 + index as u64, Context::default()),
            ..entry(index as u64 + 1, *node_id)
        })
        .collect();

    let (status, events) = run_modify(handle, entries).await;
    assert_eq!(status, 0, "a batch of independent files must succeed");
    let mut reported = modify_outcomes(&events);
    reported.sort_by_key(|(id, _, _)| *id);
    let expected: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(index, node_id)| (index as u64 + 1, *node_id, LoreErrorCode::None))
        .collect();
    assert_eq!(reported, expected, "every entry must report its own node");

    for (index, node_id) in nodes.iter().enumerate() {
        let info = fetch_node_info(handle, *node_id).await;
        assert_eq!(
            (info.size, info.address.hash),
            (index as u64, Hash::from_u64(100 + index as u64)),
            "entry {index} must have landed its own values"
        );
    }
    release(handle, store_handle_id);
}

/// An empty batch is a no-op that still reports the call.
#[tokio::test]
async fn modify_with_no_entries_succeeds() {
    let partition = Partition::from([0x2au8; 16]);
    let (handle, store_handle_id) = load_handle("modify-empty", partition).await;

    let (status, events) = run_modify(handle, Vec::new()).await;
    assert_eq!(status, 0, "an empty batch must succeed");
    assert!(
        modify_outcomes(&events).is_empty(),
        "no entry terminal may fire for an empty batch"
    );
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::None)],
        "the batch terminal must fire exactly once even with nothing to do"
    );
    release(handle, store_handle_id);
}

/// Nothing was looked at, so no entry may be made to look individually
/// rejected; the call reports on the batch terminal alone.
#[tokio::test]
async fn modify_on_unknown_handle_reports_only_the_batch_terminal() {
    let (status, events) =
        run_modify(LoreRevisionTree::INVALID, vec![entry(10, 1), entry(11, 2)]).await;
    assert_ne!(status, 0, "an unknown handle must fail the call");
    assert!(
        modify_outcomes(&events).is_empty(),
        "a handle miss must fire no per-entry terminal"
    );
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
        "the handle miss must be reported once, on the batch terminal"
    );
}

/// A target can pass validation and be gone by the time the batch reaches
/// it: the plan phase reads every node, the apply phase then writes them,
/// and nothing holds the tree still in between. Driving the two phases
/// separately puts that interleaving under the test's control rather than a
/// race's, which is the only way to reach the apply phase's failure path now
/// that validation catches everything the arguments can get wrong. It runs
/// through the real dispatcher rather than a hand-built scope, so the events
/// reach the callback the way they do on any other call.
#[tokio::test]
async fn a_target_deleted_after_validation_fails_the_batch_as_internal() {
    let partition = Partition::from([0x2du8; 16]);
    let (handle, store_handle_id) = load_handle("modify-vanishing", partition).await;
    let node_id = seed(
        handle,
        "data.bin",
        LoreNodeType::File,
        address(1, file_id()),
    )
    .await;

    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = revision_tree_call(
        LoreGlobalArgs::default(),
        make_callback(sink.clone()),
        handle,
        CALL_ID,
        modify,
        |_: &u64| {},
        async move |internal: Arc<RevisionTreeInternal>, call_id: u64| {
            let entries = vec![entry(10, node_id)];
            let planned = plan_entries(
                &internal.state_for_tests(),
                &internal.repository_context,
                &entries,
            )
            .await?;

            let block_index = NodeBlock::index(node_id);
            let block = internal
                .state_for_tests()
                .block(internal.repository_context.clone(), block_index)
                .await
                .expect("the block must be readable");
            block
                .write_for_tests()
                .discard_node(block_index, Node::index(node_id));

            let result = apply_plan(
                internal.claim_shared().await,
                internal.repository_context.clone(),
                planned,
            )
            .await;
            emit_batch_complete(call_id, batch_error_code(&result));
            result
        },
    )
    .await;

    assert_ne!(
        status, 0,
        "a target that vanished after validation must fail the call"
    );
    let events = sink.lock().unwrap().clone();
    assert_eq!(
        modify_outcomes(&events),
        vec![(10, INVALID_NODE, LoreErrorCode::Internal)],
        "the entry must report internal rather than land"
    );
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::Internal)],
        "the batch terminal must carry the failure, not report success"
    );
    release(handle, store_handle_id);
}

/// A target staged for deletion after validation is refused when the batch reaches it, under
/// the lock its rewrite takes: the entry reports internal and the node stays staged for deletion,
/// where a rewrite and its forced staged mark would have taken the deletion off it.
#[tokio::test]
async fn a_target_staged_for_deletion_after_validation_fails_the_batch_as_internal() {
    let partition = Partition::from([0x2eu8; 16]);
    let (handle, store_handle_id) = load_handle("modify-staged-deleted", partition).await;
    let node_id = seed(
        handle,
        "data.bin",
        LoreNodeType::File,
        address(1, file_id()),
    )
    .await;

    let still_deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = still_deleted.clone();
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = revision_tree_call(
        LoreGlobalArgs::default(),
        make_callback(sink.clone()),
        handle,
        CALL_ID,
        modify,
        |_: &u64| {},
        async move |internal: Arc<RevisionTreeInternal>, call_id: u64| {
            let entries = vec![entry(10, node_id)];
            let planned = plan_entries(
                &internal.state_for_tests(),
                &internal.repository_context,
                &entries,
            )
            .await?;

            internal
                .state_for_tests()
                .node_delete(internal.repository_context.clone(), node_id)
                .await
                .expect("the delete stages");

            let result = apply_plan(
                internal.claim_shared().await,
                internal.repository_context.clone(),
                planned,
            )
            .await;
            let node = internal
                .state_for_tests()
                .node(internal.repository_context.clone(), node_id)
                .await
                .expect("the node reads");
            observed.store(
                node.is_staged_delete(),
                std::sync::atomic::Ordering::Relaxed,
            );
            emit_batch_complete(call_id, batch_error_code(&result));
            result
        },
    )
    .await;

    assert_ne!(status, 0, "a target staged for deletion must fail the call");
    let events = sink.lock().unwrap().clone();
    assert_eq!(
        modify_outcomes(&events),
        vec![(10, INVALID_NODE, LoreErrorCode::Internal)],
        "the entry must report internal rather than land"
    );
    assert!(
        still_deleted.load(std::sync::atomic::Ordering::Relaxed),
        "the node stays staged for deletion"
    );
    release(handle, store_handle_id);
}

/// A caller must be able to treat the batch terminal as the end of the call,
/// which only holds if it fires after every entry and before `Complete`.
#[tokio::test]
async fn modify_reports_entries_then_the_batch_terminal_then_complete() {
    let partition = Partition::from([0x2bu8; 16]);
    let (handle, store_handle_id) = load_handle("modify-ordering", partition).await;

    let first = seed(handle, "a.bin", LoreNodeType::File, address(1, file_id())).await;
    let second = seed(handle, "b.bin", LoreNodeType::File, address(1, file_id())).await;

    let (_, events) = run_modify(handle, vec![entry(10, first), entry(11, second)]).await;
    let batch_at = events
        .iter()
        .position(|event| matches!(event, CapturedEvent::BatchComplete(..)))
        .expect("the batch terminal must fire");
    let complete_at = events
        .iter()
        .position(|event| matches!(event, CapturedEvent::Complete(..)))
        .expect("Complete must fire");
    let last_entry_at = events
        .iter()
        .rposition(|event| matches!(event, CapturedEvent::ModifyComplete(..)))
        .expect("both entries must report");
    assert!(
        last_entry_at < batch_at && batch_at < complete_at,
        "order must be entries, then the batch terminal, then Complete: {events:?}"
    );
    release(handle, store_handle_id);
}
