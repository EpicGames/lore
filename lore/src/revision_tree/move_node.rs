// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_move` — reparent and/or rename a batch of nodes while
//! preserving each one's `file_id`, so the resulting revision graph records true
//! moves instead of delete-plus-add pairs. The Rust module is named `move_node`
//! because `move` is a reserved keyword; the C symbol stays
//! `lore_revision_tree_move`.
//!
//! Neither batch rule can be settled entry by entry: two moves that are legal on
//! their own can be jointly a loop, and a name one entry vacates is a name another
//! entry may take. Loops are settled in batch order, against the reparenting the
//! earlier entries perform; names are settled once every destination is known,
//! against the tree the whole batch produces.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeBatchCompleteEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeMoveCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::node::SiblingCycleGuard;
use lore_revision::node::validate_node_name_for_store;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::StateNodeChildrenIterator;
use lore_storage::hash::hash_string;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One node to move. The node must already exist and must not be the root.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreRevisionTreeMoveEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `MOVE_COMPLETE`
    pub entry_id: u64,
    /// Node to move; its `file_id` is preserved across the move
    pub node_id: NodeID,
    /// Parent node the moved node is reparented under; its current parent renames it
    pub destination_parent_id: NodeID,
    /// UTF-8 name the moved node takes at the destination
    pub dst_name: LoreString,
}

/// Arguments for `lore_revision_tree_move`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(move_node_impl)]
pub struct LoreRevisionTreeMoveArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Nodes to move; each emits its own `MOVE_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeMoveEntry>,
}

#[error_set]
enum MoveError {
    InvalidArguments,
}

impl MoveError {
    /// A rejection the arguments earned, alongside the generated `internal`
    /// constructor for a failure of ours.
    fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }
}

impl EventError for MoveError {
    fn translated(&self) -> LoreError {
        match self {
            MoveError::InvalidArguments(_) => LoreError::InvalidArguments,
            MoveError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_move_complete(entry_id: u64, node_id: NodeID, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeMoveComplete(LoreRevisionTreeMoveCompleteEventData {
        entry_id,
        node_id,
        error_code,
    })
    .send();
}

/// Emit the `entry_id`-carrying terminal for a failed entry. Nothing moved, so the
/// reported node is the invalid-node sentinel.
fn emit_move_error(entry_id: u64, error_code: LoreErrorCode) {
    emit_move_complete(entry_id, INVALID_NODE, error_code);
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
fn batch_error_code(result: &Result<(), MoveError>) -> LoreErrorCode {
    match result {
        Ok(()) => LoreErrorCode::None,
        Err(MoveError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(MoveError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// Reject the whole batch as a bad argument, attributing it to `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
fn reject(entry_id: u64, entry_index: usize, reason: &str) -> MoveError {
    emit_move_error(entry_id, LoreErrorCode::InvalidArguments);
    MoveError::invalid(format!("entry {entry_index}: {reason}"))
}

/// Reject the whole batch because the tree could not be read, keeping the underlying
/// failure as context. Not the caller's fault, so it reports `INTERNAL`.
fn reject_internal(
    entry_id: u64,
    entry_index: usize,
    error: StateError,
    context: &str,
) -> MoveError {
    emit_move_error(entry_id, LoreErrorCode::Internal);
    MoveError::internal_with_context(error, &format!("entry {entry_index}: {context}"))
}

/// A validated entry, ready to apply without further checks. The name is not copied
/// here: `entry_index` indexes the batch arguments, which own it and outlive the
/// apply phase.
#[derive(Clone, Copy)]
struct Planned {
    entry_id: u64,
    entry_index: usize,
    node_id: NodeID,
    destination_parent_id: NodeID,
    name_hash: u64,
}

/// The destination name of a planned entry, borrowed from the batch arguments.
fn entry_name(args: &LoreRevisionTreeMoveArgs, entry_index: usize) -> &str {
    args.entries.as_slice()[entry_index].dst_name.as_str()
}

/// Check that an existing node can take children, attributing any failure to
/// `entry_id`. Runs once per destination per batch, for the first entry that names it.
///
/// Returns the parent it read, which the caller keeps: the name checks start from the
/// child chain it holds.
///
/// A discarded slot and a slot the allocator never handed out both read back as
/// ordinary directories, so each is refused on its own terms: a child hung off a
/// discarded slot is orphaned once the allocator reuses it, and since every non-root
/// node has a non-empty name, a zero name length is what separates an unallocated slot
/// from a real node.
async fn check_destination(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    destination_parent_id: NodeID,
    entry_id: u64,
    entry_index: usize,
) -> Result<Node, MoveError> {
    let Ok(parent) = state.node(context.clone(), destination_parent_id).await else {
        return Err(reject(
            entry_id,
            entry_index,
            "destination parent id is unknown",
        ));
    };
    if parent.is_discarded() {
        return Err(reject(
            entry_id,
            entry_index,
            "destination parent has been deleted",
        ));
    }
    if parent.is_staged_delete() {
        return Err(reject(
            entry_id,
            entry_index,
            "destination parent is staged for deletion, so the moved node would go with it",
        ));
    }
    if parent.is_link() {
        return Err(reject(
            entry_id,
            entry_index,
            "destination parent is a link, which addresses a revision this handle does not mutate",
        ));
    }
    if !parent.is_directory() {
        return Err(reject(
            entry_id,
            entry_index,
            "destination parent is not a directory",
        ));
    }
    if destination_parent_id != ROOT_NODE && parent.name_length == 0 {
        return Err(reject(
            entry_id,
            entry_index,
            "destination parent id does not resolve to a named node",
        ));
    }
    Ok(parent)
}

/// Whether moving `node_id` under `destination` closes a loop once every entry
/// planned so far has been applied.
///
/// Walks the destination's ancestors rather than the moved node's subtree — an
/// ancestor chain is one node per level, where the subtree can be the whole tree —
/// and takes each step through the batch first, so an entry that reparents an
/// ancestor is followed to where it puts it. That is what makes moving A under B and
/// B under A a rejection: individually neither is a loop, and jointly they are.
///
/// The walk is guarded against a chain that loops already, so a corrupt tree fails
/// here rather than hangs.
async fn closes_a_loop(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    moved: &HashMap<NodeID, NodeID>,
    destination: NodeID,
    node_id: NodeID,
) -> Result<bool, StateError> {
    let mut ancestor = destination;
    let mut cycle = SiblingCycleGuard::new(node_id);
    while ancestor.is_valid_node_id() {
        if ancestor == node_id {
            return Ok(true);
        }
        if cycle.observe(ancestor).is_err() {
            return Err(StateError::internal(format!(
                "the ancestors of node {destination} loop"
            )));
        }
        ancestor = match moved.get(&ancestor) {
            Some(parent) => *parent,
            None => state.node(context.clone(), ancestor).await?.parent,
        };
    }
    Ok(false)
}

/// The children of `parent` that a name can collide with, as `(name_hash, node_id)`
/// sorted by hash.
///
/// A child staged for deletion holds nothing: it leaves the revision at the commit
/// that freezes the tree, and the commit's own validator ignores it when it refuses
/// two siblings sharing a name.
///
/// `only` narrows the walk to one name, for the destination named by a single entry —
/// where collecting every child to answer one question would be a loss. The rest is a
/// snapshot every later entry landing there searches with [`snapshot_holders`], so it
/// is sorted here rather than scanned once per entry.
async fn live_children(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    parent: NodeID,
    parent_node: &Node,
    only: Option<u64>,
) -> Result<Vec<(u64, NodeID)>, StateError> {
    let mut children =
        StateNodeChildrenIterator::from_parent(state.clone(), context.clone(), parent, parent_node)
            .await?;
    let mut names = Vec::new();
    while let Some((node_id, node)) = children.next().await? {
        if node.is_staged_delete() || node.is_discarded() {
            continue;
        }
        if only.is_some_and(|name_hash| node.name_hash != name_hash) {
            continue;
        }
        names.push((node.name_hash, node_id));
    }
    names.sort_unstable_by_key(|(name_hash, _)| *name_hash);
    Ok(names)
}

/// The nodes of a sorted snapshot that hold `name_hash`.
///
/// Sorted values are already hashes, so this is a binary search over them rather than a
/// hash set that would run each of them through a hasher a second time — and rather than
/// a scan, which would cost a batch filling one directory the product of its entries and
/// that directory's width.
fn snapshot_holders(names: &[(u64, NodeID)], name_hash: u64) -> impl Iterator<Item = NodeID> + '_ {
    let start = names.partition_point(|(hash, _)| *hash < name_hash);
    names[start..]
        .iter()
        .take_while(move |(hash, _)| *hash == name_hash)
        .map(|(_, node_id)| *node_id)
}

/// Check every entry against the tree and against the rest of the batch, producing the
/// apply plan. Mutates nothing; the first invalid entry rejects the batch.
///
/// Two passes, because the two batch-level rules read the batch differently. The first
/// takes the entries in order: each is checked against the tree, and against the
/// reparenting the entries before it perform, which is what settles loops in the order
/// the apply phase will produce them. The second runs once every destination is known,
/// so a name is held against the tree the whole batch produces: a name an entry vacates
/// is free for another entry to take, and two entries claiming one name collide even
/// though neither collides with the tree.
///
/// A destination is checked once per batch however many entries name it. Its names are
/// looked up directly when one entry lands there and, from the second on, against a
/// snapshot collected in a single chain walk — so a batch filling one directory walks
/// it once instead of once per entry, while a batch touching many destinations once
/// each never walks at all.
async fn plan_entries(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    entries: &[LoreRevisionTreeMoveEntry],
) -> Result<Vec<Planned>, MoveError> {
    let mut planned: Vec<Planned> = Vec::with_capacity(entries.len());
    let mut ids: HashSet<u64> = HashSet::with_capacity(entries.len());
    // The destination parent of every node the batch moves, which is what the loop check
    // steps through and what tells a name check that a holder is on its way out.
    let mut moved: HashMap<NodeID, NodeID> = HashMap::with_capacity(entries.len());
    let mut destinations: HashMap<NodeID, Node> = HashMap::new();
    let mut landing_count: HashMap<NodeID, usize> = HashMap::new();

    for (index, entry) in entries.iter().enumerate() {
        let entry_id = entry.entry_id;
        if entry_id != 0 && !ids.insert(entry_id) {
            return Err(reject(entry_id, index, "two entries share one caller id"));
        }

        let name = entry.dst_name.as_str();
        if name.is_empty() {
            return Err(reject(
                entry_id,
                index,
                "destination name must not be empty",
            ));
        }
        if let Err(error) = validate_node_name_for_store(name) {
            return Err(reject(entry_id, index, &error.to_string()));
        }
        if entry.node_id == ROOT_NODE {
            return Err(reject(
                entry_id,
                index,
                "the root is the revision itself and cannot be moved",
            ));
        }

        let Ok(node) = state.node(context.clone(), entry.node_id).await else {
            return Err(reject(entry_id, index, "node id is unknown"));
        };
        if node.is_discarded() {
            return Err(reject(entry_id, index, "node has been deleted"));
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
                "node is staged for deletion, so there is nothing at that path to move",
            ));
        }

        let destination_parent_id = entry.destination_parent_id;
        if let std::collections::hash_map::Entry::Vacant(slot) =
            destinations.entry(destination_parent_id)
        {
            slot.insert(
                check_destination(state, context, destination_parent_id, entry_id, index).await?,
            );
        }

        let name_hash = hash_string(name);
        if node.parent == destination_parent_id && node.name_hash == name_hash {
            // The hash ignores case, so it takes the stored name to tell a move that
            // changes nothing from a rename that only changes case.
            let current = match state.node_name_clone(context.clone(), entry.node_id).await {
                Ok(current) => current,
                Err(error) => {
                    return Err(reject_internal(
                        entry_id,
                        index,
                        error,
                        "read the node's current name",
                    ));
                }
            };
            if current == name {
                return Err(reject(
                    entry_id,
                    index,
                    "the node is already under that parent by that name",
                ));
            }
        }

        match closes_a_loop(state, context, &moved, destination_parent_id, entry.node_id).await {
            Ok(true) => {
                return Err(reject(
                    entry_id,
                    index,
                    "the destination is the node itself or, once this batch is applied, one of \
                     its descendants",
                ));
            }
            Ok(false) => {}
            Err(error) => {
                return Err(reject_internal(
                    entry_id,
                    index,
                    error,
                    "walk the destination's ancestors",
                ));
            }
        }

        if moved.insert(entry.node_id, destination_parent_id).is_some() {
            return Err(reject(
                entry_id,
                index,
                "two entries move one node; send the destination it should end up at once",
            ));
        }
        *landing_count.entry(destination_parent_id).or_default() += 1;

        planned.push(Planned {
            entry_id,
            entry_index: index,
            node_id: entry.node_id,
            destination_parent_id,
            name_hash,
        });
    }

    let mut claimed: HashSet<(NodeID, u64)> = HashSet::with_capacity(planned.len());
    let mut snapshots: HashMap<NodeID, Vec<(u64, NodeID)>> = HashMap::new();
    for item in &planned {
        if !claimed.insert((item.destination_parent_id, item.name_hash)) {
            return Err(reject(
                item.entry_id,
                item.entry_index,
                "two entries take the same name under one destination parent",
            ));
        }

        let parent_node = destinations[&item.destination_parent_id];
        let holders: Vec<NodeID> = if landing_count[&item.destination_parent_id] > 1 {
            if let std::collections::hash_map::Entry::Vacant(slot) =
                snapshots.entry(item.destination_parent_id)
            {
                match live_children(
                    state,
                    context,
                    item.destination_parent_id,
                    &parent_node,
                    None,
                )
                .await
                {
                    Ok(names) => {
                        slot.insert(names);
                    }
                    Err(error) => {
                        return Err(reject_internal(
                            item.entry_id,
                            item.entry_index,
                            error,
                            "collect the destination's child names",
                        ));
                    }
                }
            }
            snapshot_holders(&snapshots[&item.destination_parent_id], item.name_hash).collect()
        } else {
            match live_children(
                state,
                context,
                item.destination_parent_id,
                &parent_node,
                Some(item.name_hash),
            )
            .await
            {
                Ok(names) => names.into_iter().map(|(_, node_id)| node_id).collect(),
                Err(error) => {
                    return Err(reject_internal(
                        item.entry_id,
                        item.entry_index,
                        error,
                        "search the destination's children for the name",
                    ));
                }
            }
        };

        // A holder this batch moves has been vacating the name since the duplicate
        // check above: the only destination that would keep it there is the one this
        // entry just claimed.
        for holder in holders {
            if holder != item.node_id && !moved.contains_key(&holder) {
                return Err(reject(
                    item.entry_id,
                    item.entry_index,
                    "a child of the destination already holds that name",
                ));
            }
        }
    }

    Ok(planned)
}

/// Move every planned node, in batch order.
///
/// Serial, because a move unlinks the node from one child chain and links it into
/// another, which [`State::move_node`] documents as work that cannot overlap with
/// another move touching either chain. Batch order is also what makes the plan's loop
/// check binding: it settled each entry against the reparenting of the entries before
/// it, which is the tree this phase produces.
///
/// The walk runs to completion whatever fails: an entry reports its own outcome, and
/// the call reports how many entries did not finish, keeping the first failure as the
/// reason. The per-entry terminal carries an error code alone, so the completion detail
/// is the only place a caller learns what the tree refused.
async fn apply_plan(
    args: &LoreRevisionTreeMoveArgs,
    state: Arc<State>,
    context: Arc<RepositoryContext>,
    planned: Vec<Planned>,
) -> Result<(), MoveError> {
    let total = planned.len();
    let mut applied = 0usize;
    let mut failure: Option<StateError> = None;
    for item in &planned {
        let name = entry_name(args, item.entry_index);
        match state
            .move_node(
                context.clone(),
                item.node_id,
                item.destination_parent_id,
                name,
            )
            .await
        {
            Ok(()) => {
                emit_move_complete(item.entry_id, item.node_id, LoreErrorCode::None);
                applied += 1;
            }
            Err(error) => {
                emit_move_error(item.entry_id, LoreErrorCode::Internal);
                failure.get_or_insert(error);
            }
        }
    }

    if let Some(error) = failure {
        let failed = total - applied;
        return Err(MoveError::internal_with_context(
            error,
            &format!("{failed}/{total} node moves failed"),
        ));
    }
    Ok(())
}

/// Move a batch of nodes to new parents and/or new names.
///
/// Each entry emits `RevisionTreeMoveComplete` carrying its own `entry_id` and the node
/// it moved, before the call's `Complete`; on failure the reported node is the
/// invalid-node sentinel. An entry naming the node's current parent renames it where it
/// is. An empty batch succeeds.
///
/// **A move keeps the node.** Its node id, its `file_id` and its children come along,
/// and the change is recorded as a move rather than as a deletion and an addition, so
/// the revision graph carries the node's history across it. The node reports
/// `LORE_NODE_STAGED_ACTION_MOVE` until the commit that freezes the tree, and so does
/// every node under a moved directory — their records do not change, but their paths
/// do, and that is what `lore file history` reads against each of them. Two exceptions:
/// a node this handle **added** is in no revision a move could be recorded against, so
/// it stays staged as an addition wherever it lands; and a node under the moved
/// directory that is staged for **deletion** keeps its deletion, since it is leaving the
/// revision at the commit either way.
///
/// The call as a whole reports on `RevisionTreeBatchComplete`, carrying the call's own
/// `batch_id` and firing exactly once — after any per-entry terminals and before
/// `Complete`. A failure that belongs to the call rather than to one entry is reported
/// only there: an unknown or closed handle, and a move that failed after its entry was
/// accepted.
///
/// Every entry is checked before any node is moved, and a single bad entry rejects the
/// whole call with `INVALID_ARGUMENTS` on that entry's `entry_id`, leaving every node
/// where it was. The reason names the entry's batch index, since `entry_id` may be `0`
/// on several entries at once. Rejected are a node id that is unknown, that addresses a
/// slot holding no node, that has been deleted, that is staged for deletion, or that is
/// the root; a destination parent that is unknown, deleted, staged for deletion, a
/// link, or not a directory; a name that is empty or that the node name table would
/// refuse — one holding `/` or `\`, exactly `..`, a leading NUL, or over a thousand
/// bytes; a destination the node already sits under by the name it already has; a node
/// id another entry in the same batch also moves; and a non-zero `entry_id` used by
/// another entry — `0` means "not correlating this entry" and may repeat.
///
/// **The two rules that read the whole batch.** A destination inside the moved node's
/// own subtree is rejected, and so is one that lands there once the batch is applied —
/// moving A under B and B under A is a loop neither entry shows on its own. A name a
/// live child of the destination already holds is rejected, but a name the batch itself
/// vacates is not: moving `x` out of a directory while moving another node to `x` in it
/// succeeds, and two entries taking one name under one destination reject even though
/// neither collides with the tree. A child staged for deletion holds no name, since the
/// commit that freezes the tree drops it.
///
/// A name that is not valid UTF-8 never reaches the verb: the entry point checks every
/// string the call carries and rejects the call before dispatching it, so no per-entry
/// terminal fires for it.
///
/// Atomicity covers the rules checked here, which is every rule a caller can break
/// through the arguments. A failure after the checks pass — a block that cannot be
/// read, or a tree changing under the call — reports `INTERNAL` for that entry and for
/// the batch, and may leave earlier entries applied: nothing is rolled back, the handle
/// stays usable, and no revision is published until `commit`.
///
/// Entries apply one at a time, in batch order, because a move rewrites the parent and
/// sibling pointers of two chains where `add` only ever prepends to one. Work per entry
/// is proportional to the moved subtree rather than to the one node named, since every
/// node under a moved directory is recorded as moved.
///
/// Concurrent calls are not serialized against each other, and a move is the edit that
/// has the most to lose by it: two calls moving nodes that share a parent chain can
/// interleave their unlinks and leave a node linked under one parent while its record
/// names another. The net is the same one that catches concurrent adds — the pre-commit
/// validator walks every staged directory and refuses a child whose parent link does not
/// lead back to it — so the cost is a rejected commit rather than a published revision
/// that is wrong. Moves that may touch one chain belong in one call, which applies them
/// in order. A commit cannot interleave at all: it claims the handle exclusively, and a
/// move holds a shared claim for its whole batch.
pub async fn move_node(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, move_node_impl).await
}

/// Plan and apply one batch. Split out of the dispatcher closure so the batch
/// terminal fires on every path the batch can take, including an early return.
async fn move_batch(
    internal: Arc<RevisionTreeInternal>,
    args: LoreRevisionTreeMoveArgs,
) -> Result<(), MoveError> {
    if args.entries.is_empty() {
        return Ok(());
    }
    let context = internal.repository_context.clone();
    let access = internal.access_shared().await;
    let state = access.state();
    let planned = plan_entries(&state, &context, args.entries.as_slice()).await?;
    apply_plan(&args, state, context, planned).await
}

async fn move_node_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMoveArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        move_node,
        |args: &LoreRevisionTreeMoveArgs| {
            emit_batch_complete(args.batch_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal: Arc<RevisionTreeInternal>, args: LoreRevisionTreeMoveArgs| {
            let call_id = args.batch_id;
            let result = move_batch(internal, args).await;
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
    use lore_revision::change::FileAction;
    use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreNodeStagedAction;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::metadata::BRANCH;
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::NodeBlock;

    use super::*;
    use crate::revision_tree::add::LoreRevisionTreeAddArgs;
    use crate::revision_tree::add::LoreRevisionTreeAddEntry;
    use crate::revision_tree::add::add;
    use crate::revision_tree::commit::LoreRevisionTreeCommitArgs;
    use crate::revision_tree::commit::LoreRevisionTreeCommitOptions;
    use crate::revision_tree::commit::commit;
    use crate::revision_tree::delete::LoreRevisionTreeDeleteArgs;
    use crate::revision_tree::delete::LoreRevisionTreeDeleteEntry;
    use crate::revision_tree::delete::delete;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
    use crate::revision_tree::list_children::list_children;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use crate::revision_tree::metadata_set::metadata_set;
    use crate::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
    use crate::revision_tree::node_info::node_info;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    /// Call-level id every test batch is submitted under, distinct from the
    /// per-entry ids so the two cannot be confused in an assertion.
    const CALL_ID: u64 = 800;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Complete(i32, String),
        RevisionTreeLoaded(u64),
        AddComplete(u64, NodeID, LoreErrorCode),
        MoveComplete(u64, NodeID, LoreErrorCode),
        BatchComplete(u64, LoreErrorCode),
        Child(NodeID, u32),
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
                LoreEvent::RevisionTreeMoveComplete(data) => {
                    Self::MoveComplete(data.entry_id, data.node_id, data.error_code)
                }
                LoreEvent::RevisionTreeBatchComplete(data) => {
                    Self::BatchComplete(data.batch_id, data.error_code)
                }
                LoreEvent::RevisionTreeChild(data) => Self::Child(data.node_id, data.staged_action),
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

    fn move_outcomes(events: &[CapturedEvent]) -> Vec<(u64, NodeID, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::MoveComplete(id, node_id, code) => Some((*id, *node_id, *code)),
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

    fn entry(
        entry_id: u64,
        node_id: NodeID,
        destination_parent_id: NodeID,
        dst_name: &str,
    ) -> LoreRevisionTreeMoveEntry {
        LoreRevisionTreeMoveEntry {
            entry_id,
            node_id,
            destination_parent_id,
            dst_name: LoreString::from_str(dst_name),
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
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    /// Run one add batch, returning what the call reported.
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

    /// Seed one node under `parent` and return its node id.
    async fn seed(
        handle: LoreRevisionTree,
        parent: NodeID,
        name: &str,
        kind: LoreNodeType,
    ) -> NodeID {
        let (status, events) = run_add(
            handle,
            vec![LoreRevisionTreeAddEntry {
                entry_id: 1,
                parent_node_id: parent,
                parent_entry_index: 0,
                name: LoreString::from_str(name),
                kind: kind as u32,
                mode: 0o644,
                size: 10,
                address: address(1, file_id()),
            }],
        )
        .await;
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

    async fn run_move(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMoveEntry>,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = move_node(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMoveArgs {
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

    async fn run_delete(handle: LoreRevisionTree, node_id: NodeID) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = delete(
            LoreGlobalArgs::default(),
            LoreRevisionTreeDeleteArgs {
                batch_id: 2,
                handle,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeDeleteEntry {
                    entry_id: 1,
                    node_id,
                }]),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "deleting the fixture node must succeed");
    }

    /// Name the branch a commit publishes on. A handle loaded from the zero revision has
    /// no parent revision to read one from and must be told.
    async fn set_branch(handle: LoreRevisionTree, branch: Context) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: 3,
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
        assert_eq!(status, 0, "naming the branch must succeed");
    }

    async fn run_commit(handle: LoreRevisionTree, id: u64) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id,
                handle,
                options: LoreRevisionTreeCommitOptions::default(),
            },
            make_callback(sink.clone()),
        )
        .await;
        let events = sink.lock().unwrap().clone();
        assert_eq!(status, 0, "committing must succeed, got {events:?}");
    }

    /// Every child the listing reports for `parent`, as `(node_id, staged_action)`.
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
                CapturedEvent::Child(node_id, staged_action) => Some((*node_id, *staged_action)),
                _ => None,
            })
            .collect()
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
        events
            .iter()
            .find_map(|event| match event {
                CapturedEvent::NodeInfo(data) => Some((**data).clone()),
                _ => None,
            })
            .expect("node_info must emit a record")
    }

    /// The common move: a node the loaded revision holds leaves one directory for
    /// another, keeping the identity a delete-plus-add pair would have replaced.
    #[tokio::test]
    async fn move_reparents_a_node_and_keeps_its_file_id() {
        let partition = Partition::from([0x61u8; 16]);
        let (handle, store_handle_id) = load_handle("move-reparent", partition).await;

        let source = seed(handle, ROOT_NODE, "src", LoreNodeType::Directory).await;
        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, source, "data.bin", LoreNodeType::File).await;
        settle(handle, &[source, destination, node_id]).await;
        let before = fetch_node_info(handle, node_id).await;

        let (status, events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_eq!(status, 0, "moving a node must succeed");
        assert_eq!(
            move_outcomes(&events),
            vec![(10, node_id, LoreErrorCode::None)],
            "the terminal must echo the moved node"
        );

        let after = fetch_node_info(handle, node_id).await;
        assert_eq!(
            after.parent_id, destination,
            "the node must hang off the destination"
        );
        assert_eq!(
            after.name.as_str(),
            "data.bin",
            "a move that does not rename keeps the name"
        );
        assert_eq!(
            after.file_id, before.file_id,
            "the file id must survive the move, which is what records it as a move"
        );
        assert_eq!(
            after.staged_action,
            LoreNodeStagedAction::Move as u32,
            "the node must report the move it is staged for"
        );
        assert!(
            children_of(handle, source).await.is_empty(),
            "the node must be gone from the parent it left"
        );
        assert_eq!(
            children_of(handle, destination).await,
            vec![(node_id, LoreNodeStagedAction::Move as u32)],
            "the node must be listed under the parent it arrived at"
        );
        release(handle, store_handle_id);
    }

    /// A child of the moved directory that is staged for deletion keeps its deletion.
    /// The action bits hold one change at a time, so recording a move over it would take
    /// the deletion off it and the commit would keep a node the caller removed.
    #[tokio::test]
    async fn move_keeps_a_child_staged_for_deletion_deleted() {
        let partition = Partition::from([0x76u8; 16]);
        let (handle, store_handle_id) = load_handle("move-deleted-child", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
        let kept = seed(handle, directory, "kept.bin", LoreNodeType::File).await;
        let removed = seed(handle, directory, "removed.bin", LoreNodeType::File).await;
        settle(handle, &[destination, directory, kept, removed]).await;
        run_delete(handle, removed).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, directory, destination, "dir")]).await;
        assert_eq!(status, 0, "moving the directory must succeed");

        assert_eq!(
            fetch_node_info(handle, removed).await.staged_action,
            LoreNodeStagedAction::Delete as u32,
            "the deletion must survive the move of the directory holding it"
        );
        assert_eq!(
            fetch_node_info(handle, kept).await.staged_action,
            LoreNodeStagedAction::Move as u32,
            "while a child that is staying is recorded as moved"
        );
        release(handle, store_handle_id);
    }

    /// A link is one node here: its subtree lives in the linked repository's tree, which
    /// this handle does not mutate, so nothing descends into it.
    #[tokio::test]
    async fn move_moves_a_link_as_one_node() {
        let partition = Partition::from([0x77u8; 16]);
        let (handle, store_handle_id) = load_handle("move-link", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let link = seed(handle, ROOT_NODE, "link", LoreNodeType::Link).await;
        settle(handle, &[destination, link]).await;

        let (status, events) = run_move(handle, vec![entry(10, link, destination, "link")]).await;
        assert_eq!(status, 0, "moving a link must succeed");
        assert_eq!(
            move_outcomes(&events),
            vec![(10, link, LoreErrorCode::None)],
            "the link reports as the one node it is"
        );
        assert_eq!(
            fetch_node_info(handle, link).await.parent_id,
            destination,
            "the link must hang off the destination"
        );
        release(handle, store_handle_id);
    }

    /// A link addresses a revision this handle does not mutate, so it cannot take
    /// children — and is refused for that rather than for not being a directory, which
    /// would send a caller looking at the wrong thing.
    #[tokio::test]
    async fn move_rejects_a_link_destination() {
        let partition = Partition::from([0x78u8; 16]);
        let (handle, store_handle_id) = load_handle("move-link-destination", partition).await;

        let link = seed(handle, ROOT_NODE, "link", LoreNodeType::Link).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[link, node_id]).await;

        let (status, events) = run_move(handle, vec![entry(10, node_id, link, "data.bin")]).await;
        assert_ne!(status, 0, "a link destination must be refused");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("is a link"),
            "the link must be refused on its own terms, got {reason:?}"
        );
        release(handle, store_handle_id);
    }

    /// The name rules are the name table's own, applied here so a name it would refuse
    /// fails before the batch moves anything rather than part-way through.
    #[tokio::test]
    async fn move_rejects_a_name_the_name_table_would_refuse() {
        let partition = Partition::from([0x79u8; 16]);
        let (handle, store_handle_id) = load_handle("move-bad-name", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, node_id]).await;

        let (status, events) = run_move(handle, vec![entry(10, node_id, destination, "")]).await;
        assert_ne!(status, 0, "an empty name must be refused");
        assert!(
            rejection_reason(&events).contains("must not be empty"),
            "the empty name must be refused on its own terms"
        );

        let (status, _events) =
            run_move(handle, vec![entry(11, node_id, destination, "with/slash")]).await;
        assert_ne!(
            status, 0,
            "a name the name table would refuse must be refused"
        );
        assert_eq!(
            fetch_node_info(handle, node_id).await.parent_id,
            ROOT_NODE,
            "neither rejection may move anything"
        );
        release(handle, store_handle_id);
    }

    /// Node ids are allocated sequentially and a block holds `BLOCK_NODE_COUNT` of them,
    /// so a batch under 512 entries never leaves block zero: the chain walks, the name
    /// store and the per-entry reads all stay inside one block's locks and one name
    /// table. This is the test that crosses them.
    #[tokio::test]
    async fn move_moves_more_nodes_than_one_block_holds() {
        const BATCH: usize = 3 * BLOCK_NODE_COUNT;

        let partition = Partition::from([0x7au8; 16]);
        let (handle, store_handle_id) = load_handle("move-many-blocks", partition).await;

        let source = seed(handle, ROOT_NODE, "src", LoreNodeType::Directory).await;
        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let names: Vec<String> = (0..BATCH).map(|index| format!("file-{index:04}")).collect();
        let (status, events) = run_add(
            handle,
            names
                .iter()
                .enumerate()
                .map(|(index, name)| LoreRevisionTreeAddEntry {
                    entry_id: index as u64 + 1,
                    parent_node_id: source,
                    parent_entry_index: 0,
                    name: LoreString::from_str(name),
                    kind: LoreNodeType::File as u32,
                    mode: 0o644,
                    size: 10,
                    address: address(1, file_id()),
                })
                .collect(),
        )
        .await;
        assert_eq!(status, 0, "seeding the batch must succeed");
        let seeded: Vec<NodeID> = events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::AddComplete(_, node_id, LoreErrorCode::None) => Some(*node_id),
                _ => None,
            })
            .collect();
        assert_eq!(seeded.len(), BATCH, "every entry must have been created");
        assert!(
            seeded.iter().any(|node_id| NodeBlock::index(*node_id) > 0),
            "the fixture must reach past the first node block"
        );
        settle(handle, &seeded).await;

        // In listing order, so each entry unlinks from the head of the source chain
        // rather than walking it — the batch measures the block boundary, not the walk.
        let listed: Vec<NodeID> = children_of(handle, source)
            .await
            .into_iter()
            .map(|(node_id, _)| node_id)
            .collect();
        let (status, events) = run_move(
            handle,
            listed
                .iter()
                .enumerate()
                .map(|(index, node_id)| {
                    entry(
                        index as u64 + 1,
                        *node_id,
                        destination,
                        &format!("moved-{index:04}"),
                    )
                })
                .collect(),
        )
        .await;
        assert_eq!(status, 0, "moving the batch must succeed");
        assert_eq!(
            move_outcomes(&events).len(),
            BATCH,
            "every entry must report its own terminal"
        );
        assert!(
            children_of(handle, source).await.is_empty(),
            "the source must be left empty"
        );
        assert_eq!(
            children_of(handle, destination).await.len(),
            BATCH,
            "and the destination must hold every moved node"
        );
        let last = fetch_node_info(handle, listed[BATCH - 1]).await;
        assert_eq!(last.parent_id, destination, "including the last of them");
        assert_eq!(
            last.name.as_str(),
            "moved-1535",
            "renamed into a name table that had to grow past the block it started in"
        );
        release(handle, store_handle_id);
    }

    /// An entry can pass validation and be gone by the time the batch reaches it: the
    /// plan phase reads every node, the apply phase then rewrites them, and nothing holds
    /// the tree still in between. Driving the two phases separately puts that
    /// interleaving under the test's control rather than a race's, which is the only way
    /// to reach the apply phase's failure path now that validation catches everything the
    /// arguments can get wrong.
    #[tokio::test]
    async fn an_entry_that_vanishes_after_validation_fails_the_batch_as_internal() {
        let partition = Partition::from([0x7bu8; 16]);
        let (handle, store_handle_id) = load_handle("move-vanishing", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, node_id]).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let args = LoreRevisionTreeMoveArgs {
            batch_id: CALL_ID,
            handle,
            entries: LoreArray::from_vec(vec![entry(10, node_id, destination, "data.bin")]),
        };
        let status = revision_tree_call(
            LoreGlobalArgs::default(),
            make_callback(sink.clone()),
            handle,
            args,
            move_node,
            |_: &LoreRevisionTreeMoveArgs| {},
            async move |internal: Arc<RevisionTreeInternal>, args: LoreRevisionTreeMoveArgs| {
                let planned = plan_entries(
                    &internal.state_for_tests(),
                    &internal.repository_context,
                    args.entries.as_slice(),
                )
                .await?;

                let block_index = NodeBlock::index(node_id);
                let block = internal
                    .state_for_tests()
                    .block(internal.repository_context.clone(), block_index)
                    .await
                    .expect("the block must be readable");
                block
                    .write()
                    .discard_node(block_index, lore_revision::node::Node::index(node_id));

                let result = apply_plan(
                    &args,
                    internal.state_for_tests(),
                    internal.repository_context.clone(),
                    planned,
                )
                .await;
                emit_batch_complete(args.batch_id, batch_error_code(&result));
                result
            },
        )
        .await;

        assert_ne!(
            status, 0,
            "an entry whose node vanished after validation must fail the call"
        );
        let events = sink.lock().unwrap().clone();
        assert_eq!(
            move_outcomes(&events),
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

    /// What keeping the node buys: the delta the commit writes names it under a move
    /// action, so the revision graph carries its history across the change rather than
    /// reading as a deletion and an unrelated addition.
    #[tokio::test]
    async fn a_committed_move_is_recorded_as_a_move_in_the_revision_delta() {
        let partition = Partition::from([0x75u8; 16]);
        let (handle, store_handle_id) = load_handle("move-delta", partition).await;

        let source = seed(handle, ROOT_NODE, "src", LoreNodeType::Directory).await;
        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, source, "data.bin", LoreNodeType::File).await;
        set_branch(handle, Context::from(uuid::Uuid::now_v7())).await;
        run_commit(handle, 1).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_eq!(status, 0, "moving a committed node must succeed");
        run_commit(handle, 2).await;

        // The delta lives in the immutable store, which resolves its session from the
        // execution context a verb installs — so reading it outside one needs a scope.
        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        let execution = crate::call::setup_execution(LoreGlobalArgs::default(), None);
        let delta = lore_base::runtime::LORE_CONTEXT
            .scope(execution, async move {
                internal
                    .state_for_tests()
                    .node_delta(internal.repository_context.clone(), node_id)
                    .await
            })
            .await
            .expect("the published revision must carry a delta")
            .expect("the moved node must be named in it");
        assert_eq!(
            delta.action,
            FileAction::Move as u16,
            "the delta must record a move, not a deletion and an addition"
        );
        release(handle, store_handle_id);
    }

    /// Naming the node's current parent renames it where it is, which is the same
    /// operation with the reparenting left out.
    #[tokio::test]
    async fn move_renames_a_node_where_it_is() {
        let partition = Partition::from([0x62u8; 16]);
        let (handle, store_handle_id) = load_handle("move-rename", partition).await;

        let node_id = seed(handle, ROOT_NODE, "before.bin", LoreNodeType::File).await;
        settle(handle, &[node_id]).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, node_id, ROOT_NODE, "after.bin")]).await;
        assert_eq!(status, 0, "renaming a node must succeed");

        let after = fetch_node_info(handle, node_id).await;
        assert_eq!(
            after.name.as_str(),
            "after.bin",
            "the name must be the new one"
        );
        assert_eq!(after.parent_id, ROOT_NODE, "a rename does not reparent");
        release(handle, store_handle_id);
    }

    /// A name differing only in case is a rename, not a node already where it is:
    /// the name hash ignores case, so the stored name is what decides.
    #[tokio::test]
    async fn move_renames_a_node_that_only_changes_case() {
        let partition = Partition::from([0x63u8; 16]);
        let (handle, store_handle_id) = load_handle("move-case", partition).await;

        let node_id = seed(handle, ROOT_NODE, "readme.md", LoreNodeType::File).await;
        settle(handle, &[node_id]).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, node_id, ROOT_NODE, "README.md")]).await;
        assert_eq!(status, 0, "a case-only rename must succeed");
        assert_eq!(
            fetch_node_info(handle, node_id).await.name.as_str(),
            "README.md",
            "the stored name must take the new case"
        );
        release(handle, store_handle_id);
    }

    /// A moved directory takes its subtree with it. The children's own records do not
    /// change, but their paths do, so each is recorded as moved as well.
    #[tokio::test]
    async fn move_records_the_whole_subtree_as_moved() {
        let partition = Partition::from([0x64u8; 16]);
        let (handle, store_handle_id) = load_handle("move-subtree", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
        let nested = seed(handle, directory, "nested", LoreNodeType::Directory).await;
        let leaf = seed(handle, nested, "deep.bin", LoreNodeType::File).await;
        settle(handle, &[destination, directory, nested, leaf]).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, directory, destination, "dir")]).await;
        assert_eq!(status, 0, "moving a directory must succeed");

        assert_eq!(
            fetch_node_info(handle, nested).await.staged_action,
            LoreNodeStagedAction::Move as u32,
            "a child of the moved directory must be recorded as moved"
        );
        assert_eq!(
            fetch_node_info(handle, leaf).await.staged_action,
            LoreNodeStagedAction::Move as u32,
            "the record must reach the whole subtree, not just its top"
        );
        assert_eq!(
            fetch_node_info(handle, nested).await.parent_id,
            directory,
            "the subtree travels by its parent pointers, which do not change"
        );
        release(handle, store_handle_id);
    }

    /// A node this handle added is in no revision a move could be recorded against,
    /// so it stays staged as an addition wherever it lands.
    #[tokio::test]
    async fn a_node_this_handle_added_stays_an_addition_when_moved() {
        let partition = Partition::from([0x65u8; 16]);
        let (handle, store_handle_id) = load_handle("move-added", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        settle(handle, &[destination]).await;
        let node_id = seed(handle, ROOT_NODE, "fresh.bin", LoreNodeType::File).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, node_id, destination, "fresh.bin")]).await;
        assert_eq!(status, 0, "moving an added node must succeed");
        assert_eq!(
            fetch_node_info(handle, node_id).await.staged_action,
            LoreNodeStagedAction::Add as u32,
            "an addition that moves is still an addition"
        );
        release(handle, store_handle_id);
    }

    /// A node cannot be moved into its own subtree: the subtree would leave the tree
    /// with it, reachable from nothing.
    #[tokio::test]
    async fn move_rejects_a_destination_inside_the_moved_subtree() {
        let partition = Partition::from([0x66u8; 16]);
        let (handle, store_handle_id) = load_handle("move-descendant", partition).await;

        let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
        let nested = seed(handle, directory, "nested", LoreNodeType::Directory).await;
        settle(handle, &[directory, nested]).await;

        let (status, events) = run_move(handle, vec![entry(10, directory, nested, "dir")]).await;
        assert_ne!(
            status, 0,
            "a move into the node's own subtree must be refused"
        );
        assert_eq!(
            move_outcomes(&events),
            vec![(10, INVALID_NODE, LoreErrorCode::InvalidArguments)],
            "the entry must be refused as a bad argument"
        );
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("descendants"),
            "the loop must be refused on its own terms, got {reason:?}"
        );
        assert_eq!(
            fetch_node_info(handle, directory).await.parent_id,
            ROOT_NODE,
            "the refused move must leave the node where it was"
        );
        release(handle, store_handle_id);
    }

    /// Only a directory holds children, so a file cannot be a destination.
    #[tokio::test]
    async fn move_rejects_a_destination_that_is_not_a_directory() {
        let partition = Partition::from([0x67u8; 16]);
        let (handle, store_handle_id) = load_handle("move-file-destination", partition).await;

        let file = seed(handle, ROOT_NODE, "leaf.bin", LoreNodeType::File).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[file, node_id]).await;

        let (status, events) = run_move(handle, vec![entry(10, node_id, file, "data.bin")]).await;
        assert_ne!(status, 0, "a file destination must be refused");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("not a directory"),
            "the destination must be refused for its kind, got {reason:?}"
        );
        release(handle, store_handle_id);
    }

    /// The root is the revision itself: it has no parent to move it under.
    #[tokio::test]
    async fn move_rejects_the_root() {
        let partition = Partition::from([0x68u8; 16]);
        let (handle, store_handle_id) = load_handle("move-root", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        settle(handle, &[destination]).await;

        let (status, events) =
            run_move(handle, vec![entry(10, ROOT_NODE, destination, "root")]).await;
        assert_ne!(status, 0, "the root must not be movable");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("the root is the revision itself"),
            "the root must be refused on its own terms, got {reason:?}"
        );
        release(handle, store_handle_id);
    }

    /// Neither end of a move may name a node that is not there.
    #[tokio::test]
    async fn move_rejects_an_unknown_node_and_an_unknown_destination() {
        let partition = Partition::from([0x69u8; 16]);
        let (handle, store_handle_id) = load_handle("move-unknown", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, node_id]).await;

        let (status, events) = run_move(
            handle,
            vec![entry(10, INVALID_NODE, destination, "data.bin")],
        )
        .await;
        assert_ne!(status, 0, "an unknown node must be refused");
        assert!(
            rejection_reason(&events).contains("node id is unknown"),
            "the node must be refused as unknown"
        );

        let (status, events) =
            run_move(handle, vec![entry(11, node_id, INVALID_NODE, "data.bin")]).await;
        assert_ne!(status, 0, "an unknown destination must be refused");
        assert!(
            rejection_reason(&events).contains("destination parent id is unknown"),
            "the destination must be refused as unknown"
        );
        release(handle, store_handle_id);
    }

    /// Two siblings cannot share a name, so a name the destination already holds is
    /// refused rather than duplicated for the commit to reject later.
    #[tokio::test]
    async fn move_rejects_a_name_the_destination_already_holds() {
        let partition = Partition::from([0x6au8; 16]);
        let (handle, store_handle_id) = load_handle("move-collision", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let occupant = seed(handle, destination, "data.bin", LoreNodeType::File).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, occupant, node_id]).await;

        let (status, events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_ne!(status, 0, "a taken name must be refused");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("already holds that name"),
            "the collision must be refused on its own terms, got {reason:?}"
        );
        assert_eq!(
            fetch_node_info(handle, node_id).await.parent_id,
            ROOT_NODE,
            "the refused move must leave the node where it was"
        );
        release(handle, store_handle_id);
    }

    /// A child staged for deletion leaves the revision at the commit that freezes the
    /// tree, so the name it carries until then is not a name to collide with.
    #[tokio::test]
    async fn move_takes_a_name_only_a_deleted_child_holds() {
        let partition = Partition::from([0x6bu8; 16]);
        let (handle, store_handle_id) = load_handle("move-onto-deleted", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let occupant = seed(handle, destination, "data.bin", LoreNodeType::File).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, occupant, node_id]).await;
        run_delete(handle, occupant).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_eq!(
            status, 0,
            "a name only a node staged for deletion holds must be free"
        );
        assert_eq!(
            fetch_node_info(handle, node_id).await.parent_id,
            destination,
            "the move must have landed"
        );
        release(handle, store_handle_id);
    }

    /// A move that would change nothing is a mistake rather than a no-op: recording it
    /// would report a move the revision graph never made.
    #[tokio::test]
    async fn move_rejects_a_node_already_where_it_is_asked_to_go() {
        let partition = Partition::from([0x6cu8; 16]);
        let (handle, store_handle_id) = load_handle("move-noop", partition).await;

        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[node_id]).await;

        let (status, events) =
            run_move(handle, vec![entry(10, node_id, ROOT_NODE, "data.bin")]).await;
        assert_ne!(status, 0, "a move that changes nothing must be refused");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("already under that parent by that name"),
            "the no-op must be refused on its own terms, got {reason:?}"
        );
        release(handle, store_handle_id);
    }

    /// Two moves that are each legal on their own and jointly a loop: the batch is
    /// settled against the tree it produces, so it is the second entry that is refused.
    #[tokio::test]
    async fn move_rejects_a_batch_whose_entries_form_a_loop() {
        let partition = Partition::from([0x6du8; 16]);
        let (handle, store_handle_id) = load_handle("move-batch-loop", partition).await;

        let first = seed(handle, ROOT_NODE, "a", LoreNodeType::Directory).await;
        let second = seed(handle, ROOT_NODE, "b", LoreNodeType::Directory).await;
        settle(handle, &[first, second]).await;

        let (status, events) = run_move(
            handle,
            vec![entry(10, first, second, "a"), entry(11, second, first, "b")],
        )
        .await;
        assert_ne!(status, 0, "a batch that closes a loop must be refused");
        assert_eq!(
            move_outcomes(&events),
            vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
            "the entry that closes the loop is the one refused"
        );
        assert_eq!(
            fetch_node_info(handle, first).await.parent_id,
            ROOT_NODE,
            "a refused batch leaves every node where it was, including the entry that \
             passed its own checks"
        );
        release(handle, store_handle_id);
    }

    /// Two nodes exchange directories in one call. Neither entry could run alone —
    /// each wants a name the other still holds — so the names are settled against the
    /// tree the batch produces.
    #[tokio::test]
    async fn move_swaps_two_nodes_between_directories() {
        let partition = Partition::from([0x6eu8; 16]);
        let (handle, store_handle_id) = load_handle("move-swap", partition).await;

        let first = seed(handle, ROOT_NODE, "a", LoreNodeType::Directory).await;
        let second = seed(handle, ROOT_NODE, "b", LoreNodeType::Directory).await;
        let from_first = seed(handle, first, "x.bin", LoreNodeType::File).await;
        let from_second = seed(handle, second, "x.bin", LoreNodeType::File).await;
        settle(handle, &[first, second, from_first, from_second]).await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, from_first, second, "x.bin"),
                entry(11, from_second, first, "x.bin"),
            ],
        )
        .await;
        assert_eq!(status, 0, "a swap must succeed");
        assert_eq!(
            move_outcomes(&events),
            vec![
                (10, from_first, LoreErrorCode::None),
                (11, from_second, LoreErrorCode::None),
            ],
            "both entries must report their own node"
        );
        assert_eq!(
            children_of(handle, first).await,
            vec![(from_second, LoreNodeStagedAction::Move as u32)],
            "the second node must have taken the first's place"
        );
        assert_eq!(
            children_of(handle, second).await,
            vec![(from_first, LoreNodeStagedAction::Move as u32)],
            "and the first the second's"
        );
        release(handle, store_handle_id);
    }

    /// A name the batch vacates is free, but a name two entries both want is not: they
    /// would leave one directory holding two children under one name.
    #[tokio::test]
    async fn move_rejects_two_entries_taking_one_name() {
        let partition = Partition::from([0x6fu8; 16]);
        let (handle, store_handle_id) = load_handle("move-batch-collision", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let first = seed(handle, ROOT_NODE, "one.bin", LoreNodeType::File).await;
        let second = seed(handle, ROOT_NODE, "two.bin", LoreNodeType::File).await;
        settle(handle, &[destination, first, second]).await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, first, destination, "same.bin"),
                entry(11, second, destination, "same.bin"),
            ],
        )
        .await;
        assert_ne!(status, 0, "two entries taking one name must be refused");
        assert_eq!(
            move_outcomes(&events),
            vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
            "the second claim on the name is the one refused"
        );
        assert_eq!(
            children_of(handle, destination).await,
            vec![],
            "a refused batch moves nothing"
        );
        release(handle, store_handle_id);
    }

    /// One node, one destination: two entries moving it leave the outcome depending on
    /// which ran last, so the caller sends the destination it wants once.
    #[tokio::test]
    async fn move_rejects_a_repeated_node_id() {
        let partition = Partition::from([0x70u8; 16]);
        let (handle, store_handle_id) = load_handle("move-repeated-node", partition).await;

        let first = seed(handle, ROOT_NODE, "a", LoreNodeType::Directory).await;
        let second = seed(handle, ROOT_NODE, "b", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[first, second, node_id]).await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, node_id, first, "data.bin"),
                entry(11, node_id, second, "data.bin"),
            ],
        )
        .await;
        assert_ne!(status, 0, "moving one node twice must be refused");
        assert_eq!(
            move_outcomes(&events),
            vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
            "the repeat is the entry refused"
        );
        release(handle, store_handle_id);
    }

    /// A caller id correlates one entry, so a repeat would leave two terminals a caller
    /// cannot tell apart. Zero says the entry is not being correlated and may repeat.
    #[tokio::test]
    async fn move_rejects_a_repeated_caller_id_but_accepts_repeated_zeros() {
        let partition = Partition::from([0x71u8; 16]);
        let (handle, store_handle_id) = load_handle("move-repeated-id", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let first = seed(handle, ROOT_NODE, "one.bin", LoreNodeType::File).await;
        let second = seed(handle, ROOT_NODE, "two.bin", LoreNodeType::File).await;
        settle(handle, &[destination, first, second]).await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, first, destination, "one.bin"),
                entry(10, second, destination, "two.bin"),
            ],
        )
        .await;
        assert_ne!(status, 0, "a repeated caller id must be refused");
        assert!(
            rejection_reason(&events).contains("two entries share one caller id"),
            "the repeat must be refused on its own terms"
        );

        let (status, events) = run_move(
            handle,
            vec![
                entry(0, first, destination, "one.bin"),
                entry(0, second, destination, "two.bin"),
            ],
        )
        .await;
        assert_eq!(status, 0, "repeated zero ids must be accepted");
        assert_eq!(
            move_outcomes(&events),
            vec![
                (0, first, LoreErrorCode::None),
                (0, second, LoreErrorCode::None),
            ],
            "both entries must report under the id they were given"
        );
        release(handle, store_handle_id);
    }

    /// Every entry is checked before any node is touched, so one bad entry leaves the
    /// whole batch unapplied — including the node the good entry named.
    #[tokio::test]
    async fn move_rejects_the_whole_batch_and_changes_nothing() {
        let partition = Partition::from([0x72u8; 16]);
        let (handle, store_handle_id) = load_handle("move-atomic", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, node_id]).await;
        let before = fetch_node_info(handle, node_id).await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, node_id, destination, "data.bin"),
                entry(11, INVALID_NODE, destination, "other.bin"),
            ],
        )
        .await;
        assert_ne!(status, 0, "a batch with a bad entry must be refused");
        assert_eq!(
            move_outcomes(&events),
            vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
            "only the offending entry reports a terminal"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
            "the batch terminal must fire exactly once, carrying the call's outcome"
        );

        let after = fetch_node_info(handle, node_id).await;
        assert_eq!(
            after.parent_id, before.parent_id,
            "the node the accepted entry named must not have moved"
        );
        assert_eq!(
            after.file_id, before.file_id,
            "and must keep the identity it had"
        );
        assert_eq!(
            after.staged_action, before.staged_action,
            "a refused batch stages nothing"
        );
        release(handle, store_handle_id);
    }

    /// A batch with nothing in it asks for nothing and gets it, reporting only the
    /// call's own terminal.
    #[tokio::test]
    async fn move_with_no_entries_succeeds() {
        let partition = Partition::from([0x73u8; 16]);
        let (handle, store_handle_id) = load_handle("move-empty", partition).await;

        let (status, events) = run_move(handle, vec![]).await;
        assert_eq!(status, 0, "an empty batch must succeed");
        assert!(
            move_outcomes(&events).is_empty(),
            "no entry means no per-entry terminal"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)],
            "the batch terminal must still fire exactly once"
        );
        release(handle, store_handle_id);
    }

    /// A handle that never resolves has no entries to report against: the failure
    /// belongs to the call, so only the batch terminal carries it.
    #[tokio::test]
    async fn move_on_unknown_handle_reports_only_the_batch_terminal() {
        let (status, events) = run_move(
            LoreRevisionTree::INVALID,
            vec![entry(10, 1, ROOT_NODE, "data.bin")],
        )
        .await;
        assert_ne!(status, 0, "an unknown handle must fail the call");
        assert!(
            move_outcomes(&events).is_empty(),
            "no entry terminal fires when the handle never resolved"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)],
            "the batch terminal must carry the handle failure"
        );
    }

    /// The event contract: every entry's terminal, then exactly one batch terminal,
    /// then the call's `Complete`.
    #[tokio::test]
    async fn move_reports_entries_then_the_batch_terminal_then_complete() {
        let partition = Partition::from([0x74u8; 16]);
        let (handle, store_handle_id) = load_handle("move-ordering", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let first = seed(handle, ROOT_NODE, "one.bin", LoreNodeType::File).await;
        let second = seed(handle, ROOT_NODE, "two.bin", LoreNodeType::File).await;
        settle(handle, &[destination, first, second]).await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, first, destination, "one.bin"),
                entry(11, second, destination, "two.bin"),
            ],
        )
        .await;
        assert_eq!(status, 0, "the batch must succeed");

        let order: Vec<&CapturedEvent> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    CapturedEvent::MoveComplete(..)
                        | CapturedEvent::BatchComplete(..)
                        | CapturedEvent::Complete(..)
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                &CapturedEvent::MoveComplete(10, first, LoreErrorCode::None),
                &CapturedEvent::MoveComplete(11, second, LoreErrorCode::None),
                &CapturedEvent::BatchComplete(CALL_ID, LoreErrorCode::None),
                &CapturedEvent::Complete(0, String::new()),
            ],
            "entries report in batch order, then the batch, then the call"
        );
        release(handle, store_handle_id);
    }

    /// A node staged for deletion is already on its way out of the revision, so moving it
    /// would record a path change for a node the next commit drops.
    #[tokio::test]
    async fn move_rejects_a_node_staged_for_deletion() {
        let partition = Partition::from([0x7cu8; 16]);
        let (handle, store_handle_id) = load_handle("move-deleted-node", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, node_id]).await;
        run_delete(handle, node_id).await;

        let (status, events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_ne!(status, 0, "a node staged for deletion must not be movable");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("staged for deletion"),
            "the deletion must be the reason given, got {reason:?}"
        );
        release(handle, store_handle_id);
    }

    /// A destination staged for deletion takes whatever is moved under it when the commit
    /// drops it — the same rule `add` applies to a parent.
    #[tokio::test]
    async fn move_rejects_a_destination_staged_for_deletion() {
        let partition = Partition::from([0x7du8; 16]);
        let (handle, store_handle_id) = load_handle("move-deleted-destination", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, node_id]).await;
        run_delete(handle, destination).await;

        let (status, events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_ne!(
            status, 0,
            "a destination staged for deletion must be refused"
        );
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("would go with it"),
            "the destination must be refused for where it is heading, got {reason:?}"
        );
        assert_eq!(
            fetch_node_info(handle, node_id).await.parent_id,
            ROOT_NODE,
            "the refused move must leave the node where it was"
        );
        release(handle, store_handle_id);
    }

    /// A discarded node is not merely the wrong kind: discarding replaces its flags
    /// wholesale, so it reads back as an ordinary empty directory and has to be refused on
    /// its own terms or the caller is sent after a node that is simply gone.
    #[tokio::test]
    async fn move_rejects_a_discarded_node() {
        let partition = Partition::from([0x7eu8; 16]);
        let (handle, store_handle_id) = load_handle("move-discarded", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        // Everything this test needs is allocated before the discard: a discarded slot goes
        // straight back on the free list, so the next add would take the very id under test.
        let other = seed(handle, ROOT_NODE, "other.bin", LoreNodeType::File).await;
        settle(handle, &[destination, other]).await;
        let node_id = seed(handle, ROOT_NODE, "fresh.bin", LoreNodeType::File).await;
        run_delete(handle, node_id).await;

        let (status, events) =
            run_move(handle, vec![entry(10, node_id, destination, "fresh.bin")]).await;
        assert_ne!(status, 0, "a discarded node must not be movable");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("has been deleted"),
            "the discard must be the reason given, got {reason:?}"
        );

        let (status, events) = run_move(handle, vec![entry(11, other, node_id, "other.bin")]).await;
        assert_ne!(status, 0, "and must not be a destination either");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("destination parent has been deleted"),
            "the destination end must be refused for the discard too, got {reason:?}"
        );
        release(handle, store_handle_id);
    }

    /// A slot the allocator never handed out reads back zeroed, which is an ordinary
    /// directory — so both ends of a move refuse it as unnamed rather than as a kind, which
    /// is the reason a caller passing a fabricated id needs to see.
    #[tokio::test]
    async fn move_rejects_ids_on_an_unallocated_slot() {
        let partition = Partition::from([0x7fu8; 16]);
        let (handle, store_handle_id) = load_handle("move-unallocated", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[destination, node_id]).await;

        let (status, events) =
            run_move(handle, vec![entry(10, 400, destination, "data.bin")]).await;
        assert_ne!(status, 0, "an unallocated slot must not be movable");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("node id does not resolve to a named node"),
            "the node end must be refused as unnamed, got {reason:?}"
        );

        let (status, events) = run_move(handle, vec![entry(11, node_id, 400, "data.bin")]).await;
        assert_ne!(status, 0, "an unallocated slot must not be a destination");
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("destination parent id does not resolve to a named node"),
            "the destination end must be refused as unnamed too, got {reason:?}"
        );
        release(handle, store_handle_id);
    }

    /// From the second entry landing in one destination, names are checked against a
    /// snapshot of its children rather than a fresh walk. Same rejection, different code
    /// path — and the path a batch filling one directory actually takes.
    #[tokio::test]
    async fn move_rejects_a_taken_name_through_the_destination_snapshot() {
        let partition = Partition::from([0x80u8; 16]);
        let (handle, store_handle_id) = load_handle("move-snapshot-collision", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let occupant = seed(handle, destination, "taken.bin", LoreNodeType::File).await;
        let first = seed(handle, ROOT_NODE, "one.bin", LoreNodeType::File).await;
        let second = seed(handle, ROOT_NODE, "two.bin", LoreNodeType::File).await;
        settle(handle, &[destination, occupant, first, second]).await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, first, destination, "fresh.bin"),
                entry(11, second, destination, "taken.bin"),
            ],
        )
        .await;
        assert_ne!(status, 0, "the taken name must be refused");
        assert_eq!(
            move_outcomes(&events),
            vec![(11, INVALID_NODE, LoreErrorCode::InvalidArguments)],
            "the entry that took it is the one refused"
        );
        assert!(
            rejection_reason(&events).contains("already holds that name"),
            "the collision must be refused on its own terms"
        );
        assert_eq!(
            children_of(handle, destination).await.len(),
            1,
            "a refused batch moves nothing, including the entry that passed"
        );
        release(handle, store_handle_id);
    }

    /// Sibling names collide case-insensitively, because the name hash they are compared
    /// by is case-insensitive — the same rule `add` applies.
    #[tokio::test]
    async fn move_rejects_a_name_a_child_holds_in_another_case() {
        let partition = Partition::from([0x81u8; 16]);
        let (handle, store_handle_id) = load_handle("move-case-collision", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let occupant = seed(handle, destination, "Data.bin", LoreNodeType::File).await;
        let node_id = seed(handle, ROOT_NODE, "other.bin", LoreNodeType::File).await;
        settle(handle, &[destination, occupant, node_id]).await;

        let (status, events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_ne!(
            status, 0,
            "a name differing only in case must still collide"
        );
        assert!(
            rejection_reason(&events).contains("already holds that name"),
            "the collision must be refused on its own terms"
        );
        release(handle, store_handle_id);
    }

    /// A directory this handle added takes a node the revision holds: the pair is what the
    /// commit freezes together, the added directory carrying its own addition and the moved
    /// node its move.
    #[tokio::test]
    async fn move_lands_a_settled_node_under_a_directory_this_handle_added() {
        let partition = Partition::from([0x82u8; 16]);
        let (handle, store_handle_id) = load_handle("move-into-added", partition).await;

        let node_id = seed(handle, ROOT_NODE, "data.bin", LoreNodeType::File).await;
        settle(handle, &[node_id]).await;
        let destination = seed(handle, ROOT_NODE, "fresh-dir", LoreNodeType::Directory).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_eq!(status, 0, "an added directory must be able to take a node");
        assert_eq!(
            children_of(handle, destination).await,
            vec![(node_id, LoreNodeStagedAction::Move as u32)],
            "the node keeps its own record under a parent that is itself an addition"
        );
        assert_eq!(
            fetch_node_info(handle, destination).await.staged_action,
            LoreNodeStagedAction::Add as u32,
            "and the destination stays an addition"
        );
        release(handle, store_handle_id);
    }

    /// The subtree walk reads each node's own staging state rather than inheriting the
    /// parent's, so a child added in this handle stays an addition while the directory
    /// around it becomes a move.
    #[tokio::test]
    async fn an_addition_inside_a_moved_directory_stays_an_addition() {
        let partition = Partition::from([0x83u8; 16]);
        let (handle, store_handle_id) = load_handle("move-added-child", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
        let settled_child = seed(handle, directory, "old.bin", LoreNodeType::File).await;
        settle(handle, &[destination, directory, settled_child]).await;
        let added_child = seed(handle, directory, "new.bin", LoreNodeType::File).await;

        let (status, _events) =
            run_move(handle, vec![entry(10, directory, destination, "dir")]).await;
        assert_eq!(status, 0, "moving the directory must succeed");

        assert_eq!(
            fetch_node_info(handle, added_child).await.staged_action,
            LoreNodeStagedAction::Add as u32,
            "a child this handle added is in no revision a move could be recorded against"
        );
        assert_eq!(
            fetch_node_info(handle, settled_child).await.staged_action,
            LoreNodeStagedAction::Move as u32,
            "while a child the revision holds is recorded as moved"
        );
        release(handle, store_handle_id);
    }

    /// A node can be moved again from where a move left it. The second unlink walks a chain
    /// the first prepended into, which is the case a first move never reaches: the node it
    /// takes out is at the head.
    #[tokio::test]
    async fn a_node_can_be_moved_again_from_where_the_first_move_left_it() {
        let partition = Partition::from([0x84u8; 16]);
        let (handle, store_handle_id) = load_handle("move-twice", partition).await;

        let first_stop = seed(handle, ROOT_NODE, "b", LoreNodeType::Directory).await;
        let second_stop = seed(handle, ROOT_NODE, "c", LoreNodeType::Directory).await;
        let source = seed(handle, ROOT_NODE, "a", LoreNodeType::Directory).await;
        let travelling = seed(handle, source, "x.bin", LoreNodeType::File).await;
        let staying = seed(handle, source, "y.bin", LoreNodeType::File).await;
        settle(
            handle,
            &[first_stop, second_stop, source, travelling, staying],
        )
        .await;
        let file_id = fetch_node_info(handle, travelling).await.file_id;

        let (status, _events) = run_move(
            handle,
            vec![
                entry(10, travelling, first_stop, "x.bin"),
                entry(11, staying, first_stop, "y.bin"),
            ],
        )
        .await;
        assert_eq!(status, 0, "the first move must succeed");
        assert_eq!(
            children_of(handle, first_stop)
                .await
                .into_iter()
                .map(|(node_id, _)| node_id)
                .collect::<Vec<_>>(),
            vec![staying, travelling],
            "the second entry prepends over the first, leaving it at the tail"
        );

        let (status, _events) =
            run_move(handle, vec![entry(12, travelling, second_stop, "x.bin")]).await;
        assert_eq!(
            status, 0,
            "moving it on from the tail of that chain must succeed"
        );
        assert_eq!(
            children_of(handle, first_stop).await,
            vec![(staying, LoreNodeStagedAction::Move as u32)],
            "the chain must close over the node that left it"
        );
        assert_eq!(
            children_of(handle, second_stop).await,
            vec![(travelling, LoreNodeStagedAction::Move as u32)],
            "and the node must be listed where it went"
        );
        assert_eq!(
            fetch_node_info(handle, travelling).await.file_id,
            file_id,
            "two moves are still one identity"
        );
        release(handle, store_handle_id);
    }

    /// The parent a node leaves holds a different child set afterwards, so the address a
    /// commit derives for it changes — and a commit skips rehashing a directory that is not
    /// staged. Nothing else in the move marks it, which is why the move does.
    #[tokio::test]
    async fn the_parent_a_node_left_is_rehashed_by_the_commit() {
        let partition = Partition::from([0x85u8; 16]);
        let (handle, store_handle_id) = load_handle("move-source-rehash", partition).await;

        let source = seed(handle, ROOT_NODE, "src", LoreNodeType::Directory).await;
        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let node_id = seed(handle, source, "data.bin", LoreNodeType::File).await;
        set_branch(handle, Context::from(uuid::Uuid::now_v7())).await;
        run_commit(handle, 1).await;

        let before = fetch_node_info(handle, source).await.address;
        assert!(
            !before.hash.is_zero(),
            "the fixture must start from a directory the commit has hashed"
        );

        let (status, _events) =
            run_move(handle, vec![entry(10, node_id, destination, "data.bin")]).await;
        assert_eq!(status, 0, "moving the file out must succeed");
        run_commit(handle, 2).await;

        assert_ne!(
            fetch_node_info(handle, source).await.address.hash,
            before.hash,
            "the parent that lost the child must be rehashed, or the revision carries a \
             directory hash that no longer describes its children"
        );
        assert!(
            !fetch_node_info(handle, destination)
                .await
                .address
                .hash
                .is_zero(),
            "and the parent that gained it must be hashed too"
        );
        release(handle, store_handle_id);
    }

    /// The snapshot is searched rather than scanned, so the search has to find every node
    /// under a name — settled siblings can share one, which is what the commit's validator
    /// exists to refuse.
    #[test]
    fn snapshot_holders_finds_every_node_under_one_name() {
        let names: Vec<(u64, NodeID)> = vec![(1, 10), (5, 20), (5, 21), (9, 30)];

        assert_eq!(
            snapshot_holders(&names, 5).collect::<Vec<_>>(),
            vec![20, 21],
            "both holders of one name must be found"
        );
        assert_eq!(
            snapshot_holders(&names, 1).collect::<Vec<_>>(),
            vec![10],
            "including the first entry"
        );
        assert_eq!(
            snapshot_holders(&names, 9).collect::<Vec<_>>(),
            vec![30],
            "and the last"
        );
        assert!(
            snapshot_holders(&names, 4).next().is_none(),
            "a name nothing holds finds nothing between the ones that do"
        );
        assert!(
            snapshot_holders(&names, 20).next().is_none(),
            "or past the end"
        );
        assert!(
            snapshot_holders(&[], 1).next().is_none(),
            "an empty destination holds no name at all"
        );
    }

    /// A batch that reorganizes a tree moves a directory and one of its own children in the
    /// same call. The child leaves the subtree its parent's move recorded, so its own entry
    /// has to be what decides where it ends up and what it is recorded as.
    #[tokio::test]
    async fn move_takes_a_directory_and_one_of_its_children_in_one_batch() {
        let partition = Partition::from([0x86u8; 16]);
        let (handle, store_handle_id) = load_handle("move-parent-and-child", partition).await;

        let destination = seed(handle, ROOT_NODE, "dst", LoreNodeType::Directory).await;
        let elsewhere = seed(handle, ROOT_NODE, "other", LoreNodeType::Directory).await;
        let directory = seed(handle, ROOT_NODE, "dir", LoreNodeType::Directory).await;
        let staying = seed(handle, directory, "stays.bin", LoreNodeType::File).await;
        let leaving = seed(handle, directory, "leaves.bin", LoreNodeType::File).await;
        settle(
            handle,
            &[destination, elsewhere, directory, staying, leaving],
        )
        .await;

        let (status, events) = run_move(
            handle,
            vec![
                entry(10, directory, destination, "dir"),
                entry(11, leaving, elsewhere, "leaves.bin"),
            ],
        )
        .await;
        assert_eq!(status, 0, "a batch reorganizing a tree must succeed");
        assert_eq!(
            move_outcomes(&events),
            vec![
                (10, directory, LoreErrorCode::None),
                (11, leaving, LoreErrorCode::None),
            ],
            "both entries report their own node"
        );

        assert_eq!(
            fetch_node_info(handle, directory).await.parent_id,
            destination,
            "the directory must have moved"
        );
        assert_eq!(
            children_of(handle, directory).await,
            vec![(staying, LoreNodeStagedAction::Move as u32)],
            "and taken only the child that had no entry of its own"
        );
        assert_eq!(
            children_of(handle, elsewhere).await,
            vec![(leaving, LoreNodeStagedAction::Move as u32)],
            "while the child with an entry went where its entry said"
        );
        release(handle, store_handle_id);
    }
}
