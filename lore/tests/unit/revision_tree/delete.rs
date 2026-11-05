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
use lore::revision_tree::delete::*;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::handle::RevisionTreeInternal;
use lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
use lore::revision_tree::list_children::list_children;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
use lore::revision_tree::modify::modify;
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
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreNodeStagedAction;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;

/// Call-level id every test batch is submitted under, distinct from the
/// per-entry ids so the two cannot be confused in an assertion.
const CALL_ID: u64 = 700;

#[derive(Debug, Clone, PartialEq)]
enum CapturedEvent {
    Complete(i32, String),
    RevisionTreeLoaded(u64),
    AddComplete(u64, NodeID, LoreErrorCode),
    DeleteComplete(u64, u64, LoreErrorCode),
    BatchComplete(u64, LoreErrorCode),
    Child(NodeID, u32, u32),
    NodeInfo(NodeID, u32, u32, Address),
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
            LoreEvent::RevisionTreeDeleteComplete(data) => {
                Self::DeleteComplete(data.entry_id, data.node_count, data.error_code)
            }
            LoreEvent::RevisionTreeBatchComplete(data) => {
                Self::BatchComplete(data.batch_id, data.error_code)
            }
            LoreEvent::RevisionTreeChild(data) => {
                Self::Child(data.node_id, data.kind, data.staged_action)
            }
            LoreEvent::RevisionTreeNodeInfo(data) => {
                Self::NodeInfo(data.node_id, data.kind, data.staged_action, data.address)
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

fn delete_outcomes(events: &[CapturedEvent]) -> Vec<(u64, u64, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            CapturedEvent::DeleteComplete(id, count, code) => Some((*id, *count, *code)),
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

fn file_id() -> Context {
    Context::from(uuid::Uuid::now_v7())
}

fn address(hash: u64, context: Context) -> Address {
    Address {
        hash: Hash::from_u64(hash),
        context,
    }
}

fn entry(entry_id: u64, node_id: NodeID) -> LoreRevisionTreeDeleteEntry {
    LoreRevisionTreeDeleteEntry { entry_id, node_id }
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

/// Run one add batch and return the node id each entry produced, in entry
/// order.
async fn run_add(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeAddEntry>,
) -> (i32, Vec<CapturedEvent>) {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: 1,
            handle,
            entries: LoreArray::from_vec(entries),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    (status, events)
}

fn add_entry(
    entry_id: u64,
    parent: NodeID,
    name: &str,
    kind: LoreNodeType,
) -> LoreRevisionTreeAddEntry {
    LoreRevisionTreeAddEntry {
        entry_id,
        parent_node_id: parent,
        parent_entry_index: 0,
        name: LoreString::from_str(name),
        kind: kind as u32,
        mode: 0o644,
        size: 10,
        address: address(1, file_id()),
    }
}

/// Seed one node under `parent` and return its node id.
async fn seed(handle: LoreRevisionTree, parent: NodeID, name: &str, kind: LoreNodeType) -> NodeID {
    let (status, events) = run_add(handle, vec![add_entry(1, parent, name, kind)]).await;
    assert_eq!(status, 0, "seeding {name} must succeed");
    events
        .iter()
        .find_map(|event| match event {
            CapturedEvent::AddComplete(_, node_id, LoreErrorCode::None) => Some(*node_id),
            _ => None,
        })
        .expect("seeding must report a node id")
}

/// Commit-free stand-in for a node the loaded revision holds: seed it, then
/// clear the staging flags the add left behind, which is what commit does to
/// everything it writes.
async fn settle(handle: LoreRevisionTree, node_ids: &[NodeID]) {
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    for node_id in node_ids {
        let block_index = lore_revision::node::NodeBlock::index(*node_id);
        let block = internal
            .state_for_tests()
            .block(internal.repository_context.clone(), block_index)
            .await
            .expect("the block must be readable");
        let mut writer = block.write_for_tests();
        writer
            .node(lore_revision::node::Node::index(*node_id))
            .clear_all_change_flags();
    }
}

async fn run_delete(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeDeleteEntry>,
) -> (i32, Vec<CapturedEvent>) {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = delete(
        LoreGlobalArgs::default(),
        LoreRevisionTreeDeleteArgs {
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

/// Every child the listing reports for `parent`, as `(node_id,
/// staged_action)`.
async fn children_of(handle: LoreRevisionTree, parent: NodeID) -> Vec<(NodeID, u32)> {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = list_children(
        LoreGlobalArgs::default(),
        LoreRevisionTreeListChildrenArgs {
            id: 5,
            handle,
            parent_node_id: parent,
        },
        make_callback(sink.clone()),
    )
    .await;
    assert_eq!(status, 0, "listing children must succeed");
    let events = sink.lock().unwrap().clone();
    events
        .iter()
        .filter_map(|event| match event {
            CapturedEvent::Child(node_id, _, staged_action) => Some((*node_id, *staged_action)),
            _ => None,
        })
        .collect()
}

/// The common deletion: a subtree the loaded revision holds is staged, every
/// node of it counted, and the nodes stay in the tree carrying the deletion so
/// a caller can see what a commit would drop.
#[tokio::test]
async fn delete_stages_a_subtree_and_counts_every_node() {
    let partition = Partition::from([0x41u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-subtree", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let first = seed(handle, directory, "a.bin", LoreNodeType::File).await;
    let second = seed(handle, directory, "b.bin", LoreNodeType::File).await;
    settle(handle, &[directory, first, second]).await;

    let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
    assert_eq!(status, 0, "deleting a subtree must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 3, LoreErrorCode::None)],
        "the directory and both children must be counted"
    );

    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(directory, LoreNodeStagedAction::Delete as u32)],
        "a staged deletion stays listed, carrying the deletion"
    );
    let listed = children_of(handle, directory).await;
    assert_eq!(
        listed,
        vec![
            (second, LoreNodeStagedAction::Delete as u32),
            (first, LoreNodeStagedAction::Delete as u32),
        ],
        "every child must be staged too, and still listed: {listed:?}"
    );
    release(handle, store_handle_id);
}

/// A node this handle added is in no revision, so there is nothing for a
/// commit to drop: it leaves the tree outright and stops being listed.
#[tokio::test]
async fn delete_discards_a_node_this_handle_added() {
    let partition = Partition::from([0x42u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-added", partition).await;

    let settled = seed(handle, ROOT_NODE, "kept.bin", LoreNodeType::File).await;
    settle(handle, &[settled]).await;
    let added = seed(handle, ROOT_NODE, "fresh.bin", LoreNodeType::File).await;

    let (status, events) = run_delete(handle, vec![entry(10, added)]).await;
    assert_eq!(status, 0, "deleting an added node must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 1, LoreErrorCode::None)],
        "the discarded node must still be counted"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(settled, LoreNodeStagedAction::None as u32)],
        "a discarded node leaves the tree, unlike a staged one"
    );
    release(handle, store_handle_id);
}

/// A subtree of added nodes leaves the tree whole: every node of it is counted,
/// and its root stops being listed.
#[tokio::test]
async fn delete_discards_a_subtree_this_handle_added() {
    let partition = Partition::from([0x43u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-added-subtree", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    seed(handle, directory, "a.bin", LoreNodeType::File).await;
    seed(handle, directory, "b.bin", LoreNodeType::File).await;

    let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
    assert_eq!(status, 0, "deleting an added subtree must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 3, LoreErrorCode::None)],
        "every added node of the subtree must be counted"
    );
    assert!(
        children_of(handle, ROOT_NODE).await.is_empty(),
        "the whole added subtree must be gone from the tree"
    );
    release(handle, store_handle_id);
}

/// A link addresses a revision this handle does not mutate, so it goes as one
/// node and the walk does not descend into the tree it points at.
#[tokio::test]
async fn delete_removes_a_link_without_descending_into_it() {
    let partition = Partition::from([0x44u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-link", partition).await;

    let link = seed(handle, ROOT_NODE, "link", LoreNodeType::Link).await;
    settle(handle, &[link]).await;

    let (status, events) = run_delete(handle, vec![entry(10, link)]).await;
    assert_eq!(status, 0, "deleting a link must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 1, LoreErrorCode::None)],
        "a link counts as the one node it is"
    );
    release(handle, store_handle_id);
}

/// The root is the revision itself; there is no tree left without it.
#[tokio::test]
async fn delete_rejects_the_root() {
    let partition = Partition::from([0x45u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-root", partition).await;

    let (status, events) = run_delete(handle, vec![entry(10, ROOT_NODE)]).await;
    assert_ne!(status, 0, "the root must not be deletable");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 0, LoreErrorCode::InvalidArguments)],
        "the root must be refused as a bad argument, reporting nothing removed"
    );
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("the root is the revision itself"),
        "the root must be refused on its own terms, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// The sentinel names no node, so the tree cannot be read for it at all.
#[tokio::test]
async fn delete_rejects_an_unknown_node() {
    let partition = Partition::from([0x46u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-unknown", partition).await;

    let (status, events) = run_delete(handle, vec![entry(10, INVALID_NODE)]).await;
    assert_ne!(status, 0, "an unknown node must not be deletable");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("entry 0: node id is unknown"),
        "the reason must name the offending entry's batch index, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// A node id inside the tree's blocks but on a slot the allocator never
/// handed out reads back zeroed, which is an ordinary empty directory —
/// deletable-looking, and nothing below would catch it.
#[tokio::test]
async fn delete_rejects_a_node_id_on_an_unallocated_slot() {
    let partition = Partition::from([0x47u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-unallocated", partition).await;

    seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;

    let (status, events) = run_delete(handle, vec![entry(10, 400)]).await;
    assert_ne!(status, 0, "an unallocated slot must not be deletable");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("does not resolve to a named node"),
        "an unallocated slot must be refused as unnamed, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// Deleting twice says nothing new, and letting it through would report a
/// second removal of nodes the first call already took.
#[tokio::test]
async fn delete_rejects_a_node_already_staged_for_deletion() {
    let partition = Partition::from([0x48u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-twice", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;

    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "the first deletion must succeed");

    let (status, events) = run_delete(handle, vec![entry(11, node_id)]).await;
    assert_ne!(status, 0, "the second deletion must reject");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("already staged for deletion"),
        "the repeat must be refused as already staged, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// One subtree named twice in a call would count its nodes twice, and the
/// second pass would find them all staged already.
#[tokio::test]
async fn delete_rejects_two_entries_naming_one_node() {
    let partition = Partition::from([0x49u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-repeat-node", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;

    let (status, events) = run_delete(handle, vec![entry(10, node_id), entry(11, node_id)]).await;
    assert_ne!(status, 0, "one node named twice must reject the batch");
    assert_eq!(
        delete_outcomes(&events),
        vec![(11, 0, LoreErrorCode::InvalidArguments)],
        "only the repeating entry reports; the first was never applied"
    );
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("two entries delete one node"),
        "the repeat must be refused as a repeat, got {reason:?}"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(node_id, LoreNodeStagedAction::None as u32)],
        "a rejected batch must leave the node untouched"
    );
    release(handle, store_handle_id);
}

/// An entry under another entry's subtree is removed by that entry's own
/// recursion, so accepting both would count the same nodes twice.
#[tokio::test]
async fn delete_rejects_an_entry_whose_ancestor_another_entry_deletes() {
    let partition = Partition::from([0x4au8; 16]);
    let (handle, store_handle_id) = load_handle("delete-nested", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let nested = seed(handle, directory, "deep", LoreNodeType::Directory).await;
    let leaf = seed(handle, nested, "a.bin", LoreNodeType::File).await;
    settle(handle, &[directory, nested, leaf]).await;

    let (status, events) = run_delete(handle, vec![entry(10, directory), entry(11, leaf)]).await;
    assert_ne!(status, 0, "an entry under another entry must reject");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("deletes an ancestor of this node"),
        "the descendant must be refused for its ancestor, got {reason:?}"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(directory, LoreNodeStagedAction::None as u32)],
        "a rejected batch must stage nothing"
    );
    release(handle, store_handle_id);
}

/// A repeated non-zero `entry_id` would make a reported id ambiguous, so it
/// rejects; a repeated zero is an explicit opt-out and does not.
#[tokio::test]
async fn delete_rejects_a_repeated_caller_id_but_accepts_repeated_zeros() {
    let partition = Partition::from([0x4bu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-repeat-id", partition).await;

    let first = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    let second = seed(handle, ROOT_NODE, "b.bin", LoreNodeType::File).await;
    settle(handle, &[first, second]).await;

    let (status, events) = run_delete(handle, vec![entry(10, first), entry(10, second)]).await;
    assert_ne!(status, 0, "a repeated non-zero caller id must reject");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("two entries share one caller id"),
        "the repeat must be refused as a shared id, got {reason:?}"
    );

    let (status, events) = run_delete(handle, vec![entry(0, first), entry(0, second)]).await;
    assert_eq!(status, 0, "repeated zero caller ids must be accepted");
    assert_eq!(
        delete_outcomes(&events).len(),
        2,
        "both entries must report under the shared zero id"
    );
    release(handle, store_handle_id);
}

/// Validation runs over the whole batch before anything is touched, so a bad
/// entry anywhere in it leaves every subtree in place.
#[tokio::test]
async fn delete_rejects_the_whole_batch_and_changes_nothing() {
    let partition = Partition::from([0x4cu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-atomic", partition).await;

    let good = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[good]).await;

    let (status, events) = run_delete(handle, vec![entry(10, good), entry(11, 400)]).await;
    assert_ne!(status, 0, "one bad entry must reject the batch");
    assert_eq!(
        delete_outcomes(&events),
        vec![(11, 0, LoreErrorCode::InvalidArguments)],
        "only the offending entry reports; the valid one was never attempted"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(good, LoreNodeStagedAction::None as u32)],
        "the valid entry's target must be untouched"
    );

    let (status, _) = run_delete(handle, vec![entry(12, good)]).await;
    assert_eq!(status, 0, "the handle must stay usable after a rejection");
    release(handle, store_handle_id);
}

/// An empty batch is a no-op that still reports the call.
#[tokio::test]
async fn delete_with_no_entries_succeeds() {
    let partition = Partition::from([0x4du8; 16]);
    let (handle, store_handle_id) = load_handle("delete-empty", partition).await;

    let (status, events) = run_delete(handle, Vec::new()).await;
    assert_eq!(status, 0, "an empty batch must succeed");
    assert!(
        delete_outcomes(&events).is_empty(),
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
async fn delete_on_unknown_handle_reports_only_the_batch_terminal() {
    let (status, events) =
        run_delete(LoreRevisionTree::INVALID, vec![entry(10, 1), entry(11, 2)]).await;
    assert_ne!(status, 0, "an unknown handle must fail the call");
    assert!(
        delete_outcomes(&events).is_empty(),
        "a handle miss must fire no per-entry terminal"
    );
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
        "the handle miss must be reported once, on the batch terminal"
    );
}

/// A caller must be able to treat the batch terminal as the end of the call,
/// which only holds if it fires after every entry and before `Complete`.
#[tokio::test]
async fn delete_reports_entries_then_the_batch_terminal_then_complete() {
    let partition = Partition::from([0x4eu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-ordering", partition).await;

    let first = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    let second = seed(handle, ROOT_NODE, "b.bin", LoreNodeType::File).await;
    settle(handle, &[first, second]).await;

    let (_, events) = run_delete(handle, vec![entry(10, first), entry(11, second)]).await;
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
        .rposition(|event| matches!(event, CapturedEvent::DeleteComplete(..)))
        .expect("both entries must report");
    assert!(
        last_entry_at < batch_at && batch_at < complete_at,
        "order must be entries, then the batch terminal, then Complete: {events:?}"
    );
    release(handle, store_handle_id);
}

/// A block holds `BLOCK_NODE_COUNT` nodes, so every batch under that size
/// sits in block zero and never crosses a boundary. This is the only delete
/// test whose wavefront spans blocks and spreads a level over every task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_stages_more_nodes_than_one_block_holds() {
    let partition = Partition::from([0x4fu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-blocks", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let children = 3 * BLOCK_NODE_COUNT;
    let entries: Vec<_> = (0..children)
        .map(|index| {
            add_entry(
                index as u64 + 1,
                directory,
                &format!("f{index}.bin"),
                LoreNodeType::File,
            )
        })
        .collect();
    let (status, events) = run_add(handle, entries).await;
    assert_eq!(status, 0, "seeding a multi-block directory must succeed");
    let mut settled: Vec<NodeID> = events
        .iter()
        .filter_map(|event| match event {
            CapturedEvent::AddComplete(_, node_id, LoreErrorCode::None) => Some(*node_id),
            _ => None,
        })
        .collect();
    assert_eq!(settled.len(), children, "every child must have been seeded");
    settled.push(directory);
    settle(handle, &settled).await;

    let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
    assert_eq!(status, 0, "deleting a multi-block subtree must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, children as u64 + 1, LoreErrorCode::None)],
        "every node across every block must be staged and counted"
    );
    release(handle, store_handle_id);
}

/// The node's preserved file id, which is the `context` slot of its address.
async fn file_id_of(handle: LoreRevisionTree, node_id: NodeID) -> Context {
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    internal
        .state_for_tests()
        .node(internal.repository_context.clone(), node_id)
        .await
        .expect("the node must be readable")
        .address
        .context
}

/// The staged and dirty state of a node, which every verb has to record as a
/// pair — a staged action without its dirty counterpart leaves the two views
/// of the tree disagreeing.
async fn marks_of(handle: LoreRevisionTree, node_id: NodeID) -> (u32, u16) {
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    let node = internal
        .state_for_tests()
        .node(internal.repository_context.clone(), node_id)
        .await
        .expect("the node must be readable");
    (
        node.staged_action() as u32,
        node.flags & NodeFlags::DirtyBits.bits(),
    )
}

async fn run_modify(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeModifyEntry>,
) -> (i32, Vec<CapturedEvent>) {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = modify(
        LoreGlobalArgs::default(),
        LoreRevisionTreeModifyArgs {
            batch_id: 800,
            handle,
            entries: LoreArray::from_vec(entries),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    (status, events)
}

fn modify_entry(entry_id: u64, node_id: NodeID) -> LoreRevisionTreeModifyEntry {
    LoreRevisionTreeModifyEntry {
        entry_id,
        node_id,
        mode: 0o600,
        size: 99,
        address: address(7, Context::default()),
    }
}

/// Adding a node records the addition on the node and marks its ancestors, so
/// a commit walking down from the root reaches it. The root node itself is
/// never flagged — `node_mark` stops above it and dirties block zero instead.
#[tokio::test]
async fn add_stages_an_addition_and_marks_the_ancestors() {
    let partition = Partition::from([0x51u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-add", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let nested = seed(handle, directory, "deep", LoreNodeType::Directory).await;
    settle(handle, &[directory, nested]).await;
    let node_id = seed(handle, nested, "a.bin", LoreNodeType::File).await;

    assert_eq!(
        marks_of(handle, node_id).await,
        (LoreNodeStagedAction::Add as u32, NodeFlags::DirtyAdd.bits()),
        "an added node must carry both the staged and the dirty addition"
    );
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    for (ancestor, label) in [(nested, "parent"), (directory, "grandparent")] {
        let node = internal
            .state_for_tests()
            .node(internal.repository_context.clone(), ancestor)
            .await
            .expect("the ancestor must be readable");
        assert!(
            node.is_staged() && node.is_dirty(),
            "the {label} must be marked staged and dirty by the add"
        );
    }
    release(handle, store_handle_id);
}

/// A node the loaded revision holds becomes a modification when rewritten.
#[tokio::test]
async fn modify_stages_a_modification_on_a_settled_node() {
    let partition = Partition::from([0x52u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-modify", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;

    let (status, _) = run_modify(handle, vec![modify_entry(10, node_id)]).await;
    assert_eq!(status, 0, "modifying a settled node must succeed");
    assert_eq!(
        marks_of(handle, node_id).await,
        (
            LoreNodeStagedAction::Modify as u32,
            NodeFlags::DirtyModify.bits()
        ),
        "a rewritten settled node must be staged as a modification"
    );
    release(handle, store_handle_id);
}

/// A node this handle added stays an addition however often it is rewritten:
/// it is in no revision, so there is nothing to record a modification
/// against.
#[tokio::test]
async fn modify_keeps_an_added_node_staged_as_an_addition() {
    let partition = Partition::from([0x53u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-add-modify", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;

    let (status, _) = run_modify(handle, vec![modify_entry(10, node_id)]).await;
    assert_eq!(status, 0, "modifying an added node must succeed");
    assert_eq!(
        marks_of(handle, node_id).await,
        (LoreNodeStagedAction::Add as u32, NodeFlags::DirtyAdd.bits()),
        "a rewritten added node must still read as an addition"
    );
    release(handle, store_handle_id);
}

/// Deleting after modifying replaces the modification: the node goes either
/// way, so what was rewritten no longer matters.
#[tokio::test]
async fn delete_replaces_a_modification_with_a_deletion() {
    let partition = Partition::from([0x54u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-modify-delete", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;

    let (status, _) = run_modify(handle, vec![modify_entry(10, node_id)]).await;
    assert_eq!(status, 0, "modifying must succeed");
    let (status, _) = run_delete(handle, vec![entry(11, node_id)]).await;
    assert_eq!(status, 0, "deleting a modified node must succeed");
    assert_eq!(
        marks_of(handle, node_id).await,
        (
            LoreNodeStagedAction::Delete as u32,
            NodeFlags::DirtyDelete.bits()
        ),
        "the deletion must replace the modification"
    );
    release(handle, store_handle_id);
}

/// Adding a node, rewriting it and then deleting it leaves nothing behind:
/// the node was never in a revision, so it is discarded rather than staged.
#[tokio::test]
async fn delete_discards_a_node_that_was_added_then_modified() {
    let partition = Partition::from([0x55u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-add-modify-delete", partition).await;

    let settled = seed(handle, ROOT_NODE, "kept.bin", LoreNodeType::File).await;
    settle(handle, &[settled]).await;
    let node_id = seed(handle, ROOT_NODE, "fresh.bin", LoreNodeType::File).await;

    let (status, _) = run_modify(handle, vec![modify_entry(10, node_id)]).await;
    assert_eq!(status, 0, "modifying must succeed");
    let (status, events) = run_delete(handle, vec![entry(11, node_id)]).await;
    assert_eq!(status, 0, "deleting must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(11, 1, LoreErrorCode::None)],
        "the discarded node must be counted once"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(settled, LoreNodeStagedAction::None as u32)],
        "a node added and then deleted must leave no trace"
    );
    release(handle, store_handle_id);
}

/// Adding the name back restores the node itself, keeping its id and its file
/// id, and stages it as a modification because it is in the loaded revision.
#[tokio::test]
async fn add_restores_a_node_staged_for_deletion_of_the_same_kind() {
    let partition = Partition::from([0x56u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-delete-readd", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;
    let seeded_file_id = file_id_of(handle, node_id).await;
    assert_eq!(
        marks_of(handle, node_id).await.0,
        LoreNodeStagedAction::None as u32,
        "the settled node must start with no staged change"
    );

    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "deleting must succeed");

    let (status, events) = run_add(
        handle,
        vec![LoreRevisionTreeAddEntry {
            address: address(5, Context::default()),
            mode: 0o755,
            size: 77,
            ..add_entry(20, ROOT_NODE, "a.bin", LoreNodeType::File)
        }],
    )
    .await;
    assert_eq!(status, 0, "adding the name back must succeed");
    let restored = events
        .iter()
        .find_map(|event| match event {
            CapturedEvent::AddComplete(20, node_id, LoreErrorCode::None) => Some(*node_id),
            _ => None,
        })
        .expect("the add must report a node id");
    assert_eq!(
        restored, node_id,
        "the restore must return the node that was deleted, not a new one"
    );
    assert_eq!(
        file_id_of(handle, node_id).await,
        seeded_file_id,
        "the restore must keep the node's identity; a generated one would record a move"
    );
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    let held = internal
        .state_for_tests()
        .node(internal.repository_context.clone(), node_id)
        .await
        .expect("the node must be readable");
    assert_eq!(
        (held.mode, held.size, held.address.hash),
        (0o755, 77, address(5, Context::default()).hash),
        "the restore must take the content the entry carries"
    );
    assert_eq!(
        marks_of(handle, node_id).await,
        (
            LoreNodeStagedAction::Modify as u32,
            NodeFlags::DirtyModify.bits()
        ),
        "a restored node is a modification of one the revision holds"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(node_id, LoreNodeStagedAction::Modify as u32)],
        "the parent must hold the one restored child"
    );
    release(handle, store_handle_id);
}

/// Restoring a directory brings back that node and nothing else — a restore
/// cannot know which of the children the caller wants, so each stays staged for
/// deletion until it is added back in turn. This is the most surprising rule on
/// the verb, so it is pinned rather than left to the doc.
#[tokio::test]
async fn restoring_a_directory_leaves_its_children_staged_for_deletion() {
    let partition = Partition::from([0x61u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-readd-directory", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let first = seed(handle, directory, "a.bin", LoreNodeType::File).await;
    let second = seed(handle, directory, "b.bin", LoreNodeType::File).await;
    settle(handle, &[directory, first, second]).await;

    let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
    assert_eq!(status, 0, "deleting the directory must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 3, LoreErrorCode::None)],
        "the directory and both children must be staged"
    );

    let (status, _) = run_add(
        handle,
        vec![add_entry(20, ROOT_NODE, "dir", LoreNodeType::Directory)],
    )
    .await;
    assert_eq!(status, 0, "restoring the directory must succeed");
    assert_eq!(
        marks_of(handle, directory).await.0,
        LoreNodeStagedAction::Modify as u32,
        "the directory itself must come back"
    );

    let mut listed = children_of(handle, directory).await;
    listed.sort_unstable();
    let mut expected = vec![
        (first, LoreNodeStagedAction::Delete as u32),
        (second, LoreNodeStagedAction::Delete as u32),
    ];
    expected.sort_unstable();
    assert_eq!(
        listed, expected,
        "every child must still be on its way out after the parent is restored"
    );

    let (status, _) = run_add(
        handle,
        vec![add_entry(21, directory, "a.bin", LoreNodeType::File)],
    )
    .await;
    assert_eq!(status, 0, "a child must be restorable in its own right");
    assert_eq!(
        marks_of(handle, first).await.0,
        LoreNodeStagedAction::Modify as u32,
        "adding the child back must restore it, not create a second one"
    );
    assert_eq!(
        children_of(handle, directory).await.len(),
        2,
        "restoring a child must not add a duplicate alongside it"
    );
    release(handle, store_handle_id);
}

/// A caller supplying a file id on the restore is recording a new identity for
/// the path deliberately, and gets it — the same asymmetry `modify` has, where
/// only a zero context means "keep what is there".
#[tokio::test]
async fn add_takes_a_supplied_file_id_when_restoring() {
    let partition = Partition::from([0x60u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-readd-file-id", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;
    let seeded_file_id = file_id_of(handle, node_id).await;
    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "deleting must succeed");

    let replacement = file_id();
    let (status, _) = run_add(
        handle,
        vec![LoreRevisionTreeAddEntry {
            address: address(5, replacement),
            ..add_entry(20, ROOT_NODE, "a.bin", LoreNodeType::File)
        }],
    )
    .await;
    assert_eq!(status, 0, "restoring with a new identity must succeed");
    let restored_file_id = file_id_of(handle, node_id).await;
    assert_eq!(
        restored_file_id, replacement,
        "a supplied file id must replace the one the node had"
    );
    assert_ne!(
        restored_file_id, seeded_file_id,
        "the supplied id must not be quietly discarded in favour of the old one"
    );
    release(handle, store_handle_id);
}

/// A deleted namesake of another kind is a replacement, not a restore: the
/// caller gets a new node and the old one stays on its way out.
#[tokio::test]
async fn add_creates_a_new_node_when_the_deleted_namesake_is_another_kind() {
    let partition = Partition::from([0x57u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-delete-retype", partition).await;

    let node_id = seed(handle, ROOT_NODE, "thing", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;
    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "deleting must succeed");

    let (status, events) = run_add(
        handle,
        vec![add_entry(20, ROOT_NODE, "thing", LoreNodeType::Directory)],
    )
    .await;
    assert_eq!(status, 0, "replacing with another kind must succeed");
    let created = events
        .iter()
        .find_map(|event| match event {
            CapturedEvent::AddComplete(20, created, LoreErrorCode::None) => Some(*created),
            _ => None,
        })
        .expect("the add must report a node id");
    assert_ne!(
        created, node_id,
        "a different kind must not restore the deleted node"
    );

    let mut listed = children_of(handle, ROOT_NODE).await;
    listed.sort_unstable();
    let mut expected = vec![
        (node_id, LoreNodeStagedAction::Delete as u32),
        (created, LoreNodeStagedAction::Add as u32),
    ];
    expected.sort_unstable();
    assert_eq!(
        listed, expected,
        "the outgoing node and its replacement must both be listed"
    );
    release(handle, store_handle_id);
}

/// A restored node can be deleted again, and it stages rather than discards:
/// restoring made it a modification of a node the revision still holds.
#[tokio::test]
async fn delete_stages_a_node_that_was_restored_by_add() {
    let partition = Partition::from([0x58u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-delete-readd-delete", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;

    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "the first deletion must succeed");
    let (status, _) = run_add(
        handle,
        vec![add_entry(20, ROOT_NODE, "a.bin", LoreNodeType::File)],
    )
    .await;
    assert_eq!(status, 0, "restoring must succeed");
    let (status, events) = run_delete(handle, vec![entry(30, node_id)]).await;
    assert_eq!(status, 0, "deleting the restored node must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(30, 1, LoreErrorCode::None)],
        "the restored node must be staged, and counted"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(node_id, LoreNodeStagedAction::Delete as u32)],
        "a restored node deletes back to staged, not discarded"
    );
    release(handle, store_handle_id);
}

/// Deleting a node this handle added frees its name, so adding it again is an
/// ordinary addition rather than a restore.
#[tokio::test]
async fn add_after_discarding_an_added_node_creates_a_fresh_node() {
    let partition = Partition::from([0x59u8; 16]);
    let (handle, store_handle_id) = load_handle("mark-add-delete-add", partition).await;

    let first = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    let (status, _) = run_delete(handle, vec![entry(10, first)]).await;
    assert_eq!(status, 0, "deleting the added node must succeed");

    let second = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    assert_eq!(
        marks_of(handle, second).await,
        (LoreNodeStagedAction::Add as u32, NodeFlags::DirtyAdd.bits()),
        "the name must be free again and the node a plain addition"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(second, LoreNodeStagedAction::Add as u32)],
        "only the new node must be listed"
    );
    release(handle, store_handle_id);
}

/// A commit drops a node staged for deletion, so rewriting its content is
/// work the revision throws away; adding the name back restores it first.
#[tokio::test]
async fn modify_rejects_a_node_staged_for_deletion() {
    let partition = Partition::from([0x5au8; 16]);
    let (handle, store_handle_id) = load_handle("mark-delete-modify", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;
    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "deleting must succeed");

    let (status, events) = run_modify(handle, vec![modify_entry(20, node_id)]).await;
    assert_ne!(
        status, 0,
        "a node staged for deletion must not be modifiable"
    );
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("staged for deletion"),
        "the rejection must name the deletion, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// A child added under a node on its way out would go with it, so the add is
/// refused rather than silently amounting to nothing.
#[tokio::test]
async fn add_rejects_a_parent_staged_for_deletion() {
    let partition = Partition::from([0x5bu8; 16]);
    let (handle, store_handle_id) = load_handle("mark-delete-add-child", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    settle(handle, &[directory]).await;
    let (status, _) = run_delete(handle, vec![entry(10, directory)]).await;
    assert_eq!(status, 0, "deleting the directory must succeed");

    let (status, events) = run_add(
        handle,
        vec![add_entry(20, directory, "a.bin", LoreNodeType::File)],
    )
    .await;
    assert_ne!(status, 0, "adding under a deleted parent must reject");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("parent node is staged for deletion"),
        "the rejection must name the parent's deletion, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// A target can pass validation and be gone by the time the batch reaches it.
/// Driving the two phases separately puts that interleaving under the test's
/// control rather than a race's, which is the only way to reach the apply
/// phase's failure path now that validation catches everything the arguments
/// can get wrong. What it pins is the reporting: the entry that hit the fault
/// fails alone, and the entries beside it still report what they removed.
#[tokio::test]
async fn a_target_discarded_after_validation_fails_only_its_own_entry() {
    let partition = Partition::from([0x5du8; 16]);
    let (handle, store_handle_id) = load_handle("delete-vanishing", partition).await;

    let doomed = seed(handle, ROOT_NODE, "doomed.bin", LoreNodeType::File).await;
    let intact = seed(handle, ROOT_NODE, "intact.bin", LoreNodeType::File).await;
    settle(handle, &[doomed, intact]).await;

    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = revision_tree_call(
        LoreGlobalArgs::default(),
        make_callback(sink.clone()),
        handle,
        CALL_ID,
        delete,
        |_: &u64| {},
        async move |internal: Arc<RevisionTreeInternal>, call_id: u64| {
            let entries = vec![entry(10, doomed), entry(11, intact)];
            let planned = plan_entries(
                &internal.state_for_tests(),
                &internal.repository_context,
                &entries,
            )
            .await?;

            let block_index = lore_revision::node::NodeBlock::index(doomed);
            let block = internal
                .state_for_tests()
                .block(internal.repository_context.clone(), block_index)
                .await
                .expect("the block must be readable");
            block
                .write_for_tests()
                .discard_node(block_index, lore_revision::node::Node::index(doomed));

            let result = apply_plan(
                internal.state_for_tests(),
                internal.repository_context.clone(),
                planned,
            )
            .await;
            emit_batch_complete(call_id, batch_error_code(&result));
            result
        },
    )
    .await;

    assert_ne!(status, 0, "a target that vanished must fail the call");
    let events = sink.lock().unwrap().clone();
    assert_eq!(
        delete_outcomes(&events),
        vec![
            (10, 0, LoreErrorCode::Internal),
            (11, 1, LoreErrorCode::None),
        ],
        "the failing entry must not take the untouched one down with it"
    );
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::Internal)],
        "the call as a whole must still report the failure"
    );
    release(handle, store_handle_id);
}

/// A settled directory holding a node this handle added drives both phases of
/// the apply inside a single entry: the settled nodes are staged for deletion
/// and stay listed, the added one leaves the tree, and all three are counted.
#[tokio::test]
async fn delete_stages_the_settled_nodes_of_a_subtree_and_discards_the_added_one() {
    let partition = Partition::from([0x5eu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-mixed", partition).await;

    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let settled_child = seed(handle, directory, "kept.bin", LoreNodeType::File).await;
    settle(handle, &[directory, settled_child]).await;
    seed(handle, directory, "fresh.bin", LoreNodeType::File).await;

    let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
    assert_eq!(status, 0, "deleting a mixed subtree must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 3, LoreErrorCode::None)],
        "the two settled nodes and the discarded one must all count"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(directory, LoreNodeStagedAction::Delete as u32)],
        "the settled directory must stay, staged for deletion"
    );
    assert_eq!(
        children_of(handle, directory).await,
        vec![(settled_child, LoreNodeStagedAction::Delete as u32)],
        "the added child must be gone and the settled one staged"
    );
    release(handle, store_handle_id);
}

/// The per-node record carries the staged change too, not only the listing —
/// a caller holding a node id has to be able to see the deletion without
/// listing its parent.
#[tokio::test]
async fn node_info_reports_a_node_staged_for_deletion() {
    let partition = Partition::from([0x5fu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-node-info", partition).await;

    let node_id = seed(handle, ROOT_NODE, "a.bin", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;

    let before = fetch_node_info(handle, node_id).await;
    assert_eq!(
        before,
        (
            node_id,
            LoreNodeType::File as u32,
            LoreNodeStagedAction::None as u32
        ),
        "a settled node must report no staged change"
    );

    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "deleting must succeed");
    assert_eq!(
        fetch_node_info(handle, node_id).await,
        (
            node_id,
            LoreNodeType::File as u32,
            LoreNodeStagedAction::Delete as u32
        ),
        "the record must carry the deletion while keeping the node's kind"
    );
    release(handle, store_handle_id);
}

/// The queried node's `(node_id, kind, staged_action)`.
async fn fetch_node_info(handle: LoreRevisionTree, node_id: NodeID) -> (NodeID, u32, u32) {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = node_info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodeInfoArgs {
            id: 9,
            handle,
            node_id,
        },
        make_callback(sink.clone()),
    )
    .await;
    assert_eq!(status, 0, "node_info must succeed");
    let events = sink.lock().unwrap().clone();
    events
        .iter()
        .find_map(|event| match event {
            CapturedEvent::NodeInfo(node_id, kind, staged_action, _) => {
                Some((*node_id, *kind, *staged_action))
            }
            _ => None,
        })
        .expect("node_info must emit a record")
}

/// A live child holds its name even when a deleted namesake sits beside it,
/// so the replacement cannot itself be replaced without deleting it first.
#[tokio::test]
async fn add_rejects_a_name_a_live_child_holds_beside_a_deleted_one() {
    let partition = Partition::from([0x5cu8; 16]);
    let (handle, store_handle_id) = load_handle("mark-live-beside-deleted", partition).await;

    let node_id = seed(handle, ROOT_NODE, "thing", LoreNodeType::File).await;
    settle(handle, &[node_id]).await;
    let (status, _) = run_delete(handle, vec![entry(10, node_id)]).await;
    assert_eq!(status, 0, "deleting must succeed");
    let (status, _) = run_add(
        handle,
        vec![add_entry(20, ROOT_NODE, "thing", LoreNodeType::Directory)],
    )
    .await;
    assert_eq!(status, 0, "the replacement must succeed");

    let (status, events) = run_add(
        handle,
        vec![add_entry(30, ROOT_NODE, "thing", LoreNodeType::Directory)],
    )
    .await;
    assert_ne!(status, 0, "a live child must still hold the name");
    let reason = rejection_reason(&events);
    assert!(
        reason.contains("a child with this name already exists"),
        "the rejection must be the ordinary collision, got {reason:?}"
    );
    release(handle, store_handle_id);
}

/// The address `node_info` reports for `node_id`, whose context is the file id.
async fn fetch_node_address(handle: LoreRevisionTree, node_id: NodeID) -> Address {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = node_info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeNodeInfoArgs {
            id: 9,
            handle,
            node_id,
        },
        make_callback(sink.clone()),
    )
    .await;
    assert_eq!(status, 0, "node_info must succeed");
    let events = sink.lock().unwrap().clone();
    events
        .iter()
        .find_map(|event| match event {
            CapturedEvent::NodeInfo(_, _, _, address) => Some(*address),
            _ => None,
        })
        .expect("node_info must emit a record")
}

/// A deleted added directory reads as a freed id does until the commit frees it: gone from its
/// parent's listing, listed itself as empty, and reported a directory with no staged change and
/// no address or file id; so does the file below it, whatever its kind was.
#[tokio::test]
async fn a_deleted_added_directory_reads_as_a_freed_id() {
    let partition = Partition::from([0xd1u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-added-reads-freed", partition).await;

    let kept = seed(handle, ROOT_NODE, "kept.bin", LoreNodeType::File).await;
    settle(handle, &[kept]).await;
    let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let child = seed(handle, directory, "a.bin", LoreNodeType::File).await;
    let (status, events) = run_delete(handle, vec![entry(10, directory)]).await;
    assert_eq!(status, 0, "deleting the added directory must succeed");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 2, LoreErrorCode::None)],
        "the directory and its file must be counted"
    );

    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(kept, LoreNodeStagedAction::None as u32)],
        "the deleted directory leaves its parent's listing"
    );
    for node in [directory, child] {
        assert!(
            children_of(handle, node).await.is_empty(),
            "a deleted node lists as empty"
        );
        assert_eq!(
            fetch_node_info(handle, node).await,
            (
                node,
                LoreNodeType::Directory as u32,
                LoreNodeStagedAction::None as u32
            ),
            "a deleted node reports as a freed id does"
        );
        assert_eq!(
            fetch_node_address(handle, node).await,
            Address::default(),
            "with no address and no file id"
        );
    }
    release(handle, store_handle_id);
}

/// A deleted added link reads as a freed id does, as a deleted added directory does: gone from
/// its parent's listing, listed as empty, and reported a directory with no staged change and
/// no address, though its record keeps its kind for the commit's free.
#[tokio::test]
async fn a_deleted_added_link_reads_as_a_freed_id() {
    let partition = Partition::from([0xd7u8; 16]);
    let (handle, store_handle_id) = load_handle("delete-added-link-reads-freed", partition).await;

    let link = seed(handle, ROOT_NODE, "lib", LoreNodeType::Link).await;
    let (status, events) = run_delete(handle, vec![entry(10, link)]).await;
    assert_eq!(status, 0, "deleting the added link must succeed");
    assert_eq!(delete_outcomes(&events), vec![(10, 1, LoreErrorCode::None)]);

    assert!(
        children_of(handle, ROOT_NODE).await.is_empty(),
        "the deleted link leaves its parent's listing"
    );
    assert!(
        children_of(handle, link).await.is_empty(),
        "and lists as empty"
    );
    assert_eq!(
        fetch_node_info(handle, link).await,
        (
            link,
            LoreNodeType::Directory as u32,
            LoreNodeStagedAction::None as u32
        ),
        "a deleted link reports as a freed id does"
    );
    assert_eq!(fetch_node_address(handle, link).await, Address::default());
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    let record = internal
        .state_for_tests()
        .node(internal.repository_context.clone(), link)
        .await
        .expect("the link's record reads");
    assert!(
        record.is_discarded() && record.is_link(),
        "the record keeps its kind for the commit: {:x}",
        record.flags
    );
    release(handle, store_handle_id);
}

/// What a dropped delete's task holds, for the tests of its claim: a tracked `t/a`, an added
/// `n/c`, and the delete of both queued on the moves permit the test holds, its call dropped.
struct DroppedDelete {
    handle: LoreRevisionTree,
    store_handle_id: u64,
    internal: Arc<RevisionTreeInternal>,
    t: NodeID,
    a: NodeID,
    n: NodeID,
    c: NodeID,
}

impl DroppedDelete {
    async fn start(label: &str, partition: Partition) -> (Self, impl Drop) {
        let (handle, store_handle_id) = load_handle(label, partition).await;
        let t = seed(handle, ROOT_NODE, "t", LoreNodeType::Directory).await;
        let a = seed(handle, t, "a.bin", LoreNodeType::File).await;
        settle(handle, &[t, a]).await;
        let n = seed(handle, ROOT_NODE, "n", LoreNodeType::Directory).await;
        let c = seed(handle, n, "c.bin", LoreNodeType::File).await;
        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        let state = internal.state_for_tests();
        let permit = OwnedPermit::take(state).await;

        #[allow(clippy::disallowed_methods)]
        let call = tokio::spawn(run_delete(handle, vec![entry(10, t), entry(11, n)]));
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert!(
            !call.is_finished(),
            "the delete must wait for the moves permit"
        );
        call.abort();
        assert!(
            call.await.expect_err("the call is dropped").is_cancelled(),
            "the call must be dropped, not finished"
        );
        let dropped = Self {
            handle,
            store_handle_id,
            internal,
            t,
            a,
            n,
            c,
        };
        (dropped, permit)
    }

    /// The delete went whole once its task ran: `t` staged for deletion with `t/a`, and `n`
    /// pending with `n/c`.
    async fn assert_deleted(&self) {
        let state = self.internal.state_for_tests();
        let context = self.internal.repository_context.clone();
        for node in [self.t, self.a] {
            let record = state.node(context.clone(), node).await.expect("reads");
            assert!(record.is_staged_delete(), "{node} is staged for deletion");
        }
        for node in [self.n, self.c] {
            let record = state.node(context.clone(), node).await.expect("reads");
            assert!(record.is_discarded(), "{node} is pending");
        }
    }
}

/// The moves permit, held by a task of its own until dropped, so a test can hold it across
/// awaits without borrowing the State.
struct OwnedPermit {
    release: Option<tokio::sync::oneshot::Sender<()>>,
}

impl OwnedPermit {
    async fn take(state: Arc<lore_revision::state::State>) -> Self {
        let (taken, is_taken) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            let permit = state.moves_permit().await.expect("the moves permit");
            let _ = taken.send(());
            let _ = released.await;
            drop(permit);
        });
        is_taken.await.expect("the permit is taken");
        Self {
            release: Some(release),
        }
    }
}

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

/// A delete whose call is dropped while its task waits for the moves permit still holds the
/// handle's shared claim, so an exclusive claim, a commit's, waits for it; once the permit is
/// free the task runs the whole delete and lets the claim go.
#[tokio::test]
async fn a_dropped_delete_holds_its_claim_until_its_task_ends() {
    let (dropped, permit) =
        DroppedDelete::start("delete-dropped-claim", Partition::from([0xd8u8; 16])).await;

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            dropped.internal.access_exclusive()
        )
        .await
        .is_err(),
        "an exclusive claim must wait for the delete's task"
    );
    drop(permit);
    let exclusive = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        dropped.internal.access_exclusive(),
    )
    .await
    .expect("the delete's task ends and lets its claim go");
    drop(exclusive);
    dropped.assert_deleted().await;
    release(dropped.handle, dropped.store_handle_id);
}

/// A close of the handle waits for a dropped delete's task, which entered the handle on its own
/// account, and the task runs the whole delete before the close completes.
#[tokio::test]
async fn a_close_waits_for_a_dropped_deletes_task() {
    use lore::revision_tree::close::LoreRevisionTreeCloseArgs;
    use lore::revision_tree::close::close;

    let (dropped, permit) =
        DroppedDelete::start("delete-dropped-close", Partition::from([0xd9u8; 16])).await;
    let handle = dropped.handle;
    #[allow(clippy::disallowed_methods)]
    let closing = tokio::spawn(async move {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        close(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCloseArgs { id: 1, handle },
            make_callback(sink),
        )
        .await
    });
    let mut closing = closing;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut closing)
            .await
            .is_err(),
        "the close must wait for the delete's task"
    );
    drop(permit);
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), closing)
        .await
        .expect("the close completes once the task ends")
        .expect("the close's task");
    assert_eq!(status, 0, "the close succeeds");
    dropped.assert_deleted().await;
    storage_handle::unregister(lore::storage::handle::LoreStore {
        handle_id: dropped.store_handle_id,
    });
}

/// Moves `node` under `destination` as `name` in a call of its own, which must succeed.
async fn move_into(handle: LoreRevisionTree, node: NodeID, destination: NodeID, name: &str) {
    use lore::revision_tree::move_node::LoreRevisionTreeMoveArgs;
    use lore::revision_tree::move_node::LoreRevisionTreeMoveEntry;
    use lore::revision_tree::move_node::move_node;

    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = move_node(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMoveArgs {
            batch_id: 3,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeMoveEntry {
                entry_id: 1,
                node_id: node,
                destination_parent_id: destination,
                dst_name: LoreString::from_str(name),
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    assert_eq!(status, 0, "the move into {destination} must succeed");
}

/// A tracked node moved into an added directory and deleted there does not make the directory's
/// subtree mixed: the delete of the directory detaches it with the node, which the commit frees
/// with the rest, and counts it no second time.
#[tokio::test]
async fn a_deleted_tracked_node_below_an_added_directory_goes_with_it() {
    let partition = Partition::from([0xdau8; 16]);
    let (handle, store_handle_id) = load_handle("delete-added-over-deleted", partition).await;
    let q = seed(handle, ROOT_NODE, "q", LoreNodeType::Directory).await;
    let y = seed(handle, q, "y.bin", LoreNodeType::File).await;
    settle(handle, &[q, y]).await;
    let n = seed(handle, ROOT_NODE, "n", LoreNodeType::Directory).await;
    move_into(handle, y, n, "y.bin").await;
    let (status, _) = run_delete(handle, vec![entry(10, y)]).await;
    assert_eq!(status, 0, "the delete of the moved node must succeed");

    let (status, events) = run_delete(handle, vec![entry(11, n)]).await;
    assert_eq!(status, 0, "the added directory is not mixed: {events:?}");
    assert_eq!(
        delete_outcomes(&events),
        vec![(11, 1, LoreErrorCode::None)],
        "the directory goes with the deleted node below it, which its own delete counted"
    );
    for node in [n, y] {
        assert_eq!(
            fetch_node_info(handle, node).await,
            (
                node,
                LoreNodeType::Directory as u32,
                LoreNodeStagedAction::None as u32
            ),
            "{node} reads as a freed id does"
        );
    }
    release(handle, store_handle_id);
}

/// A live tracked node moved into an added directory makes the directory's subtree mixed: its
/// detach would take the node along, so the delete fails the entry with nothing detached, and
/// the directory and the node stay listed.
#[tokio::test]
async fn an_added_directory_holding_a_moved_tracked_node_fails_its_entry() {
    let partition = Partition::from([0xdfu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-mixed-added", partition).await;
    let y = seed(handle, ROOT_NODE, "y.bin", LoreNodeType::File).await;
    settle(handle, &[y]).await;
    let n = seed(handle, ROOT_NODE, "n", LoreNodeType::Directory).await;
    move_into(handle, y, n, "y.bin").await;

    let (status, events) = run_delete(handle, vec![entry(10, n)]).await;
    assert_ne!(status, 0, "the mixed subtree fails the call: {events:?}");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 0, LoreErrorCode::Internal)],
        "the entry fails, reporting nothing removed"
    );
    assert_eq!(
        batch_outcomes(&events),
        vec![(CALL_ID, LoreErrorCode::Internal)],
        "the call reports the failure for the batch"
    );
    assert_eq!(
        children_of(handle, ROOT_NODE).await,
        vec![(n, LoreNodeStagedAction::Add as u32)],
        "the added directory is not detached"
    );
    assert_eq!(
        children_of(handle, n).await,
        vec![(y, LoreNodeStagedAction::Move as u32)],
        "the moved node stays below it"
    );
    release(handle, store_handle_id);
}

/// Plans `entries`, runs `between` on the tree, then applies the plan, all inside one call, as
/// [`a_target_discarded_after_validation_fails_only_its_own_entry`] does; answers the call's
/// status and events.
async fn plan_change_apply(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeDeleteEntry>,
    between: impl AsyncFnOnce(
        Arc<lore_revision::state::State>,
        Arc<lore_revision::repository::RepositoryContext>,
    ) + Send
    + 'static,
) -> (i32, Vec<CapturedEvent>) {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = revision_tree_call(
        LoreGlobalArgs::default(),
        make_callback(sink.clone()),
        handle,
        CALL_ID,
        delete,
        |_: &u64| {},
        async move |internal: Arc<RevisionTreeInternal>, call_id: u64| {
            let state = internal.state_for_tests();
            let planned = plan_entries(&state, &internal.repository_context, &entries).await?;
            between(state.clone(), internal.repository_context.clone()).await;
            let result = apply_plan(state, internal.repository_context.clone(), planned).await;
            emit_batch_complete(call_id, batch_error_code(&result));
            result
        },
    )
    .await;
    let events = sink.lock().unwrap().clone();
    (status, events)
}

/// An entry whose marks of the directories above it fail, here at an ancestor whose parent link
/// names a block the tree does not have, fails alone, its node unmarked, as the climb comes first
/// (H8), and an unfinished change noted for the directories the climb staged before it failed;
/// the entry beside it reports what it removed.
#[tokio::test]
async fn an_entry_whose_ancestors_cannot_be_marked_fails_alone() {
    let partition = Partition::from([0xdbu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-unclimbed", partition).await;
    let t = seed(handle, ROOT_NODE, "t", LoreNodeType::Directory).await;
    let sub = seed(handle, t, "sub", LoreNodeType::Directory).await;
    let x = seed(handle, sub, "x.bin", LoreNodeType::File).await;
    let intact = seed(handle, ROOT_NODE, "intact.bin", LoreNodeType::File).await;
    settle(handle, &[t, sub, x, intact]).await;

    let (status, events) = plan_change_apply(
        handle,
        vec![entry(10, x), entry(11, intact)],
        async move |state, repository| {
            let block = state
                .block(repository, lore_revision::node::NodeBlock::index(t))
                .await
                .expect("t's block");
            block
                .write_for_tests()
                .node(lore_revision::node::Node::index(t))
                .parent = 0x00ff_ff00;
        },
    )
    .await;

    assert_ne!(status, 0, "the failed climb fails the call");
    assert_eq!(
        delete_outcomes(&events),
        vec![
            (10, 0, LoreErrorCode::Internal),
            (11, 1, LoreErrorCode::None)
        ],
        "the entry whose climb failed fails alone"
    );
    let internal = rt_handle::lookup(handle).expect("the handle must resolve");
    let record = internal
        .state_for_tests()
        .node(internal.repository_context.clone(), x)
        .await
        .expect("x reads");
    assert!(
        !record.is_staged_delete(),
        "x is not marked after its climb failed"
    );
    assert!(
        internal.state_for_tests().has_unfinished_change(),
        "the climb that failed after marking sub is noted"
    );
    release(handle, store_handle_id);
}

/// An added root no longer where the plan found it when the delete reaches it, here detached
/// meanwhile, fails its entry alone; the entry beside it reports what it removed.
#[tokio::test]
async fn an_added_root_gone_from_its_place_fails_its_entry_alone() {
    let partition = Partition::from([0xdcu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-root-gone", partition).await;
    let intact = seed(handle, ROOT_NODE, "intact.bin", LoreNodeType::File).await;
    settle(handle, &[intact]).await;
    let n = seed(handle, ROOT_NODE, "n", LoreNodeType::Directory).await;
    seed(handle, n, "c.bin", LoreNodeType::File).await;

    let (status, events) = plan_change_apply(
        handle,
        vec![entry(10, n), entry(11, intact)],
        async move |state, repository| {
            assert!(
                state
                    .node_detach(repository, n, lore_revision::state::Detach::Always)
                    .await
                    .expect("the detach"),
                "n is detached"
            );
        },
    )
    .await;

    assert_ne!(status, 0, "the root that left fails the call");
    assert_eq!(
        delete_outcomes(&events),
        vec![
            (10, 0, LoreErrorCode::Internal),
            (11, 1, LoreErrorCode::None)
        ],
        "the entry whose root left fails alone"
    );
    release(handle, store_handle_id);
}

/// Added roots below two directories are detached in groups, one per directory, which may run
/// side by side, as the tracked children each level finds are gathered from groups that may too:
/// each entry counts its own, and every added node reads as freed. The groups run as tasks only
/// with two processors or more; on one, both run in place.
#[tokio::test]
async fn added_roots_below_two_directories_are_detached_per_entry() {
    let partition = Partition::from([0xddu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-two-groups", partition).await;
    let t = seed(handle, ROOT_NODE, "t", LoreNodeType::Directory).await;
    let ts = seed(handle, t, "ts", LoreNodeType::Directory).await;
    let tf = seed(handle, ts, "f.bin", LoreNodeType::File).await;
    let q = seed(handle, ROOT_NODE, "q", LoreNodeType::Directory).await;
    let qs = seed(handle, q, "qs", LoreNodeType::Directory).await;
    let qg = seed(handle, qs, "g.bin", LoreNodeType::File).await;
    settle(handle, &[t, ts, tf, q, qs, qg]).await;
    let x = seed(handle, t, "x", LoreNodeType::Directory).await;
    let xc = seed(handle, x, "c.bin", LoreNodeType::File).await;
    let z = seed(handle, q, "z.bin", LoreNodeType::File).await;

    let (status, events) = run_delete(handle, vec![entry(10, t), entry(11, q)]).await;

    assert_eq!(status, 0, "both deletes succeed: {events:?}");
    assert_eq!(
        delete_outcomes(&events),
        vec![(10, 5, LoreErrorCode::None), (11, 4, LoreErrorCode::None)],
        "each entry counts its tracked nodes and the added subtree below it"
    );
    for node in [x, xc, z] {
        assert_eq!(
            fetch_node_info(handle, node).await,
            (
                node,
                LoreNodeType::Directory as u32,
                LoreNodeStagedAction::None as u32
            ),
            "{node} reads as a freed id does"
        );
    }
    release(handle, store_handle_id);
}

/// An added root still live but no longer where the plan found it, here under another name,
/// fails its entry alone: its detach finds no node at the place it names.
#[tokio::test]
async fn an_added_root_moved_from_its_place_fails_its_entry_alone() {
    let partition = Partition::from([0xdeu8; 16]);
    let (handle, store_handle_id) = load_handle("delete-root-moved", partition).await;
    let intact = seed(handle, ROOT_NODE, "intact.bin", LoreNodeType::File).await;
    settle(handle, &[intact]).await;
    let n = seed(handle, ROOT_NODE, "n", LoreNodeType::Directory).await;

    let (status, events) = plan_change_apply(
        handle,
        vec![entry(10, n), entry(11, intact)],
        async move |state, repository| {
            let block = state
                .block(repository, lore_revision::node::NodeBlock::index(n))
                .await
                .expect("n's block");
            block
                .write_for_tests()
                .node(lore_revision::node::Node::index(n))
                .name_hash ^= 1;
        },
    )
    .await;

    assert_ne!(status, 0, "the root that moved fails the call");
    assert_eq!(
        delete_outcomes(&events),
        vec![
            (10, 0, LoreErrorCode::Internal),
            (11, 1, LoreErrorCode::None)
        ],
        "the entry whose root moved fails alone"
    );
    release(handle, store_handle_id);
}
