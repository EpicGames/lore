// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_delete` — remove a batch of subtrees from the revision
//! being built. A node the loaded revision holds is staged for deletion and
//! stays in the tree until commit freezes it; a node this handle added has
//! nothing to delete and is discarded outright.

use std::collections::HashSet;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::error::SlowDown;
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
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::node::SiblingCycleGuard;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::LevelMark;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::StateNodeChildrenIterator;
use lore_revision::subtask;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::EntryFailure;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeGuard;
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
    SlowDown,
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
            DeleteError::SlowDown(_) => LoreError::SlowDown,
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

/// Emit the `entry_id`-carrying terminal for a failed entry, its count zero
/// whatever part of its subtree was removed.
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
        Err(DeleteError::SlowDown(_)) => LoreErrorCode::SlowDown,
        Err(DeleteError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// An entry the planning refused: the whole batch is rejected as a bad argument, attributed to
/// `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
struct Rejection {
    entry_id: u64,
    entry_index: usize,
    reason: &'static str,
}

impl Rejection {
    /// Reports the rejection on its entry and makes the call's error.
    fn emit(self) -> DeleteError {
        emit_delete_error(self.entry_id, LoreErrorCode::InvalidArguments);
        DeleteError::invalid(format!("entry {}: {}", self.entry_index, self.reason))
    }
}

fn reject(entry_id: u64, entry_index: usize, reason: &'static str) -> Rejection {
    Rejection {
        entry_id,
        entry_index,
        reason,
    }
}

/// A validated entry, ready to apply without further checks. The kind and
/// staging state are the ones the plan phase read, so the apply phase does not
/// fetch the target a second time.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct Planned {
    entry_id: u64,
    node_id: NodeID,
    /// The directory the node stood in, and its name's hash, as the plan read them: where an
    /// added root is detached from.
    parent: NodeID,
    name_hash: u64,
    staged_add: bool,
    directory: bool,
}

/// Checks `entries` as a call does, reporting a rejection on its entry.
#[cfg(feature = "test-util")]
pub async fn plan_entries(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    entries: &[LoreRevisionTreeDeleteEntry],
) -> Result<Vec<Planned>, DeleteError> {
    plan(state, context, entries).await.map_err(Rejection::emit)
}

/// Checks every entry against the tree and against the rest of the batch, producing the apply
/// plan. Mutates nothing and reports nothing: the first invalid entry rejects the batch, handed
/// back for the call to report once its task has ended.
///
/// A discarded slot and a slot the allocator never handed out both read back as
/// ordinary empty directories, so each is refused on its own terms; a zero name
/// length is what separates the second from a real node. Nesting is settled by
/// walking each target's ancestors — depth per entry, where comparing every pair
/// would cost the batch squared — with `cleared` holding the nodes already proven
/// to have no targeted ancestor so entries sharing a chain walk it once between
/// them.
async fn plan(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    entries: &[LoreRevisionTreeDeleteEntry],
) -> Result<Vec<Planned>, Rejection> {
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
            parent: node.parent,
            name_hash: node.name_hash,
            staged_add: node.is_staged_add(),
            directory: node.is_directory(),
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

/// A tracked node of a level, below `parent`, reached by entry `entry_index`.
#[derive(Clone, Copy)]
struct Tracked {
    node_id: NodeID,
    parent: NodeID,
    entry_index: usize,
}

/// An added node standing in a tracked one, or an added entry root: the root of a subtree the
/// delete detaches whole.
#[derive(Clone, Copy)]
struct AddedRoot {
    node_id: NodeID,
    parent: NodeID,
    name_hash: u64,
    entry_index: usize,
    /// Whether it is a directory, as the delete read it: a file or a link holds nothing below.
    directory: bool,
}

/// What a group of a level found: its nodes staged and its failures per entry, in `T`, the
/// tracked children making the next level, and the added children.
#[derive(Default)]
struct LevelFound<T> {
    tally: T,
    next: Vec<Tracked>,
    added: Vec<AddedRoot>,
}

/// Per entry of the batch, how many nodes the delete removed, and how it failed.
struct Applied {
    counts: Vec<u64>,
    failed: Vec<EntryFailure>,
}

/// Per entry a fan-out task's items reached, a count and how it failed, consecutive items of one
/// entry folded into one run, so a task's memory and its merge follow its items, not the batch's
/// entries.
#[derive(Default)]
struct Runs(Vec<Run>);

/// What consecutive items of one entry counted, and how they failed.
#[derive(Clone, Copy)]
struct Run {
    entry: usize,
    count: u64,
    failure: EntryFailure,
}

/// Where a group counts its entries' nodes and failures: the call's [`Applied`], or a fan-out
/// task's [`Runs`].
trait Tally: Send {
    /// Counts `by` more nodes removed for `entry`.
    fn count(&mut self, entry: usize, by: u64);

    /// Fails `entry` as `failure` says, keeping the stronger failure.
    fn fail(&mut self, entry: usize, failure: EntryFailure);
}

impl Tally for Applied {
    fn count(&mut self, entry: usize, by: u64) {
        self.counts[entry] += by;
    }

    fn fail(&mut self, entry: usize, failure: EntryFailure) {
        self.failed[entry].merge(failure);
    }
}

impl Tally for Runs {
    fn count(&mut self, entry: usize, by: u64) {
        match self.0.last_mut() {
            Some(run) if run.entry == entry => run.count += by,
            _ => self.0.push(Run {
                entry,
                count: by,
                failure: EntryFailure::None,
            }),
        }
    }

    fn fail(&mut self, entry: usize, failure: EntryFailure) {
        match self.0.last_mut() {
            Some(run) if run.entry == entry => run.failure.merge(failure),
            _ => self.0.push(Run {
                entry,
                count: 0,
                failure,
            }),
        }
    }
}

impl Applied {
    /// Nothing removed and nothing failed, for `entry_count` entries.
    fn new(entry_count: usize) -> Self {
        Self {
            counts: vec![0u64; entry_count],
            failed: vec![EntryFailure::None; entry_count],
        }
    }

    /// Takes in what a fan-out task counted.
    fn take_runs(&mut self, runs: Runs) {
        for run in runs.0 {
            self.count(run.entry, run.count);
            self.fail(run.entry, run.failure);
        }
    }

    /// Reports each entry's outcome, an entry that failed with a count of zero, whatever part
    /// of its subtree it removed, and makes the call's result: `SLOW_DOWN` where every entry
    /// that failed slowed, `INTERNAL` otherwise.
    fn emit(self, planned: &[Planned]) -> Result<(), DeleteError> {
        let mut failures = 0usize;
        let mut strongest = EntryFailure::None;
        for (index, item) in planned.iter().enumerate() {
            let failure = self.failed[index];
            if failure.failed() {
                failures += 1;
                strongest.merge(failure);
                emit_delete_error(item.entry_id, failure.code());
            } else {
                emit_delete_complete(item.entry_id, self.counts[index], LoreErrorCode::None);
            }
        }
        if failures > 0 {
            return Err(strongest.call_error(&format!(
                "{failures}/{} subtree deletions failed",
                planned.len()
            )));
        }
        Ok(())
    }
}

/// Stages every tracked node of `level` for deletion, over at most one task per processor,
/// each reading the node's chain head under the lock its mark takes
/// ([`State::node_delete_level`]) and sorting the children it reaches: tracked ones make the
/// next level, added ones are the roots of subtrees to detach, and nodes staged for deletion or
/// discarded already are passed over. A node that cannot be marked fails its own entry and no
/// other, and a group that dies without reporting fails every entry it held. A level one task
/// takes is marked in place, with no task, where a panic fails the whole batch rather than the
/// group's entries.
async fn mark_level(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    level: Vec<Tracked>,
    found: &mut LevelFound<Applied>,
) {
    let total = level.len();
    let task_count = processor_count().min(total).max(1);
    if task_count == 1 {
        mark_group(state, context, &level, found).await;
        return;
    }
    let chunk = total.div_ceil(task_count);
    let ranges: Vec<(usize, usize)> = (0..total)
        .step_by(chunk)
        .map(|start| (start, (start + chunk).min(total)))
        .collect();
    fan_out(state, context, level, ranges, found).await;
}

/// What the groups of a fan-out find ([`fan_out`]): each task finds into a [`Self::Own`] of its
/// own, holding its items' entries alone, merged into the call's once the task ends.
trait Found {
    /// The items a group is made of.
    type Item: Send + Sync + 'static;

    /// What a group's task finds into.
    type Own: Default + Send + 'static;

    /// Works `items`, one after another, into `own`.
    fn work<'a>(
        own: &'a mut Self::Own,
        state: &'a Arc<State>,
        context: &'a Arc<RepositoryContext>,
        items: &'a [Self::Item],
    ) -> impl Future<Output = ()> + Send + 'a;

    /// Takes in what a group's task found.
    fn merge(&mut self, own: Self::Own);

    /// Fails the entry that reached `item`.
    fn fail_item(&mut self, item: &Self::Item);
}

/// Works `items` into `found`, each of `ranges` of them in a task of its own, as [`Found`] says.
/// A task that dies without reporting fails every entry its range held.
async fn fan_out<F: Found>(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    items: Vec<F::Item>,
    ranges: Vec<(usize, usize)>,
    found: &mut F,
) {
    let items = Arc::new(items);
    let mut tasks: JoinSet<(usize, F::Own)> = JoinSet::new();
    for (group, (start, end)) in ranges.iter().copied().enumerate() {
        let items = items.clone();
        let state = state.clone();
        let context = context.clone();
        lore_spawn!(
            tasks,
            subtask!(async move {
                let mut own = F::Own::default();
                F::work(&mut own, &state, &context, &items[start..end]).await;
                (group, own)
            })
        );
    }

    let mut reported = vec![false; ranges.len()];
    while let Some(result) = tasks.join_next().await {
        if let Ok((group, own)) = result {
            reported[group] = true;
            found.merge(own);
        }
    }
    for (group, (start, end)) in ranges.iter().copied().enumerate() {
        if !reported[group] {
            for item in &items[start..end] {
                found.fail_item(item);
            }
        }
    }
}

impl Found for LevelFound<Applied> {
    type Item = Tracked;
    type Own = LevelFound<Runs>;

    fn work<'a>(
        own: &'a mut LevelFound<Runs>,
        state: &'a Arc<State>,
        context: &'a Arc<RepositoryContext>,
        items: &'a [Tracked],
    ) -> impl Future<Output = ()> + Send + 'a {
        mark_group(state, context, items, own)
    }

    fn merge(&mut self, mut own: LevelFound<Runs>) {
        self.tally.take_runs(own.tally);
        take_all(&mut self.next, &mut own.next);
        take_all(&mut self.added, &mut own.added);
    }

    fn fail_item(&mut self, item: &Tracked) {
        self.tally.fail(item.entry_index, EntryFailure::Internal);
    }
}

impl Found for Applied {
    type Item = AddedRoot;
    type Own = Runs;

    fn work<'a>(
        own: &'a mut Runs,
        state: &'a Arc<State>,
        context: &'a Arc<RepositoryContext>,
        items: &'a [AddedRoot],
    ) -> impl Future<Output = ()> + Send + 'a {
        detach_group(state, context, items, own)
    }

    fn merge(&mut self, own: Runs) {
        self.take_runs(own);
    }

    fn fail_item(&mut self, root: &AddedRoot) {
        self.fail(root.entry_index, EntryFailure::Internal);
    }
}

/// Marks `items`, one after another, into `found`, as [`mark_level`] says.
async fn mark_group<T: Tally>(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    items: &[Tracked],
    found: &mut LevelFound<T>,
) {
    for item in items {
        match state
            .node_delete_level(context, item.node_id, item.parent)
            .await
        {
            Ok(LevelMark::Marked(head)) => {
                found.tally.count(item.entry_index, 1);
                if let Err(error) = sort_children(state, context, item, head, found).await {
                    found.tally.fail(item.entry_index, EntryFailure::of(&error));
                }
            }
            Ok(LevelMark::Staged) => {}
            Ok(LevelMark::Other) => found.tally.fail(item.entry_index, EntryFailure::Internal),
            Err(error) => found.tally.fail(item.entry_index, EntryFailure::of(&error)),
        }
    }
}

/// Moves everything `from` holds onto the end of `into`, taking its buffer where `into` holds
/// nothing, so the first group's findings are not copied.
fn take_all<T>(into: &mut Vec<T>, from: &mut Vec<T>) {
    if into.is_empty() {
        std::mem::swap(into, from);
    } else {
        into.append(from);
    }
}

/// Sorts the children of `item`, its chain starting at `head`, into `found`: tracked ones for
/// the next level, added ones as roots to detach.
async fn sort_children<T>(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    item: &Tracked,
    head: Option<NodeID>,
    found: &mut LevelFound<T>,
) -> Result<(), StateError> {
    let mut cycle = SiblingCycleGuard::new(item.node_id);
    let mut next = head;
    // The block of the last child read, as siblings mostly share one.
    let mut held: Option<(usize, Arc<NodeBlock>)> = None;
    while let Some(child_id) = next {
        let block_index = NodeBlock::index(child_id);
        let block = match &held {
            Some((index, block)) if *index == block_index => block,
            _ => {
                let block = match state.resident_block(block_index) {
                    Some(block) => block,
                    None => state.block(context.clone(), block_index).await?,
                };
                &held.insert((block_index, block)).1
            }
        };
        let child = block.node(Node::index(child_id));
        child.walk_step(child_id, item.node_id, &mut cycle)?;
        next = child.sibling();
        if child.is_discarded() || child.is_staged_delete() {
            continue;
        }
        if child.is_staged_add() {
            found.added.push(AddedRoot {
                node_id: child_id,
                parent: item.node_id,
                name_hash: child.name_hash,
                entry_index: item.entry_index,
                directory: child.is_directory(),
            });
        } else {
            found.next.push(Tracked {
                node_id: child_id,
                parent: item.node_id,
                entry_index: item.entry_index,
            });
        }
    }
    Ok(())
}

/// Whether the subtree below added node `root` holds a node that is not added: a tracked node moved
/// into it, which a detach of the subtree would take along. A node staged for deletion is passed
/// over with all below it: the commit frees it with the subtree. Links are leaves. `pending` is the
/// caller's buffer for the directories still to walk, cleared on entry.
async fn mixed(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    root: &AddedRoot,
    pending: &mut Vec<NodeID>,
) -> Result<bool, StateError> {
    if !root.directory {
        return Ok(false);
    }
    pending.clear();
    pending.push(root.node_id);
    while let Some(directory) = pending.pop() {
        let mut children =
            StateNodeChildrenIterator::new(state.clone(), context.clone(), directory).await?;
        while let Some((child_id, child)) = children.next().await? {
            if child.is_staged_delete() {
                continue;
            }
            if !child.is_staged_add() {
                return Ok(true);
            }
            if child.is_directory() {
                pending.push(child_id);
            }
        }
    }
    Ok(false)
}

/// Detaches every added root of an entry that has not failed, grouped by parent, the groups over
/// at most one task per processor: detaches from one directory's chain run one after another, so
/// siblings do not fail each other's chain checks. Each root goes with its whole subtree onto the
/// State's pending queue ([`State::node_discard_pending`]), counted for its entry; one no longer
/// where the delete found it fails its entry. Where one task would take every group, they are
/// detached in place, with no task, where a panic fails the whole batch rather than the groups'
/// entries.
async fn detach_added(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    mut added: Vec<AddedRoot>,
    applied: &mut Applied,
) {
    added.retain(|root| !applied.failed[root.entry_index].failed());
    if added.is_empty() {
        return;
    }
    // Stable, so each parent's roots keep the order the delete read them in.
    added.sort_by_key(|root| root.parent);
    let group_count = 1 + added
        .windows(2)
        .filter(|pair| pair[0].parent != pair[1].parent)
        .count();
    let task_count = processor_count().min(group_count).max(1);
    if task_count == 1 {
        detach_group(state, context, &added, applied).await;
        return;
    }
    let mut groups: Vec<(usize, usize)> = Vec::with_capacity(group_count);
    let mut start = 0;
    for index in 1..=added.len() {
        if index == added.len() || added[index].parent != added[start].parent {
            groups.push((start, index));
            start = index;
        }
    }
    let per_task = groups.len().div_ceil(task_count);
    let batches: Vec<(usize, usize)> = groups
        .chunks(per_task)
        .map(|batch| (batch[0].0, batch[batch.len() - 1].1))
        .collect();
    fan_out(state, context, added, batches, applied).await;
}

/// Detaches `roots` one after another onto the State's pending queue, counting each root's
/// subtree for its entry; a root no longer where the delete found it fails its entry.
async fn detach_group<T: Tally>(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    roots: &[AddedRoot],
    tally: &mut T,
) {
    for root in roots {
        match state
            .node_discard_pending(context.clone(), root.node_id, root.parent, root.name_hash)
            .await
        {
            Ok(Some(count)) => tally.count(root.entry_index, count),
            Ok(None) => tally.fail(root.entry_index, EntryFailure::Internal),
            Err(error) => tally.fail(root.entry_index, EntryFailure::of(&error)),
        }
    }
}

/// Applies `planned` as a call does, reporting each entry's outcome.
#[cfg(feature = "test-util")]
pub async fn apply_plan(
    state: Arc<State>,
    context: Arc<RepositoryContext>,
    planned: Vec<Planned>,
) -> Result<(), DeleteError> {
    apply(&state, &context, &planned).await.emit(&planned)
}

/// Walks every planned subtree and removes it, counting per entry how many nodes went, and hands
/// the outcome back for the call to report once its task has ended. The walk runs to completion
/// whatever fails: an entry reports its own outcome, and the call reports how many entries did not
/// finish. An entry that failed reports nothing removed, since part of its subtree is in whichever
/// state the failure left it.
///
/// Tracked nodes are staged for deletion a level at a time, top-down, each level read from the
/// chain heads the level above read under its marks' locks, so a child added or restored meanwhile
/// is reached, staged and counted, or refused under the lock. Then every added subtree an entry
/// reached is checked whole, one holding a tracked node failing its entry with nothing detached,
/// and the subtrees of the entries left are detached onto the pending queue.
async fn apply(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    planned: &[Planned],
) -> Applied {
    let mut found = LevelFound {
        tally: Applied::new(planned.len()),
        next: Vec::new(),
        added: Vec::new(),
    };
    let mut level: Vec<Tracked> = Vec::new();
    for (entry_index, item) in planned.iter().enumerate() {
        if item.staged_add {
            found.added.push(AddedRoot {
                node_id: item.node_id,
                parent: item.parent,
                name_hash: item.name_hash,
                entry_index,
                directory: item.directory,
            });
        } else {
            level.push(Tracked {
                node_id: item.node_id,
                parent: item.parent,
                entry_index,
            });
        }
    }
    while !level.is_empty() {
        mark_level(state, context, level, &mut found).await;
        level = std::mem::take(&mut found.next);
    }

    let LevelFound {
        tally: mut applied,
        added,
        ..
    } = found;
    let mut pending = Vec::new();
    for root in &added {
        if applied.failed[root.entry_index].failed() {
            continue;
        }
        match mixed(state, context, root, &mut pending).await {
            Ok(false) => {}
            Ok(true) => applied.fail(root.entry_index, EntryFailure::Internal),
            Err(error) => applied.fail(root.entry_index, EntryFailure::of(&error)),
        }
    }
    detach_added(state, context, added, &mut applied).await;
    applied
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
/// for a commit to delete and it leaves the tree outright: it stops being listed,
/// `node_info` reads it as a freed id does, and the next commit that goes ahead frees
/// its node id.
/// A link is staged or discarded as one node; its subtree lives in the linked
/// repository's tree, which this verb does not touch.
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
/// than to one entry is reported only there: an unknown or closed handle, a
/// moves permit that cannot be taken, and the batch's own task ending without a
/// result, as a panic in it does.
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
/// for the batch. An entry the tree gave up on under load, heavy concurrent
/// change or a store asking it to slow down, reports `SLOW_DOWN` instead, which a
/// caller can retry, and so does the batch where nothing failed otherwise. A
/// failed entry may leave part of its subtree removed; nothing is rolled back,
/// the handle stays usable, and no revision is published until `commit`.
///
/// Memory while a batch runs is proportional to its entry count, for the plan and
/// each entry's count and failure; a task a level or the detaches spread over keeps
/// a count and a failure only for the entries its own items reach. Added to
/// that are two adjacent levels of the tracked subtrees being removed, the one
/// being staged and the next, collected from the chain heads its marks read, at a
/// few dozen bytes per node; one record per added subtree root found, held to the
/// end of the batch; and, while an added subtree is checked, its directories still
/// to walk.
///
/// Staging fans out. Nodes are staged one depth level at a time and a level's
/// nodes spread over at most one task per processor, since a tag is a flag write
/// under the node's own block lock, beside a read lock of its parent's, and
/// touches no parent or sibling pointer. Each added subtree is checked whole,
/// then detached from its directory in one step, its nodes kept for the commit to
/// free; detaches from one directory run one after another. Per-entry events fire
/// in entry order after the whole batch has been walked, and carry a count only
/// on success. A node staged for deletion inside an added subtree, by a move
/// there then a delete, is not counted again.
///
/// A call holds the tree's moves permit from before its checks to after its last
/// change, so deletes and moves apply one after another, each checking its
/// entries against the tree the one before left: a second delete of a subtree
/// the first removed is rejected. Adds, modifications and restores run
/// alongside: one that lands before the delete reaches its directory is removed
/// with it, and one after is refused. A restore of a node below an added
/// directory, which stands there only once a move put it there, waits for the
/// permit instead: landing first, it makes the added subtree mixed, so the
/// delete's entry fails with `INTERNAL` and nothing is detached. The batch runs
/// in a task the call waits for, so a call dropped part way still finishes its
/// batch, and reports nothing.
pub async fn delete(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeDeleteArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, delete_impl).await
}

/// Plan and apply one batch in a task of its own, then report what it found. Split out of the
/// dispatcher closure so the batch terminal fires on every path the batch can take, including an
/// early return.
///
/// The task holds a guard of its own, so a close waits for it, and takes the shared claim and
/// then the moves permit itself, as a move batch does: a delete and a move never interleave, and
/// a call dropped while its task runs leaves no subtree half deleted. Every event the batch
/// reports, its rejection included, is emitted here after the task ends, so a dropped call
/// reports nothing. A dropped call drops the task's `JoinHandle`, which detaches the task, as a
/// `DetachingJoinSet` would without its several hundred instructions per call.
async fn delete_batch(
    internal: Arc<RevisionTreeInternal>,
    args: LoreRevisionTreeDeleteArgs,
) -> Result<(), DeleteError> {
    if args.entries.is_empty() {
        return Ok(());
    }
    let Some(guard) = RevisionTreeGuard::join(internal.clone()) else {
        return Err(DeleteError::invalid(
            "revision tree handle is unknown or has been closed",
        ));
    };
    let task = lore_spawn!(subtask!(async move {
        let context = internal.repository_context.clone();
        let access = internal.access_shared().await;
        let state = access.state();
        let permit = state
            .moves_permit()
            .await
            .map_err(|error| DeleteError::internal_with_context(error, "take the moves permit"))?;
        let planned = match plan(&state, &context, args.entries.as_slice()).await {
            Ok(planned) => planned,
            Err(rejection) => return Ok(Err(rejection)),
        };
        let applied = apply(&state, &context, &planned).await;
        drop(permit);
        drop(access);
        drop(guard);
        Ok::<_, DeleteError>(Ok((planned, applied)))
    }));
    let found = task.await.internal("run the delete's task")??;
    match found {
        Ok((planned, applied)) => applied.emit(&planned),
        Err(rejection) => Err(rejection.emit()),
    }
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
