// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering as AtomicOrdering;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::add::add;
use lore::revision_tree::commit::*;
use lore::revision_tree::delete::LoreRevisionTreeDeleteArgs;
use lore::revision_tree::delete::LoreRevisionTreeDeleteEntry;
use lore::revision_tree::delete::delete;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
use lore::revision_tree::list_children::list_children;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
use lore::revision_tree::metadata_set::metadata_set;
use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
use lore::revision_tree::modify::modify;
use lore::storage::handle as storage_handle;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::InvalidArguments;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreNodeStagedAction;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::metadata::BRANCH;
use lore_revision::metadata::MESSAGE;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

#[derive(Debug, Clone, PartialEq)]
enum Captured {
    Complete(i32),
    Loaded(u64),
    AddComplete(u64, NodeID, LoreErrorCode),
    DeleteComplete(u64, u64, LoreErrorCode),
    /// A listed child: its id, staged action, name and kind.
    Child(NodeID, u32, String, u32),
    CommitComplete(u64, Hash, Hash, LoreErrorCode),
    CommitRevision(Hash, Hash, BranchId, u64),
    CommitBegin,
    CommitProgress,
    CommitEnd,
    Other(u32),
}

impl Captured {
    fn from_event(event: &LoreEvent) -> Self {
        match event {
            LoreEvent::Complete(data) => Self::Complete(data.status),
            LoreEvent::RevisionTreeLoaded(data) => Self::Loaded(data.handle_id),
            LoreEvent::RevisionTreeAddComplete(data) => {
                Self::AddComplete(data.entry_id, data.node_id, data.error_code)
            }
            LoreEvent::RevisionTreeDeleteComplete(data) => {
                Self::DeleteComplete(data.entry_id, data.node_count, data.error_code)
            }
            LoreEvent::RevisionTreeChild(data) => Self::Child(
                data.node_id,
                data.staged_action,
                data.name.as_str().to_string(),
                data.kind,
            ),
            LoreEvent::RevisionTreeCommitComplete(data) => Self::CommitComplete(
                data.id,
                data.revision_hash,
                data.new_tip_hash,
                data.error_code,
            ),
            LoreEvent::RevisionCommitRevision(data) => Self::CommitRevision(
                data.revision,
                data.parent,
                data.branch,
                data.revision_number,
            ),
            LoreEvent::RevisionCommitBegin(_) => Self::CommitBegin,
            LoreEvent::RevisionCommitProgress(_) => Self::CommitProgress,
            LoreEvent::RevisionCommitEnd(_) => Self::CommitEnd,
            other => Self::Other(other.discriminant()),
        }
    }
}

type Sink = Arc<Mutex<Vec<Captured>>>;

fn make_sink() -> Sink {
    Arc::new(Mutex::new(Vec::new()))
}

fn make_callback(sink: Sink) -> LoreEventCallback {
    Some(Box::new(move |event: &LoreEvent| {
        sink.lock().unwrap().push(Captured::from_event(event));
    }))
}

/// One partition per test. The in-memory store fixtures are process-global, so
/// two tests sharing a partition race each other's branch pointers and tree
/// blocks — which is what the rest of the namespace's tests avoid the same way.
async fn load_handle(label: &str, repository: Partition) -> (LoreRevisionTree, u64) {
    let store = in_memory_for_tests(label).await;
    let store_handle = storage_handle::register(store);
    let handle = load_on(store_handle.handle_id, repository).await;
    (handle, store_handle.handle_id)
}

/// Load another revision tree against an already-open storage handle, so two
/// handles share one store and one branch.
async fn load_on(store_handle_id: u64, repository: Partition) -> LoreRevisionTree {
    let sink = make_sink();
    let status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store: lore::storage::handle::LoreStore {
                handle_id: store_handle_id,
            },
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
            Captured::Loaded(id) => Some(*id),
            _ => None,
        })
        .expect("load fixture must emit RevisionTreeLoaded");
    LoreRevisionTree { handle_id: id }
}

fn release(handle: LoreRevisionTree, store_handle_id: u64) {
    rt_handle::unregister(handle);
    storage_handle::unregister(lore::storage::handle::LoreStore {
        handle_id: store_handle_id,
    });
}

fn handle_state(handle: LoreRevisionTree) -> Arc<State> {
    rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered")
        .state_for_tests()
}

fn pending_metadata_keys(handle: LoreRevisionTree) -> usize {
    let entry = rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered");
    let pending = entry.pending_metadata.read();
    let mut count = 0usize;
    pending.walk(|_, _, _| count += 1);
    count
}

fn is_poisoned(handle: LoreRevisionTree) -> bool {
    rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered")
        .invalid
        .load(AtomicOrdering::Acquire)
}

/// Add one file under the root, returning its node id. `content_hash` of zero
/// produces the node `rehash_directory` refuses, which is the reachable post-freeze
/// failure.
async fn add_file(
    handle: LoreRevisionTree,
    entry_id: u64,
    name: &str,
    content_hash: u64,
) -> NodeID {
    let sink = make_sink();
    let status = add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: 900 + entry_id,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
                entry_id,
                parent_node_id: ROOT_NODE,
                parent_entry_index: 0,
                name: LoreString::from_str(name),
                kind: LoreNodeType::File as u32,
                mode: 0o644,
                size: 12,
                address: Address {
                    hash: Hash::from_u64(content_hash),
                    context: Context::from(uuid::Uuid::now_v7()),
                },
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    assert_eq!(status, 0, "adding {name} must succeed, got {events:?}");
    events
        .iter()
        .find_map(|event| match event {
            Captured::AddComplete(id, node_id, _) if *id == entry_id => Some(*node_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("adding {name} must report a node id, got {events:?}"))
}

/// Give a leaf a content address, so a tree the rehash refused can be corrected.
async fn modify_file(handle: LoreRevisionTree, entry_id: u64, node_id: NodeID, content_hash: u64) {
    let sink = make_sink();
    let status = modify(
        LoreGlobalArgs::default(),
        LoreRevisionTreeModifyArgs {
            batch_id: 700 + entry_id,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeModifyEntry {
                entry_id,
                node_id,
                mode: 0o644,
                size: 12,
                address: Address {
                    hash: Hash::from_u64(content_hash),
                    context: Context::default(),
                },
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    assert_eq!(status, 0, "modifying the node must succeed, got {events:?}");
}

async fn set_message(handle: LoreRevisionTree, message: &str) {
    let sink = make_sink();
    let status = metadata_set(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMetadataSetArgs {
            batch_id: 810,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                entry_id: 2,
                key: LoreString::from_str(MESSAGE),
                value: LoreMetadata::String(LoreString::from_str(message)),
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    assert_eq!(
        status, 0,
        "setting the message must succeed, got {events:?}"
    );
}

async fn set_branch(handle: LoreRevisionTree, branch: BranchId) {
    let sink = make_sink();
    let status = metadata_set(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMetadataSetArgs {
            batch_id: 800,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                entry_id: 1,
                key: LoreString::from_str(BRANCH),
                value: LoreMetadata::Context(branch),
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    assert_eq!(status, 0, "setting the branch must succeed, got {events:?}");
}

async fn run_commit(handle: LoreRevisionTree, id: u64) -> (i32, Vec<Captured>) {
    run_commit_with(
        handle,
        id,
        LoreGlobalArgs::default(),
        LoreRevisionTreeCommitOptions::default(),
    )
    .await
}

/// Commit with chosen globals and options. `resolve_upload` reads the globals
/// from the execution context, which `revision_tree_call` installs from the
/// ones passed here.
async fn run_commit_with(
    handle: LoreRevisionTree,
    id: u64,
    globals: LoreGlobalArgs,
    options: LoreRevisionTreeCommitOptions,
) -> (i32, Vec<Captured>) {
    let sink = make_sink();
    let status = commit(
        globals,
        LoreRevisionTreeCommitArgs {
            id,
            handle,
            options,
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    (status, events)
}

/// Whether the handle's context will upload what it writes. Defaults to
/// disabled, so only a commit that resolved an upload clears it.
fn upload_disabled(handle: LoreRevisionTree) -> bool {
    rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered")
        .repository_context
        .disable_upload()
}

fn commit_outcome(events: &[Captured], id: u64) -> (Hash, Hash, LoreErrorCode) {
    events
        .iter()
        .find_map(|event| match event {
            Captured::CommitComplete(event_id, revision, new_tip, code) if *event_id == id => {
                Some((*revision, *new_tip, *code))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("CommitComplete must fire for {id}, got {events:?}"))
}

#[tokio::test]
async fn commit_publishes_the_tree_and_reports_the_revision() {
    let (handle, store_handle_id) =
        load_handle("commit-publish", Partition::from([0x41u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0x11).await;
    set_branch(handle, branch).await;

    let (status, events) = run_commit(handle, 5).await;

    assert_eq!(status, 0, "committing must succeed, got {events:?}");
    let (revision, new_tip, code) = commit_outcome(&events, 5);
    assert_eq!(code, LoreErrorCode::None, "got {events:?}");
    assert!(!revision.is_zero(), "got {events:?}");
    assert!(
        new_tip.is_zero(),
        "a successful commit reports no advanced tip, got {events:?}"
    );
    assert_eq!(
        handle_state(handle).revision(),
        revision,
        "the handle must be left on the revision it published"
    );
    assert!(
        events.contains(&Captured::CommitRevision(
            revision,
            Hash::default(),
            branch,
            1
        )),
        "the revision event file-system consumers watch must fire, got {events:?}"
    );
    let commit_pos = events
        .iter()
        .position(|event| matches!(event, Captured::CommitComplete(..)))
        .expect("CommitComplete must fire");
    let complete_pos = events
        .iter()
        .position(|event| matches!(event, Captured::Complete(_)))
        .expect("Complete must fire");
    assert!(
        commit_pos < complete_pos,
        "CommitComplete must fire before Complete, got {events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn commit_on_unknown_handle_emits_commit_complete_with_invalid_arguments() {
    let (status, events) = run_commit(LoreRevisionTree::INVALID, 6).await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "committing an unknown handle must fail"
    );
    let (revision, new_tip, code) = commit_outcome(&events, 6);
    assert_eq!(code, LoreErrorCode::InvalidArguments, "got {events:?}");
    assert!(revision.is_zero() && new_tip.is_zero(), "got {events:?}");
}

/// A handle nobody edited is a caller mistake, not a corrupted tree, so the
/// handle survives it and a following commit can succeed.
///
/// Setting the branch is itself a metadata edit, so the first commit clears
/// that and the second is the one running against a genuinely empty handle.
#[tokio::test]
async fn commit_with_no_edits_leaves_the_handle_usable() {
    let (handle, store_handle_id) =
        load_handle("commit-no-edits", Partition::from([0x42u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    set_branch(handle, branch).await;
    let (first_status, _) = run_commit(handle, 7).await;
    assert_eq!(first_status, 0, "the metadata-only revision must commit");

    let (status, events) = run_commit(handle, 8).await;

    assert_eq!(
        status, -1,
        "an unedited handle must not commit, got {events:?}"
    );
    let (revision, new_tip, code) = commit_outcome(&events, 8);
    assert_eq!(code, LoreErrorCode::Internal, "got {events:?}");
    assert!(revision.is_zero() && new_tip.is_zero(), "got {events:?}");
    assert!(
        !is_poisoned(handle),
        "a rejection before any write must not poison the handle"
    );

    add_file(handle, 2, "a.bin", 0x22).await;
    let (retry_status, retry_events) = run_commit(handle, 9).await;
    assert_eq!(
        retry_status, 0,
        "the handle must still commit after a no-op, got {retry_events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn commit_without_a_branch_on_an_empty_handle_is_rejected() {
    let (handle, store_handle_id) =
        load_handle("commit-no-branch", Partition::from([0x43u8; 16])).await;
    add_file(handle, 1, "a.bin", 0x33).await;

    let (status, events) = run_commit(handle, 10).await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an initial revision needs a branch, got {events:?}"
    );
    let (_revision, _new_tip, code) = commit_outcome(&events, 10);
    assert_eq!(code, LoreErrorCode::InvalidArguments, "got {events:?}");
    assert!(
        !is_poisoned(handle),
        "a rejected argument must not poison the handle"
    );

    release(handle, store_handle_id);
}

/// The atomicity contract, on the one failure that reaches past the freeze: a file
/// whose content address is zero gets past the validator, which checks no
/// addresses, and the rehash refuses it once flags have been cleared and nodes
/// discarded. The handle must come back on the revision it was on, dirty, with the
/// edits still staged and still committable — not poisoned.
///
/// The handle commits once first so the restored revision is a real one rather than
/// the zero hash an empty handle would report either way.
#[tokio::test]
async fn a_commit_the_rehash_refuses_restores_the_pre_commit_state() {
    let (handle, store_handle_id) =
        load_handle("commit-restore", Partition::from([0x44u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0x11).await;
    set_branch(handle, branch).await;
    let (first_status, first_events) = run_commit(handle, 11).await;
    assert_eq!(
        first_status, 0,
        "the first revision must commit, got {first_events:?}"
    );
    let (published, _, _) = commit_outcome(&first_events, 11);

    let node_id = add_file(handle, 2, "b.bin", 0).await;
    set_message(handle, "the message must survive").await;
    let (status, events) = run_commit(handle, 12).await;

    assert_eq!(
        status, -1,
        "a tree with a zero content hash must not commit, got {events:?}"
    );
    let (revision, _new_tip, code) = commit_outcome(&events, 12);
    assert_eq!(code, LoreErrorCode::Internal, "got {events:?}");
    assert!(revision.is_zero(), "got {events:?}");
    assert!(
        !is_poisoned(handle),
        "a restored handle must not be poisoned"
    );

    let state = handle_state(handle);
    assert_eq!(
        state.revision(),
        published,
        "the restored state must sit on the revision the handle was on"
    );
    assert!(
        state.is_dirty(),
        "the restored state carries unserialized edits again"
    );
    assert!(
        !state.has_unfinished_change(),
        "the restored state notes no unfinished change the state it replaces did not"
    );
    assert_eq!(
        pending_metadata_keys(handle),
        1,
        "a failed commit must not consume the pending metadata"
    );

    modify_file(handle, 3, node_id, 0x22).await;
    let (retry_status, retry_events) = run_commit(handle, 13).await;
    assert_eq!(
        retry_status, 0,
        "the restored tree must commit once corrected, got {retry_events:?}"
    );
    let (second, _, _) = commit_outcome(&retry_events, 13);
    assert!(
        retry_events.contains(&Captured::CommitRevision(second, published, branch, 2)),
        "the retry must chain onto the revision the restore came back to, got {retry_events:?}"
    );

    release(handle, store_handle_id);
}

/// A metadata edit must not land inside a commit: the commit clones the pending
/// metadata and empties it on success, so an edit arriving between those two points
/// would be recorded on the handle and then dropped without reaching any revision.
/// The claim `metadata_set` takes is what makes that impossible, and this pins it at
/// the lock, since a race cannot be scheduled deterministically.
#[tokio::test]
async fn a_metadata_edit_cannot_land_inside_a_commit() {
    let (handle, store_handle_id) =
        load_handle("commit-metadata-exclusion", Partition::from([0x4du8; 16])).await;
    let internal = rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered")
        .clone();

    let commit_claim = internal.access_exclusive().await;
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        set_message(handle, "must wait for the commit"),
    )
    .await;
    assert!(
        blocked.is_err(),
        "a metadata edit must not run while a commit holds the handle"
    );
    drop(commit_claim);

    set_message(handle, "lands once the commit is done").await;
    assert_eq!(
        pending_metadata_keys(handle),
        1,
        "the edit must land once the handle is free"
    );

    release(handle, store_handle_id);
}

/// The exclusion the atomicity rests on, at the lock rather than through a race:
/// while a commit holds the handle no other call can take it, and every call can
/// again once it lets go.
#[tokio::test]
async fn a_commit_holds_the_handle_against_every_other_call() {
    let (handle, store_handle_id) =
        load_handle("commit-exclusive", Partition::from([0x4cu8; 16])).await;
    let internal = rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered")
        .clone();

    let commit_claim = internal.access_exclusive().await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            internal.access_shared()
        )
        .await
        .is_err(),
        "a commit holding the handle must block every other call"
    );
    drop(commit_claim);

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            internal.access_shared()
        )
        .await
        .is_ok(),
        "the handle must be usable again once the commit lets go"
    );

    release(handle, store_handle_id);
}

/// Two handles on one store racing the same branch: the loser is told which tip
/// to reload from rather than having to ask.
#[tokio::test]
async fn commit_reports_the_new_tip_when_the_branch_advanced() {
    let (winner, store_handle_id) =
        load_handle("commit-advanced", Partition::from([0x45u8; 16])).await;
    let loser = load_on(store_handle_id, Partition::from([0x45u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());

    add_file(winner, 1, "a.bin", 0x44).await;
    set_branch(winner, branch).await;
    let (winner_status, winner_events) = run_commit(winner, 13).await;
    assert_eq!(
        winner_status, 0,
        "the first commit must publish, got {winner_events:?}"
    );
    let (published, _, _) = commit_outcome(&winner_events, 13);

    add_file(loser, 2, "b.bin", 0x45).await;
    set_branch(loser, branch).await;
    let (status, events) = run_commit(loser, 14).await;

    assert_eq!(status, -1, "a branch that moved must reject the commit");
    let (revision, new_tip, code) = commit_outcome(&events, 14);
    assert_eq!(code, LoreErrorCode::Internal, "got {events:?}");
    assert!(revision.is_zero(), "got {events:?}");
    assert_eq!(
        new_tip, published,
        "the terminal must carry the tip to reload from, got {events:?}"
    );
    assert!(
        !is_poisoned(loser),
        "a tip collision is caught before any write, so the handle survives"
    );

    rt_handle::unregister(loser);
    release(winner, store_handle_id);
}

/// Pending metadata is per revision: the second commit must not re-record the
/// first's keys.
#[tokio::test]
async fn commit_empties_the_pending_metadata() {
    let (handle, store_handle_id) =
        load_handle("commit-metadata-reset", Partition::from([0x46u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0x55).await;
    set_branch(handle, branch).await;

    let (status, events) = run_commit(handle, 14).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    let pending_keys = {
        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let pending = entry.pending_metadata.read();
        let mut count = 0usize;
        pending.walk(|_, _, _| count += 1);
        count
    };
    assert_eq!(
        pending_keys, 0,
        "a successful commit must leave the next revision's metadata empty"
    );

    release(handle, store_handle_id);
}

/// The branch key was consumed by the first commit, so the second has to derive
/// the branch from the parent revision — which is the whole point of resolving it
/// from the tree rather than taking it as an argument.
#[tokio::test]
async fn a_second_commit_chains_onto_the_first_without_restating_the_branch() {
    let (handle, store_handle_id) =
        load_handle("commit-chain", Partition::from([0x47u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0x66).await;
    set_branch(handle, branch).await;
    let (first_status, first_events) = run_commit(handle, 15).await;
    assert_eq!(
        first_status, 0,
        "the first revision must commit, got {first_events:?}"
    );
    let (first, _, _) = commit_outcome(&first_events, 15);

    add_file(handle, 2, "b.bin", 0x77).await;
    let (status, events) = run_commit(handle, 16).await;

    assert_eq!(status, 0, "the second revision must commit, got {events:?}");
    let (second, _, code) = commit_outcome(&events, 16);
    assert_eq!(code, LoreErrorCode::None, "got {events:?}");
    assert!(
        events.contains(&Captured::CommitRevision(second, first, branch, 2)),
        "the second revision must record the first as its parent on the same branch, \
             got {events:?}"
    );

    release(handle, store_handle_id);
}

/// Per-call flags that contradict each other are refused before the commit
/// reads the tree, and the caller learns which call failed from the terminal
/// rather than only from the status. Nothing is written, so the handle survives.
#[tokio::test]
async fn commit_with_contradictory_flags_is_rejected_before_any_write() {
    let (handle, store_handle_id) =
        load_handle("commit-flag-clash", Partition::from([0x48u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0x88).await;
    set_branch(handle, branch).await;

    let globals = LoreGlobalArgs {
        local: 1,
        remote: 1,
        ..Default::default()
    };
    let (status, events) = run_commit_with(
        handle,
        17,
        globals,
        LoreRevisionTreeCommitOptions { remote_write: 1 },
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "local=1 with remote=1 must reject the commit, got {events:?}"
    );
    let (revision, new_tip, code) = commit_outcome(&events, 17);
    assert_eq!(code, LoreErrorCode::InvalidArguments, "got {events:?}");
    assert!(revision.is_zero() && new_tip.is_zero(), "got {events:?}");
    assert!(
        !is_poisoned(handle),
        "a rejection before the freeze must not poison the handle"
    );

    let (retry_status, retry_events) = run_commit(handle, 18).await;
    assert_eq!(
        retry_status, 0,
        "the same tree must commit once the flags agree, got {retry_events:?}"
    );

    release(handle, store_handle_id);
}

/// `remote_write` is a request, not a guarantee: a call that is local-only
/// commits local-only and still succeeds, rather than failing on a contradiction
/// the caller did not state in the options.
#[tokio::test]
async fn remote_write_is_demoted_when_the_call_is_local_only() {
    let (handle, store_handle_id) =
        load_handle("commit-demoted", Partition::from([0x49u8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0x99).await;
    set_branch(handle, branch).await;

    let globals = LoreGlobalArgs {
        local: 1,
        ..Default::default()
    };
    let (status, events) = run_commit_with(
        handle,
        19,
        globals,
        LoreRevisionTreeCommitOptions { remote_write: 1 },
    )
    .await;

    assert_eq!(
        status, 0,
        "a local-only call must still commit, got {events:?}"
    );
    let (revision, _new_tip, code) = commit_outcome(&events, 19);
    assert_eq!(code, LoreErrorCode::None, "got {events:?}");
    assert!(!revision.is_zero(), "got {events:?}");
    assert!(
        upload_disabled(handle),
        "globals.local must demote remote_write=1 to a local-only commit"
    );

    release(handle, store_handle_id);
}

/// A pipeline subscribed to `RevisionCommit*` must see the same telemetry from
/// this surface as from the file-system commit. Three of the four events come
/// from the freeze in `lore-revision`, so without this nothing on either side
/// notices if that stops sending them.
///
/// Positions rather than an exact stream: a freeze that reports progress more
/// than once stays valid, while a dropped or reordered event does not.
#[tokio::test]
async fn commit_emits_the_revision_commit_telemetry_in_order() {
    let (handle, store_handle_id) =
        load_handle("commit-telemetry", Partition::from([0x4bu8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0xbb).await;
    set_branch(handle, branch).await;

    let (status, events) = run_commit(handle, 21).await;
    assert_eq!(status, 0, "the commit must succeed, got {events:?}");

    let first = |wanted: &Captured| {
        events
            .iter()
            .position(|event| event == wanted)
            .unwrap_or_else(|| panic!("{wanted:?} must be emitted, got {events:?}"))
    };
    let begin = first(&Captured::CommitBegin);
    let progress = first(&Captured::CommitProgress);
    let end = first(&Captured::CommitEnd);
    let revision = events
        .iter()
        .position(|event| matches!(event, Captured::CommitRevision(..)))
        .unwrap_or_else(|| panic!("the revision event must be emitted, got {events:?}"));

    assert!(
        begin < progress && progress < end && end < revision,
        "telemetry must run begin -> progress -> end -> revision, got {events:?}"
    );

    release(handle, store_handle_id);
}

/// The other side of the demotion: nothing forbidding a remote leaves
/// `remote_write = 1` asking for the upload, which is what makes the demotion
/// above an observable decision rather than the default doing nothing.
#[tokio::test]
async fn remote_write_requests_the_upload_when_nothing_forbids_a_remote() {
    let (handle, store_handle_id) =
        load_handle("commit-upload", Partition::from([0x4au8; 16])).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "a.bin", 0xaa).await;
    set_branch(handle, branch).await;

    let (status, events) = run_commit_with(
        handle,
        20,
        LoreGlobalArgs::default(),
        LoreRevisionTreeCommitOptions { remote_write: 1 },
    )
    .await;

    assert!(
        !upload_disabled(handle),
        "remote_write=1 on an unrestricted call must ask for the upload"
    );
    assert_eq!(status, 0, "the commit must succeed, got {events:?}");

    release(handle, store_handle_id);
}

/// Add one node of `kind` under `parent`, returning its node id.
async fn add_node(
    handle: LoreRevisionTree,
    entry_id: u64,
    parent: NodeID,
    name: &str,
    kind: LoreNodeType,
) -> NodeID {
    let sink = make_sink();
    let status = add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: 950 + entry_id,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
                entry_id,
                parent_node_id: parent,
                parent_entry_index: 0,
                name: LoreString::from_str(name),
                kind: kind as u32,
                mode: 0o644,
                size: 12,
                address: Address {
                    hash: Hash::from_u64(0x5000 + entry_id),
                    context: Context::default(),
                },
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    assert_eq!(status, 0, "adding {name} must succeed, got {events:?}");
    events
        .iter()
        .find_map(|event| match event {
            Captured::AddComplete(id, node_id, _) if *id == entry_id => Some(*node_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("adding {name} must report a node id, got {events:?}"))
}

/// Delete `node_id` in a call of its own, which must succeed.
async fn delete_node(handle: LoreRevisionTree, node_id: NodeID) {
    let sink = make_sink();
    let status = delete(
        LoreGlobalArgs::default(),
        LoreRevisionTreeDeleteArgs {
            batch_id: 960,
            handle,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeDeleteEntry {
                entry_id: 1,
                node_id,
            }]),
        },
        make_callback(sink.clone()),
    )
    .await;
    let events = sink.lock().unwrap().clone();
    assert_eq!(status, 0, "deleting {node_id} must succeed, got {events:?}");
}

/// The children listed under `parent`, each with its staged action.
async fn listed(handle: LoreRevisionTree, parent: NodeID) -> Vec<(NodeID, u32)> {
    listed_named(handle, parent)
        .await
        .into_iter()
        .map(|(node_id, staged_action, _, _)| (node_id, staged_action))
        .collect()
}

/// The children listed under `parent`, each with its staged action, name and kind.
async fn listed_named(handle: LoreRevisionTree, parent: NodeID) -> Vec<(NodeID, u32, String, u32)> {
    let sink = make_sink();
    let status = list_children(
        LoreGlobalArgs::default(),
        LoreRevisionTreeListChildrenArgs {
            id: 970,
            handle,
            parent_node_id: parent,
        },
        make_callback(sink.clone()),
    )
    .await;
    assert_eq!(status, 0, "listing must succeed");
    let events = sink.lock().unwrap().clone();
    events
        .iter()
        .filter_map(|event| match event {
            Captured::Child(node_id, staged_action, name, kind) => {
                Some((*node_id, *staged_action, name.clone(), *kind))
            }
            _ => None,
        })
        .collect()
}

/// Whether `node_id`'s slot is on its block's free list in the handle's tree.
async fn freed(handle: LoreRevisionTree, node_id: NodeID) -> bool {
    let entry = rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered");
    let state = entry.state_for_tests();
    let block = state
        .block(entry.repository_context.clone(), NodeBlock::index(node_id))
        .await
        .expect("the node's block");
    let reader = block.read();
    let data = reader.node_block();
    let mut at = data.node_unused as usize;
    for _ in 0..data.node_unused_count {
        if at == Node::index(node_id) {
            return true;
        }
        at = data.node[at].sibling as usize;
    }
    false
}

/// Every path revision `revision` holds, read from a tree loaded afresh at it.
async fn published(
    store_handle_id: u64,
    repository: Partition,
    revision: Hash,
) -> std::collections::BTreeSet<String> {
    let tree = load_at(store_handle_id, repository, revision).await;
    let mut paths = std::collections::BTreeSet::new();
    let mut pending = vec![(ROOT_NODE, String::new())];
    while let Some((parent, prefix)) = pending.pop() {
        for (node_id, _, name, kind) in listed_named(tree, parent).await {
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if kind == LoreNodeType::Directory as u32 {
                pending.push((node_id, path.clone()));
            }
            paths.insert(path);
        }
    }
    rt_handle::unregister(tree);
    paths
}

/// The set of `paths`.
fn paths(paths: &[&str]) -> std::collections::BTreeSet<String> {
    paths.iter().map(|path| (*path).to_string()).collect()
}

/// Load a revision tree at `revision` against an open storage handle.
async fn load_at(store_handle_id: u64, repository: Partition, revision: Hash) -> LoreRevisionTree {
    let sink = make_sink();
    let status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store: lore::storage::handle::LoreStore {
                handle_id: store_handle_id,
            },
            repository,
            revision_hash: revision,
        },
        make_callback(sink.clone()),
    )
    .await;
    assert_eq!(status, 0, "the load must succeed");
    let id = sink
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            Captured::Loaded(id) => Some(*id),
            _ => None,
        })
        .expect("the load must emit RevisionTreeLoaded");
    LoreRevisionTree { handle_id: id }
}

/// A delete of an added subtree leaves its slots to the next commit that goes through, which
/// frees them; a commit refused for having nothing staged frees nothing.
#[tokio::test]
async fn a_commit_frees_what_deletes_left_and_one_refused_frees_nothing() {
    let partition = Partition::from([0xd2u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-frees-pending", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    let keep = add_file(handle, 1, "keep.bin", 0x11).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 21).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    let directory = add_node(handle, 2, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let child = add_node(handle, 3, directory, "c.bin", LoreNodeType::File).await;
    delete_node(handle, directory).await;
    let (status, events) = run_commit(handle, 22).await;
    assert_ne!(status, 0, "nothing is staged, got {events:?}");
    for node in [directory, child] {
        assert!(!freed(handle, node).await, "a refused commit frees nothing");
    }

    modify_file(handle, 4, keep, 0x12).await;
    let (status, events) = run_commit(handle, 23).await;
    assert_eq!(status, 0, "the second revision must commit, got {events:?}");
    for node in [directory, child] {
        assert!(
            freed(handle, node).await,
            "the commit frees what the delete left"
        );
    }
    let (revision, _, _) = commit_outcome(&events, 23);
    assert_eq!(
        published(store_handle_id, partition, revision).await,
        paths(&["keep.bin"]),
        "the revision holds none of the deleted subtree"
    );
    release(handle, store_handle_id);
}

/// A delete of a tracked directory stages it, still listed and restorable through an add, and
/// leaves the file added below it to the commit, which frees it.
#[tokio::test]
async fn a_deleted_tracked_directory_stays_restorable_and_its_added_child_is_freed() {
    let partition = Partition::from([0xd3u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-tracked-delete", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    let directory = add_node(handle, 1, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let tracked = add_node(handle, 2, directory, "a.bin", LoreNodeType::File).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 31).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    let added = add_node(handle, 3, directory, "x.bin", LoreNodeType::File).await;
    delete_node(handle, directory).await;
    assert_eq!(
        listed(handle, ROOT_NODE).await,
        vec![(directory, LoreNodeStagedAction::Delete as u32)],
        "the directory stays listed, staged for deletion"
    );
    assert_eq!(
        listed(handle, directory).await,
        vec![(tracked, LoreNodeStagedAction::Delete as u32)],
        "its tracked file is staged too, its added one gone"
    );

    let restored = add_node(handle, 4, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    assert_eq!(restored, directory, "the add restores the directory");
    assert!(
        !freed(handle, added).await,
        "the added file waits for the commit"
    );
    let (status, events) = run_commit(handle, 32).await;
    assert_eq!(status, 0, "the second revision must commit, got {events:?}");
    assert!(
        freed(handle, added).await,
        "the commit frees the added file"
    );
    assert!(listed(handle, directory).await.is_empty());
    let (revision, _, _) = commit_outcome(&events, 32);
    assert_eq!(
        published(store_handle_id, partition, revision).await,
        paths(&["dir"]),
        "the revision holds the restored directory, its deleted file gone"
    );
    release(handle, store_handle_id);
}

/// An added directory deleted and added again under its name is a new node; the commit keeps the
/// new one and frees the old.
#[tokio::test]
async fn a_delete_a_re_add_and_a_commit_keep_the_new_node() {
    let partition = Partition::from([0xd4u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-delete-readd", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "keep.bin", 0x11).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 41).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    let old = add_node(handle, 2, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let old_child = add_node(handle, 3, old, "c.bin", LoreNodeType::File).await;
    delete_node(handle, old).await;
    let new = add_node(handle, 4, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    assert_ne!(
        new, old,
        "a slot left for the commit is not handed out again"
    );
    let new_child = add_node(handle, 5, new, "c.bin", LoreNodeType::File).await;

    let (status, events) = run_commit(handle, 42).await;
    assert_eq!(status, 0, "the second revision must commit, got {events:?}");
    for node in [old, old_child] {
        assert!(freed(handle, node).await, "the old nodes are freed");
    }
    assert_eq!(
        listed(handle, new).await,
        vec![(new_child, LoreNodeStagedAction::None as u32)]
    );
    let (revision, _, _) = commit_outcome(&events, 42);
    assert_eq!(
        published(store_handle_id, partition, revision).await,
        paths(&["dir", "dir/c.bin", "keep.bin"]),
        "the revision holds the new directory, once"
    );
    release(handle, store_handle_id);
}

/// A commit refused before it reaches the tree, here for a branch that moved, frees nothing a
/// delete left, and the deleted directory is still restorable after it.
#[tokio::test]
async fn a_commit_refused_before_the_tree_frees_nothing_and_a_restore_follows() {
    let partition = Partition::from([0xd5u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-refused-delete", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    let directory = add_node(handle, 1, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 51).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");
    let (published, _, _) = commit_outcome(&events, 51);

    let other = load_at(store_handle_id, partition, published).await;
    add_file(other, 2, "b.bin", 0x22).await;
    let (status, events) = run_commit(other, 52).await;
    assert_eq!(
        status, 0,
        "the other handle's commit must publish, got {events:?}"
    );

    let added = add_node(handle, 3, directory, "x.bin", LoreNodeType::File).await;
    delete_node(handle, directory).await;
    let (status, events) = run_commit(handle, 53).await;
    assert_ne!(status, 0, "the branch moved, got {events:?}");
    assert!(
        !freed(handle, added).await,
        "a refused commit frees nothing"
    );

    let restored = add_node(handle, 4, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    assert_eq!(
        restored, directory,
        "the directory is restored after the refusal"
    );
    rt_handle::unregister(other);
    release(handle, store_handle_id);
}

/// A commit the validator refuses, here for two live children of one name, frees nothing a delete
/// left: the slots go only once the last refusal has passed.
#[tokio::test]
async fn a_commit_the_validator_refuses_frees_nothing() {
    let partition = Partition::from([0xe5u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-validator-refuses", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    add_file(handle, 1, "keep.bin", 0x11).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 91).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    let directory = add_node(handle, 2, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let child = add_node(handle, 3, directory, "c.bin", LoreNodeType::File).await;
    delete_node(handle, directory).await;
    add_file(handle, 4, "twin.bin", 0x44).await;
    let twin = Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string("twin.bin"),
        ..Default::default()
    };
    handle_state(handle)
        .node_add_staged(handle_repository(handle), ROOT_NODE, twin, "twin.bin")
        .await
        .expect("the State adds a second child of the name");
    let (status, events) = run_commit(handle, 92).await;
    assert_ne!(status, 0, "the validator refuses the twins, got {events:?}");
    for node in [directory, child] {
        assert!(!freed(handle, node).await, "a refused commit frees nothing");
    }
    release(handle, store_handle_id);
}

/// A commit that fails after its freeze began comes back from its snapshot, taken once what
/// deletes left was freed: the tree it comes back to holds none of it, and a restore and a
/// commit follow.
#[tokio::test]
async fn a_commit_failing_past_its_freeze_comes_back_with_the_deleted_freed() {
    let partition = Partition::from([0xd6u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-failed-delete", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    let directory = add_node(handle, 1, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 61).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    let added = add_node(handle, 2, directory, "x.bin", LoreNodeType::File).await;
    delete_node(handle, directory).await;
    let refused = add_file(handle, 3, "zero.bin", 0).await;
    let (status, events) = run_commit(handle, 62).await;
    assert_eq!(status, -1, "the rehash refuses a zero hash, got {events:?}");
    assert!(
        !is_poisoned(handle),
        "the tree comes back from its snapshot"
    );
    assert!(
        freed(handle, added).await,
        "the snapshot holds the added file freed"
    );

    let restored = add_node(handle, 4, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    assert_eq!(restored, directory, "the directory is restored");
    modify_file(handle, 5, refused, 0x33).await;
    let (status, events) = run_commit(handle, 63).await;
    assert_eq!(status, 0, "the corrected tree must commit, got {events:?}");
    let (revision, _, _) = commit_outcome(&events, 63);
    assert_eq!(
        published(store_handle_id, partition, revision).await,
        paths(&["dir", "zero.bin"]),
        "the revision holds the restored directory, the added file gone"
    );
    release(handle, store_handle_id);
}

/// The handle's repository context.
fn handle_repository(
    handle: LoreRevisionTree,
) -> Arc<lore_revision::repository::RepositoryContext> {
    rt_handle::REGISTRY
        .get(&handle.handle_id)
        .expect("handle registered")
        .repository_context
        .clone()
}

/// The marks a staged move leaves where it is dropped after them, before it moves its node: the
/// directory staged and dirty with its ancestors, and the move noted unfinished.
async fn leave_unfinished_move(handle: LoreRevisionTree, directory: NodeID) {
    let state = handle_state(handle);
    let left = Mark {
        staged: Some((NodeFlags::Staged, false)),
        dirty: Some((NodeFlags::Dirty, Ext::Keep, false)),
    };
    state
        .node_mark(&handle_repository(handle), directory, left)
        .await
        .expect("the mark");
    state.note_unfinished_change();
}

/// After an unfinished change, a commit counts only nodes carrying an action as staged: marks alone
/// commit nothing and keep the note, a change two staged levels down commits and clears it, and
/// then marks alone commit as before, as an add and a delete below a tracked directory leave them.
#[tokio::test]
async fn a_commit_after_an_unfinished_change_counts_only_staged_actions() {
    let partition = Partition::from([0xe1u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-unfinished-move", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    let directory = add_node(handle, 1, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    let nested = add_node(handle, 2, directory, "sub", LoreNodeType::Directory).await;
    let file = add_node(handle, 3, nested, "a.bin", LoreNodeType::File).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 61).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    leave_unfinished_move(handle, nested).await;
    let marked = handle_state(handle);
    let (status, events) = run_commit(handle, 62).await;
    assert_ne!(status, 0, "marks alone commit nothing, got {events:?}");
    assert!(
        !events.contains(&Captured::CommitBegin) && Arc::ptr_eq(&marked, &handle_state(handle)),
        "the commit is refused as nothing staged, before its freeze, got {events:?}"
    );
    assert!(
        marked.has_unfinished_change(),
        "a commit refused for having nothing staged keeps the note"
    );

    modify_file(handle, 4, file, 0x33).await;
    let (status, events) = run_commit(handle, 63).await;
    assert_eq!(
        status, 0,
        "a change below the marked directories commits, got {events:?}"
    );
    assert!(
        !handle_state(handle).has_unfinished_change(),
        "the commit clears the note"
    );

    let added = add_node(handle, 5, nested, "b.bin", LoreNodeType::File).await;
    delete_node(handle, added).await;
    let (status, events) = run_commit(handle, 64).await;
    assert_eq!(
        status, 0,
        "an add and a delete below a tracked directory commit, got {events:?}"
    );
    release(handle, store_handle_id);
}

/// Without an unfinished change, a staged directory commits whatever it holds: the note alone
/// decides the deeper count.
#[tokio::test]
async fn marks_alone_commit_with_no_unfinished_change() {
    let partition = Partition::from([0xe2u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-marks-alone", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    let directory = add_node(handle, 1, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    add_node(handle, 2, directory, "a.bin", LoreNodeType::File).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 71).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    let state = handle_state(handle);
    let left = Mark {
        staged: Some((NodeFlags::Staged, false)),
        dirty: Some((NodeFlags::Dirty, Ext::Keep, false)),
    };
    state
        .node_mark(&handle_repository(handle), directory, left)
        .await
        .expect("the mark");
    let (status, events) = run_commit(handle, 72).await;
    assert_eq!(status, 0, "a staged directory commits, got {events:?}");
    release(handle, store_handle_id);
}

/// A commit that fails after its snapshot replaces the State with the snapshot's, which keeps an
/// unfinished change the State it replaces noted; the commit that then succeeds clears it.
#[tokio::test]
async fn a_restored_tree_keeps_an_unfinished_change() {
    let partition = Partition::from([0xe3u8; 16]);
    let (handle, store_handle_id) = load_handle("commit-restore-unfinished", partition).await;
    let branch = Context::from(uuid::Uuid::now_v7());
    let directory = add_node(handle, 1, ROOT_NODE, "dir", LoreNodeType::Directory).await;
    set_branch(handle, branch).await;
    let (status, events) = run_commit(handle, 81).await;
    assert_eq!(status, 0, "the first revision must commit, got {events:?}");

    leave_unfinished_move(handle, directory).await;
    let replaced = handle_state(handle);
    let refused = add_file(handle, 2, "b.bin", 0).await;
    let (status, events) = run_commit(handle, 82).await;
    assert_eq!(
        status, -1,
        "a zero content hash fails the freeze, got {events:?}"
    );
    let restored = handle_state(handle);
    assert!(
        !Arc::ptr_eq(&replaced, &restored),
        "the failed commit restores a new State"
    );
    assert!(
        restored.has_unfinished_change(),
        "the restored State keeps the note"
    );

    modify_file(handle, 3, refused, 0x22).await;
    let (status, events) = run_commit(handle, 83).await;
    assert_eq!(status, 0, "the corrected tree commits, got {events:?}");
    assert!(
        !handle_state(handle).has_unfinished_change(),
        "the commit clears the note"
    );
    release(handle, store_handle_id);
}
