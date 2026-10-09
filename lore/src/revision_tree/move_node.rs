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

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One node to move. The node must already exist and must not be the root.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(move_node_impl)]
pub struct LoreRevisionTreeMoveArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Nodes to move; each emits its own `MOVE_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeMoveEntry>,
}

#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
