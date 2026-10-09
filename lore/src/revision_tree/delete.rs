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
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One subtree to remove. The node must already exist and must not be the root.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode,
)]
pub struct LoreRevisionTreeDeleteEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `DELETE_COMPLETE`
    pub entry_id: u64,
    /// Root of the subtree to remove, including its transitive children
    pub node_id: NodeID,
}

/// Arguments for `lore_revision_tree_delete`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(delete_impl)]
pub struct LoreRevisionTreeDeleteArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Subtrees to remove; each emits its own `DELETE_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeDeleteEntry>,
}

#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
fn emit_batch_complete(batch_id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeBatchComplete(LoreRevisionTreeBatchCompleteEventData {
        batch_id,
        error_code,
    })
    .send();
}

/// The code the batch terminal reports for a finished call.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
