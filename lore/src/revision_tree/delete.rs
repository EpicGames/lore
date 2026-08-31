// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_delete` — remove a batch of subtrees from the revision
//! being built. A node the loaded revision holds is staged for deletion and
//! stays in the tree until commit freezes it; a node this handle added has
//! nothing to delete and is discarded outright.

use std::collections::HashSet;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::runtime::processor_count;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeBatchCompleteEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeDeleteCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenIterator;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One subtree to remove. The node must already exist and must not be the root.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreRevisionTreeDeleteEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `DELETE_COMPLETE`
    pub entry_id: u64,
    /// Root of the subtree to remove, including its transitive children
    pub node_id: NodeID,
}

/// Arguments for `lore_revision_tree_delete`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(delete_impl)]
pub struct LoreRevisionTreeDeleteArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Subtrees to remove; each emits its own `DELETE_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeDeleteEntry>,
}

#[error_set]
enum DeleteError {
    InvalidArguments,
}

impl DeleteError {
    /// A rejection the arguments earned, alongside the generated `internal`
    /// constructor for a failure of ours.
    fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }
}

impl EventError for DeleteError {
    fn translated(&self) -> LoreError {
        match self {
            DeleteError::InvalidArguments(_) => LoreError::InvalidArguments,
            DeleteError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_delete_complete(entry_id: u64, node_count: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeDeleteComplete(LoreRevisionTreeDeleteCompleteEventData {
        entry_id,
        node_count,
        error_code,
    })
    .send();
}

/// Emit the `entry_id`-carrying terminal for a failed entry. Nothing was
/// removed, so the count is zero.
fn emit_delete_error(entry_id: u64, error_code: LoreErrorCode) {
    emit_delete_complete(entry_id, 0, error_code);
}

/// Emit the terminal for the call as a whole, carrying its `batch_id`.
fn emit_batch_complete(batch_id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeBatchComplete(LoreRevisionTreeBatchCompleteEventData {
        batch_id,
        error_code,
    })
    .send();
}

/// The code the batch terminal reports for a finished call.
fn batch_error_code(result: &Result<(), DeleteError>) -> LoreErrorCode {
    match result {
        Ok(()) => LoreErrorCode::None,
        Err(DeleteError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(DeleteError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// Reject the whole batch as a bad argument, attributing it to `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
fn reject(entry_id: u64, entry_index: usize, reason: &str) -> DeleteError {
    emit_delete_error(entry_id, LoreErrorCode::InvalidArguments);
    DeleteError::invalid(format!("entry {entry_index}: {reason}"))
}

/// A validated entry, ready to apply without further checks. The kind and
/// staging state are the ones the plan phase read, so the apply phase does not
/// fetch the target a second time.
#[derive(Clone, Copy)]
struct Planned {
    entry_id: u64,
    node_id: NodeID,
    staged_add: bool,
    descend: bool,
}

/// One node of a subtree, carrying the index of the entry that reached it so a
/// per-entry count survives a wavefront shared by the whole batch.
#[derive(Clone, Copy)]
struct Reached {
    node_id: NodeID,
    entry_index: usize,
    /// The node was added through this handle, so it is discarded rather than
    /// staged for deletion.
    staged_add: bool,
    /// Children may hang below, so the walk descends. False for a file and for
    /// a link, whose subtree lives in the linked repository's tree.
    descend: bool,
}

/// Check every entry against the tree and against the rest of the batch,
/// producing the apply plan. Mutates nothing; the first invalid entry rejects
/// the batch.
///
/// A discarded slot and a slot the allocator never handed out both read back as
/// ordinary empty directories, so each is refused on its own terms; a zero name
/// length is what separates the second from a real node. Nesting is settled by
/// walking each target's ancestors — depth per entry, where comparing every pair
/// would cost the batch squared — with `cleared` holding the nodes already proven
/// to have no targeted ancestor so entries sharing a chain walk it once between
/// them.
async fn plan_entries(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    entries: &[LoreRevisionTreeDeleteEntry],
) -> Result<Vec<Planned>, DeleteError> {
    let mut planned: Vec<Planned> = Vec::with_capacity(entries.len());
    let mut targets: HashSet<NodeID> = HashSet::with_capacity(entries.len());
    let mut ids: HashSet<u64> = HashSet::with_capacity(entries.len());

    for (index, entry) in entries.iter().enumerate() {
        let entry_id = entry.entry_id;
        if entry_id != 0 && !ids.insert(entry_id) {
            return Err(reject(entry_id, index, "two entries share one caller id"));
        }
        if !targets.insert(entry.node_id) {
            return Err(reject(
                entry_id,
                index,
                "two entries delete one node; a subtree is removed once",
            ));
        }
        if entry.node_id == ROOT_NODE {
            return Err(reject(
                entry_id,
                index,
                "the root is the revision itself and cannot be deleted",
            ));
        }

        let Ok(node) = state.node(context.clone(), entry.node_id).await else {
            return Err(reject(entry_id, index, "node id is unknown"));
        };
        if node.is_discarded() {
            return Err(reject(entry_id, index, "node has already been discarded"));
        }
        if node.name_length == 0 {
            return Err(reject(
                entry_id,
                index,
                "node id does not resolve to a named node",
            ));
        }
        if node.is_staged_delete() {
            return Err(reject(
                entry_id,
                index,
                "node is already staged for deletion",
            ));
        }

        planned.push(Planned {
            entry_id,
            node_id: entry.node_id,
            staged_add: node.is_staged_add(),
            descend: node.is_directory(),
        });
    }

    let mut cleared: HashSet<NodeID> = HashSet::new();
    for (index, item) in planned.iter().enumerate() {
        let mut ancestor = item.node_id;
        loop {
            let Ok(node) = state.node(context.clone(), ancestor).await else {
                return Err(reject(
                    item.entry_id,
                    index,
                    "node id has an unreadable ancestor",
                ));
            };
            ancestor = node.parent;
            if !ancestor.is_valid_or_root_node_id() || ancestor == ROOT_NODE {
                break;
            }
            if targets.contains(&ancestor) {
                return Err(reject(
                    item.entry_id,
                    index,
                    "another entry deletes an ancestor of this node, which removes it already",
                ));
            }
            if !cleared.insert(ancestor) {
                break;
            }
        }
    }

    Ok(planned)
}

/// Read the children of every node in `level` that can have them, tagging each
/// with the entry that reached it.
///
/// A node already staged for deletion ends the descent: staging a deletion
/// stages the whole subtree, so everything below it is staged too and revisiting
/// it would re-walk a tree that is already accounted for.
///
/// An entry whose subtree mixes added and pre-existing children fails instead:
/// discarding a parent whose children are only staged would unlink a chain those
/// children still point into, and `discard_added` skips a failed entry, so the
/// parent stays put.
async fn next_level(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    level: &[Reached],
    failed: &mut [bool],
) -> Vec<Reached> {
    let mut children = Vec::new();
    for item in level.iter().filter(|item| item.descend) {
        let Ok(mut iterator) =
            StateNodeChildrenIterator::new(state.clone(), context.clone(), item.node_id).await
        else {
            failed[item.entry_index] = true;
            continue;
        };
        loop {
            match iterator.next().await {
                Ok(None) => break,
                Err(_) => {
                    failed[item.entry_index] = true;
                    break;
                }
                Ok(Some((child_id, child))) => {
                    if child.is_discarded() || child.is_staged_delete() {
                        continue;
                    }
                    if item.staged_add && !child.is_staged_add() {
                        failed[item.entry_index] = true;
                        break;
                    }
                    children.push(Reached {
                        node_id: child_id,
                        entry_index: item.entry_index,
                        staged_add: child.is_staged_add(),
                        descend: child.is_directory(),
                    });
                }
            }
        }
    }
    children
}

/// Stage every node of `level` for deletion, over at most one task per
/// processor.
///
/// Tagging is a flag write under the node's own block lock and touches no
/// parent or sibling pointer, so the nodes of one level are independent of each
/// other and of the levels around them.
///
/// A node that cannot be staged fails its own entry and no other: the level runs
/// to completion so every entry's outcome is its own, rather than the first
/// failure standing in for the batch. A group that dies without reporting fails
/// every entry it held.
async fn stage_level(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    level: Arc<Vec<Reached>>,
    counts: &mut [u64],
    failed: &mut [bool],
) {
    let total = level.len();
    if total == 0 {
        return;
    }
    let task_count = processor_count().min(total).max(1);
    let chunk = total.div_ceil(task_count);
    let ranges: Vec<(usize, usize)> = (0..total)
        .step_by(chunk)
        .map(|start| (start, (start + chunk).min(total)))
        .collect();
    let entry_count = counts.len();

    let mut tasks: JoinSet<(usize, Vec<u64>, Vec<bool>)> = JoinSet::new();
    for (group, (start, end)) in ranges.iter().copied().enumerate() {
        let level = level.clone();
        let state = state.clone();
        let context = context.clone();
        lore_spawn!(tasks, async move {
            let mut staged = vec![0u64; entry_count];
            let mut broke = vec![false; entry_count];
            for item in &level[start..end] {
                if item.staged_add {
                    continue;
                }
                match state.node_delete(context.clone(), item.node_id).await {
                    Ok(true) => staged[item.entry_index] += 1,
                    Ok(false) => {}
                    Err(_) => broke[item.entry_index] = true,
                }
            }
            (group, staged, broke)
        });
    }

    let mut reported = vec![false; ranges.len()];
    while let Some(result) = tasks.join_next().await {
        if let Ok((group, staged, broke)) = result {
            reported[group] = true;
            for index in 0..entry_count {
                counts[index] += staged[index];
                failed[index] |= broke[index];
            }
        }
    }
    for (group, (start, end)) in ranges.iter().copied().enumerate() {
        if !reported[group] {
            for item in &level[start..end] {
                failed[item.entry_index] = true;
            }
        }
    }
}

/// Discard every node this handle added, deepest first.
///
/// A node staged for addition is not in the revision the handle was loaded
/// from, so there is nothing for a commit to delete: it is removed from the
/// tree outright, exactly as removing a freshly added link does. Unlike
/// tagging, this rewrites the parent and sibling pointers around the node, and
/// [`state::node_discard_patch`] documents that as work which has to run in
/// serial — so this phase does, and deepest first, so a parent is never
/// unlinked before the children still pointing at it.
///
/// An entry that has already failed is skipped, which is what keeps a parent
/// linked when a discard below it did not happen.
async fn discard_added(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    added: &[(Reached, usize)],
    counts: &mut [u64],
    failed: &mut [bool],
) {
    let mut ordered: Vec<&(Reached, usize)> = added.iter().collect();
    ordered.sort_by_key(|(_, depth)| std::cmp::Reverse(*depth));

    for (item, _) in ordered {
        if failed[item.entry_index] {
            continue;
        }
        match state::node_discard_patch(
            state.clone(),
            context.clone(),
            item.node_id,
            |_discarded_node_id, _flags| {},
        )
        .await
        {
            Ok(_) => counts[item.entry_index] += 1,
            Err(_) => failed[item.entry_index] = true,
        }
    }
}

/// Walk every planned subtree and remove it, reporting per entry how many nodes
/// went.
///
/// The walk runs to completion whatever fails: an entry reports its own outcome,
/// and the call reports how many entries did not finish. An entry that failed
/// reports nothing removed, since part of its subtree is in whichever state the
/// failure left it.
async fn apply_plan(
    state: Arc<State>,
    context: Arc<RepositoryContext>,
    planned: Vec<Planned>,
) -> Result<(), DeleteError> {
    let mut counts = vec![0u64; planned.len()];
    let mut failed = vec![false; planned.len()];
    let mut added: Vec<(Reached, usize)> = Vec::new();

    let mut level: Vec<Reached> = planned
        .iter()
        .enumerate()
        .map(|(entry_index, item)| Reached {
            node_id: item.node_id,
            entry_index,
            staged_add: item.staged_add,
            descend: item.descend,
        })
        .collect();

    let mut depth = 0usize;
    while !level.is_empty() {
        for item in level.iter().filter(|item| item.staged_add) {
            added.push((*item, depth));
        }
        let next = next_level(&state, &context, &level, &mut failed).await;
        stage_level(&state, &context, Arc::new(level), &mut counts, &mut failed).await;
        level = next;
        depth += 1;
    }

    discard_added(&state, &context, &added, &mut counts, &mut failed).await;

    let mut failures = 0usize;
    for (index, item) in planned.iter().enumerate() {
        if failed[index] {
            failures += 1;
            emit_delete_error(item.entry_id, LoreErrorCode::Internal);
        } else {
            emit_delete_complete(item.entry_id, counts[index], LoreErrorCode::None);
        }
    }
    if failures > 0 {
        return Err(DeleteError::internal(format!(
            "{failures}/{} subtree deletions failed",
            planned.len()
        )));
    }
    Ok(())
}

/// Remove a batch of subtrees from the revision being built.
///
/// Each entry names the root of a subtree and removes it whole, transitive
/// children included. Every entry emits `RevisionTreeDeleteComplete` carrying
/// its own `entry_id` and the number of nodes its subtree removed, before the
/// call's `Complete`. An empty batch succeeds.
///
/// A node the loaded revision holds is **staged** for deletion: it keeps its
/// name, its parent and its place among its siblings, and the commit that
/// freezes the tree is what discards it. So it still lists as a child and still
/// reports through `node_info`, carrying `LORE_NODE_STAGED_ACTION_DELETE` — that
/// is how a caller sees what a commit would remove. A node **this handle
/// added** is a different case: it is in no revision yet, so there is nothing
/// for a commit to delete and it is discarded from the tree outright, freeing
/// its name and its node id. A link is staged or discarded as one node; its
/// subtree lives in the linked repository's tree, which this verb does not
/// touch.
///
/// A staged deletion is reversible through `add`: adding the same name under the
/// same parent with the same kind restores the node, staged as a modification.
/// Its `file_id` survives, because a zero `address.context` preserves the
/// identity the node already had; a caller supplying one replaces it, exactly as
/// on `modify`. Only the node named comes back: restoring a directory leaves every
/// child still staged for deletion, since a restore cannot know which of them the
/// caller wants, so each has to be added back in turn. A discarded node is not
/// restorable — its id is gone, and adding the name again creates a new node.
///
/// The call as a whole reports on `RevisionTreeBatchComplete`, carrying the
/// call's own `batch_id` and firing exactly once — after any per-entry
/// terminals and before `Complete`. A failure that belongs to the call rather
/// than to one entry is reported only there: an unknown or closed handle, and a
/// walk that could not read the tree.
///
/// Every entry is checked before any node is touched, and a single bad entry
/// rejects the whole call with `INVALID_ARGUMENTS` on that entry's `entry_id`,
/// leaving every subtree in place. The reason names the entry's batch index,
/// since `entry_id` may be `0` on several entries at once. Rejected are a node
/// id that is unknown, that addresses a slot holding no node, that has already
/// been discarded, that is already staged for deletion, or that is the root; a
/// node id another entry in the same batch also names; a node another entry in
/// the batch deletes an ancestor of, since that entry removes it already; and a
/// non-zero `entry_id` used by another entry — `0` means "not correlating this
/// entry" and may repeat.
///
/// Atomicity covers the rules checked here, which is every rule a caller can
/// break through the arguments. A failure after the checks pass — a block that
/// cannot be read, or a tree changing under the walk — belongs to the entry
/// whose subtree hit it: that entry reports `INTERNAL` with nothing removed,
/// every other entry reports its own outcome, and the call reports `INTERNAL`
/// for the batch. A failed entry may leave part of its subtree removed; nothing
/// is rolled back, the handle stays usable, and no revision is published until
/// `commit`.
///
/// Memory while a batch runs is proportional to the widest level of the subtrees
/// being removed rather than to the number of entries: a level is collected
/// before it is staged, at a few dozen bytes per node. A batch of many small
/// subtrees costs less than one entry naming a directory of a million children.
///
/// Staging fans out. Nodes are removed one depth level at a time and a level's
/// nodes spread over at most one task per processor, since a tag is a flag write
/// under the node's own block lock and touches no parent or sibling pointer.
/// Discarding an added node does rewrite those pointers, so that phase runs
/// serially and deepest first. Per-entry events fire after the whole batch has
/// been walked, so they are not ordered by entry index and carry a count only on
/// success.
///
/// Concurrent calls are not serialized against each other. Two calls deleting
/// nodes in one subtree both validate before either applies, so the second stages
/// nothing where the first already did and reports a smaller count; batch
/// deletions that may overlap into one call, which rejects the overlap.
pub async fn delete(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeDeleteArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, delete_impl).await
}

/// Plan and apply one batch. Split out of the dispatcher closure so the batch
/// terminal fires on every path the batch can take, including an early return.
async fn delete_batch(
    internal: Arc<RevisionTreeInternal>,
    args: LoreRevisionTreeDeleteArgs,
) -> Result<(), DeleteError> {
    if args.entries.is_empty() {
        return Ok(());
    }
    let context = internal.repository_context.clone();
    let access = internal.access_shared().await;
    let state = access.state();
    let planned = plan_entries(&state, &context, args.entries.as_slice()).await?;
    apply_plan(state, context, planned).await
}

async fn delete_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeDeleteArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        delete,
        |args: &LoreRevisionTreeDeleteArgs| {
            emit_batch_complete(args.batch_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal: Arc<RevisionTreeInternal>, args: LoreRevisionTreeDeleteArgs| {
            let call_id = args.batch_id;
            let result = delete_batch(internal, args).await;
            emit_batch_complete(call_id, batch_error_code(&result));
            result
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::interface::LoreNodeStagedAction;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeFlags;

    use super::*;
    use crate::revision_tree::add::LoreRevisionTreeAddArgs;
    use crate::revision_tree::add::LoreRevisionTreeAddEntry;
    use crate::revision_tree::add::add;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
    use crate::revision_tree::list_children::list_children;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::modify::LoreRevisionTreeModifyArgs;
    use crate::revision_tree::modify::LoreRevisionTreeModifyEntry;
    use crate::revision_tree::modify::modify;
    use crate::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
    use crate::revision_tree::node_info::node_info;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

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
        NodeInfo(NodeID, u32, u32),
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
                    Self::NodeInfo(data.node_id, data.kind, data.staged_action)
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
        storage_handle::unregister(crate::storage::handle::LoreStore {
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
    async fn seed(
        handle: LoreRevisionTree,
        parent: NodeID,
        name: &str,
        kind: LoreNodeType,
    ) -> NodeID {
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
            let mut writer = block.write();
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

    /// A subtree of added nodes is discarded whole, deepest first, so unlinking a
    /// parent never strands the children still pointing at it.
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

        let (status, events) =
            run_delete(handle, vec![entry(10, node_id), entry(11, node_id)]).await;
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

        let (status, events) =
            run_delete(handle, vec![entry(10, directory), entry(11, leaf)]).await;
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
                    .write()
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

    /// A settled directory holding a node this handle added is the one shape that
    /// drives both apply sub-phases inside a single entry — the settled nodes are
    /// tagged, the added one is discarded — and where the deepest-first ordering
    /// and the chain patch meet.
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
                CapturedEvent::NodeInfo(node_id, kind, staged_action) => {
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
}
