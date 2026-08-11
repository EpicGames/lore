// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_add` — add a batch of nodes in one call. An entry
//! parents onto an existing node or onto an earlier entry in the same batch,
//! so a whole subtree lands in a single atomic call.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::runtime::processor_count;
use lore_base::types::Address;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::errors::StateErrors;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeAddCompleteEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeBatchCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::node::validate_node_name_for_store;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::StateNodeChildrenIterator;
use lore_storage::hash::hash_string;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One node to add. The parent is `parent_node_id`, or the node created by an
/// earlier entry when `parent_node_id` is the invalid-node sentinel.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreRevisionTreeAddEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `ADD_COMPLETE`
    pub entry_id: u64,
    /// Parent for the new node; the invalid-node sentinel selects `parent_entry_index`
    pub parent_node_id: NodeID,
    /// Index of an earlier entry in this batch whose new node is the parent;
    /// read only when `parent_node_id` is the invalid-node sentinel
    pub parent_entry_index: u32,
    /// UTF-8 name of the new child within its parent
    pub name: LoreString,
    /// `LoreNodeType` encoding: `DIRECTORY = 0`, `FILE = 1`, `LINK = 2`
    pub kind: u32,
    /// POSIX permission bits for the new node
    pub mode: u16,
    /// Content size in bytes (leaf nodes); `0` for a directory
    pub size: u64,
    /// Content address `(hash, file_id context)` of the new node
    pub address: Address,
}

/// Arguments for `lore_revision_tree_add`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(add_impl)]
pub struct LoreRevisionTreeAddArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Nodes to add; each emits its own `ADD_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeAddEntry>,
}

#[error_set]
enum AddError {
    InvalidArguments,
}

impl AddError {
    /// A rejection the arguments earned, alongside the generated `internal`
    /// constructor for a failure of ours.
    fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }
}

impl EventError for AddError {
    fn translated(&self) -> LoreError {
        match self {
            AddError::InvalidArguments(_) => LoreError::InvalidArguments,
            AddError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_add_complete(entry_id: u64, node_id: NodeID, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeAddComplete(LoreRevisionTreeAddCompleteEventData {
        entry_id,
        node_id,
        error_code,
    })
    .send();
}

/// Emit the `entry_id`-carrying terminal for a failed entry.
fn emit_add_error(entry_id: u64, error_code: LoreErrorCode) {
    emit_add_complete(entry_id, INVALID_NODE, error_code);
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
fn batch_error_code(result: &Result<(), AddError>) -> LoreErrorCode {
    match result {
        Ok(()) => LoreErrorCode::None,
        Err(AddError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(AddError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// Zero the fields a kind does not carry, and assign a file id where one is due.
///
/// A directory's size and address are derived when the revision is committed and
/// a link has no content of its own, so a value supplied for either is dropped
/// rather than stored. A file arriving without a file id is given a generated one
/// so it has a stable identity before commit.
fn fields_for_kind(flags: NodeFlags, size: u64, address: Address) -> (u64, Address) {
    if flags.is_directory() {
        return (0, Address::default());
    }
    if flags.contains(NodeFlags::Link) {
        return (0, address);
    }
    let mut address = address;
    if address.context.is_zero() {
        address.context = uuid::Uuid::now_v7().into();
    }
    (size, address)
}

/// Map a `LoreNodeType` `kind` to its node flags; `None` if unsupported.
fn node_flags_for_kind(kind: u32) -> Option<NodeFlags> {
    if kind == LoreNodeType::File as u32 {
        Some(NodeFlags::File)
    } else if kind == LoreNodeType::Link as u32 {
        Some(NodeFlags::Link)
    } else if kind == LoreNodeType::Directory as u32 {
        Some(NodeFlags::NoFlags)
    } else {
        None
    }
}

/// Where a validated entry hangs: an existing node, or a node another entry in
/// the same batch creates.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ParentRef {
    Existing(NodeID),
    Entry(usize),
}

/// A validated entry, ready to apply without further checks. The name is not
/// copied here: `entry_index` indexes the batch arguments, which own it and
/// outlive the apply phase.
#[derive(Clone, Copy)]
struct Planned {
    entry_id: u64,
    entry_index: usize,
    parent: ParentRef,
    node: Node,
}

/// The name of a planned entry, borrowed from the batch arguments.
fn entry_name(args: &LoreRevisionTreeAddArgs, entry_index: usize) -> &str {
    args.entries.as_slice()[entry_index].name.as_str()
}

fn resolve_parent(parent: ParentRef, created: &[NodeID]) -> NodeID {
    match parent {
        ParentRef::Existing(node_id) => node_id,
        ParentRef::Entry(index) => created[index],
    }
}

/// Reject the whole batch as a bad argument, attributing it to `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
fn reject(entry_id: u64, entry_index: usize, reason: &str) -> AddError {
    emit_add_error(entry_id, LoreErrorCode::InvalidArguments);
    AddError::invalid(format!("entry {entry_index}: {reason}"))
}

/// Reject the whole batch because the tree could not be read, keeping the
/// underlying failure as context.
fn reject_internal(
    entry_id: u64,
    entry_index: usize,
    error: StateError,
    context: &str,
) -> AddError {
    emit_add_error(entry_id, LoreErrorCode::Internal);
    AddError::internal_with_context(error, &format!("entry {entry_index}: {context}"))
}

/// Name hashes of every existing child of `parent`, collected in one walk of the
/// sibling chain and sorted for lookup.
///
/// A sorted `Vec` rather than a set: the values are already hashes, so a hash set
/// would run each of them through the hasher a second time, and would carry
/// bucket overhead on a collection whose size is the parent's child count rather
/// than anything the batch chose.
async fn child_name_hashes(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    parent: NodeID,
    parent_node: &Node,
) -> Result<Vec<u64>, StateError> {
    let mut children =
        StateNodeChildrenIterator::from_parent(state.clone(), context.clone(), parent, parent_node)
            .await?;
    let mut hashes = Vec::new();
    while let Some((_, node)) = children.next().await? {
        hashes.push(node.name_hash);
    }
    hashes.sort_unstable();
    Ok(hashes)
}

/// How far a batch has got with the checks it runs once per existing parent.
#[derive(Clone, Copy, PartialEq)]
enum ParentProgress {
    /// Not seen yet: the parent itself still has to be checked.
    Unchecked,
    /// Checked, and its names are being looked up one at a time.
    Checked,
    /// Its child names have been collected, so later entries check the snapshot.
    Snapshotted,
}

/// What a batch has learned about an existing parent, so the work it drives runs
/// once for the parent rather than once per entry landing under it.
struct ParentState {
    /// The parent as it read when it was checked, so neither a name lookup nor
    /// the snapshot walk has to fetch it again.
    node: Node,
    /// Its child name hashes, sorted, collected once a second entry named it.
    names: Option<Vec<u64>>,
}

/// Check that an existing node can take children, attributing any failure to
/// `entry_id`. Runs once per parent per batch, for the first entry that names it.
///
/// Returns the parent it read, which the caller keeps: every later lookup under
/// this parent starts from the child chain it holds.
async fn check_existing_parent(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    parent: NodeID,
    entry_id: u64,
    entry_index: usize,
) -> Result<Node, AddError> {
    let Ok(parent_node) = state.node(context.clone(), parent).await else {
        return Err(reject(entry_id, entry_index, "parent node id is unknown"));
    };
    // A discarded slot keeps its name and carries neither the file nor the link
    // flag, so it reads back as a perfectly ordinary directory. Nothing below
    // would catch it, and a child hung off one is orphaned as soon as the
    // allocator hands the slot out again.
    if parent_node.is_discarded() {
        return Err(reject(
            entry_id,
            entry_index,
            "parent node has been deleted",
        ));
    }
    if parent_node.is_link() {
        return Err(reject(
            entry_id,
            entry_index,
            "parent node is a link, which addresses a revision this handle does not mutate",
        ));
    }
    if !parent_node.is_directory() {
        return Err(reject(
            entry_id,
            entry_index,
            "parent node is not a directory",
        ));
    }
    // Every non-root node has a non-empty name, so a zero name length means the
    // parent id landed on an unallocated slot rather than a real node.
    if parent != ROOT_NODE && parent_node.name_length == 0 {
        return Err(reject(
            entry_id,
            entry_index,
            "parent node id does not resolve to a named node",
        ));
    }
    Ok(parent_node)
}

/// Check every entry against the tree and against the rest of the batch,
/// producing the apply plan. Mutates nothing; the first invalid entry rejects
/// the batch.
///
/// An existing parent is checked once per batch. Its names are looked up
/// directly for the first entry that lands under it and, from the second entry
/// on, against a snapshot collected in a single chain walk — so a batch filling
/// one directory walks it once instead of once per entry, while a batch touching
/// many parents once each never walks at all.
async fn plan_entries(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    entries: &[LoreRevisionTreeAddEntry],
) -> Result<Vec<Planned>, AddError> {
    let mut existing: HashMap<NodeID, ParentState> = HashMap::new();
    let mut planned: Vec<Planned> = Vec::with_capacity(entries.len());
    let mut taken: HashSet<(ParentRef, u64)> = HashSet::with_capacity(entries.len());
    let mut ids: HashSet<u64> = HashSet::with_capacity(entries.len());

    for (index, entry) in entries.iter().enumerate() {
        let entry_id = entry.entry_id;
        if entry_id != 0 && !ids.insert(entry_id) {
            return Err(reject(entry_id, index, "two entries share one caller id"));
        }

        // Sound because the entry point checked every string the call carries
        // before dispatching it.
        let name = entry.name.as_str();
        if name.is_empty() {
            return Err(reject(entry_id, index, "name must not be empty"));
        }
        // The same rules the name table applies when the node is written, run
        // here so a name it would refuse fails the batch cleanly instead of
        // surfacing part-way through the apply phase with nodes already created.
        if let Err(error) = validate_node_name_for_store(name) {
            return Err(reject(entry_id, index, &error.to_string()));
        }

        let Some(flags) = node_flags_for_kind(entry.kind) else {
            return Err(reject(entry_id, index, "kind is not a supported node type"));
        };

        let parent = if entry.parent_node_id == INVALID_NODE {
            let target = entry.parent_entry_index as usize;
            if target >= index {
                return Err(reject(
                    entry_id,
                    index,
                    "parent entry must refer to an earlier entry in the batch",
                ));
            }
            if !planned[target].node.is_directory() {
                return Err(reject(entry_id, index, "parent entry is not a directory"));
            }
            ParentRef::Entry(target)
        } else {
            let parent_node_id = entry.parent_node_id;
            let progress = match existing.get(&parent_node_id) {
                None => ParentProgress::Unchecked,
                Some(ParentState { names: None, .. }) => ParentProgress::Checked,
                Some(ParentState { names: Some(_), .. }) => ParentProgress::Snapshotted,
            };
            match progress {
                ParentProgress::Unchecked => {
                    let node =
                        check_existing_parent(state, context, parent_node_id, entry_id, index)
                            .await?;
                    existing.insert(parent_node_id, ParentState { node, names: None });
                }
                ParentProgress::Checked => {
                    let node = existing[&parent_node_id].node;
                    match child_name_hashes(state, context, parent_node_id, &node).await {
                        Ok(hashes) => {
                            existing.insert(
                                parent_node_id,
                                ParentState {
                                    node,
                                    names: Some(hashes),
                                },
                            );
                        }
                        Err(error) => {
                            return Err(reject_internal(
                                entry_id,
                                index,
                                error,
                                "collect existing child names",
                            ));
                        }
                    }
                }
                ParentProgress::Snapshotted => {}
            }
            ParentRef::Existing(parent_node_id)
        };

        let name_hash = hash_string(name);
        if !taken.insert((parent, name_hash)) {
            return Err(reject(
                entry_id,
                index,
                "two entries add the same name under one parent",
            ));
        }
        if let ParentRef::Existing(parent_node_id) = parent {
            let snapshot_hit = existing
                .get(&parent_node_id)
                .and_then(|parent| parent.names.as_ref())
                .map(|hashes| hashes.binary_search(&name_hash).is_ok());
            let occupied = if let Some(occupied) = snapshot_hit {
                occupied
            } else {
                let parent_node = existing[&parent_node_id].node;
                match state
                    .find_subnode_of(context.clone(), parent_node_id, &parent_node, name_hash)
                    .await
                {
                    Ok(_) => true,
                    Err(StateErrors::NodeNotFound(_)) => false,
                    Err(error) => {
                        return Err(reject_internal(
                            entry_id,
                            index,
                            error,
                            "State::find_subnode",
                        ));
                    }
                }
            };
            if occupied {
                return Err(reject(
                    entry_id,
                    index,
                    "a child with this name already exists",
                ));
            }
        }

        let (size, address) = fields_for_kind(flags, entry.size, entry.address);

        planned.push(Planned {
            entry_id,
            entry_index: index,
            parent,
            node: Node {
                flags: flags.bits(),
                mode: entry.mode,
                size,
                address,
                name_hash,
                ..Default::default()
            },
        });
    }

    Ok(planned)
}

/// Group the entries by their depth in the batch forest: level zero holds every
/// entry hanging off a node that already exists, and each later level hangs off
/// the one before it.
///
/// Parent references only ever point backwards, so one forward pass settles every
/// depth, and the levels partition the batch — each entry appears in exactly one.
fn entry_levels(planned: &[Planned]) -> Vec<Vec<usize>> {
    let mut depths = vec![0usize; planned.len()];
    let mut levels: Vec<Vec<usize>> = Vec::new();
    for (index, item) in planned.iter().enumerate() {
        let depth = match item.parent {
            ParentRef::Existing(_) => 0,
            ParentRef::Entry(parent) => depths[parent] + 1,
        };
        depths[index] = depth;
        if depth == levels.len() {
            levels.push(Vec::new());
        }
        levels[depth].push(index);
    }
    levels
}

/// Pair each entry in `level` with its resolved parent.
///
/// An entry whose parent entry failed resolves to the invalid node; it is
/// reported here and left out of the wave rather than attempted against a parent
/// that does not exist.
fn take_wave(planned: &[Planned], created: &[NodeID], level: &[usize]) -> Vec<(usize, NodeID)> {
    let mut wave = Vec::with_capacity(level.len());
    for &index in level {
        let parent = resolve_parent(planned[index].parent, created);
        if parent == INVALID_NODE {
            emit_add_error(planned[index].entry_id, LoreErrorCode::Internal);
            continue;
        }
        wave.push((index, parent));
    }
    wave
}

/// Apply one wave of entries, whose parents all exist by now.
///
/// Entries are grouped by resolved parent and the groups spread round-robin over
/// at most one task per processor: one parent's entries run in batch order within
/// their group, because every publish onto a parent contends for the same
/// child-chain head and gains nothing from racing, while separate parents
/// overlap. Capping the tasks keeps a batch touching very many parents from
/// spawning one for each, which would buy nothing — slot allocation is serialized
/// per tree, so the concurrency a wave can use is bounded regardless.
///
/// Records each new node in `created` and returns how many landed; every entry
/// reports its own outcome either way.
async fn apply_wave(
    args: &Arc<LoreRevisionTreeAddArgs>,
    planned: &Arc<Vec<Planned>>,
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    wave: Vec<(usize, NodeID)>,
    created: &mut [NodeID],
) -> usize {
    let mut groups: HashMap<NodeID, Vec<usize>> = HashMap::new();
    for (index, parent) in wave {
        groups.entry(parent).or_default().push(index);
    }

    let task_count = processor_count().min(groups.len()).max(1);
    let mut buckets: Vec<Vec<(NodeID, Vec<usize>)>> = (0..task_count).map(|_| Vec::new()).collect();
    for (slot, group) in groups.into_iter().enumerate() {
        buckets[slot % task_count].push(group);
    }

    let mut tasks: JoinSet<Vec<(usize, NodeID)>> = JoinSet::new();
    for bucket in buckets {
        if bucket.is_empty() {
            continue;
        }
        let capacity = bucket.iter().map(|(_, group)| group.len()).sum();
        let args = args.clone();
        let planned = planned.clone();
        let state = state.clone();
        let context = context.clone();
        lore_spawn!(tasks, async move {
            let mut landed = Vec::with_capacity(capacity);
            for (parent, group) in bucket {
                for index in group {
                    let item = planned[index];
                    let name = entry_name(&args, item.entry_index);
                    match state
                        .node_add(context.clone(), parent, item.node, name)
                        .await
                    {
                        Ok(node_id) => {
                            emit_add_complete(item.entry_id, node_id, LoreErrorCode::None);
                            landed.push((index, node_id));
                        }
                        Err(_) => emit_add_error(item.entry_id, LoreErrorCode::Internal),
                    }
                }
            }
            landed
        });
    }

    let mut applied = 0;
    while let Some(result) = tasks.join_next().await {
        // A task that died mid-group returns nothing; its entries simply do not
        // count as applied, which is what the caller's shortfall measures.
        if let Ok(landed) = result {
            applied += landed.len();
            for (index, node_id) in landed {
                created[index] = node_id;
            }
        }
    }
    applied
}

/// Create the planned nodes, one depth level at a time.
///
/// Everything at a level is independent of everything else at that level — an
/// entry names either a node that already exists or an entry exactly one level
/// up — so a level runs as a single wave, and the barrier between levels keeps a
/// child from running before the parent it names exists. A batch that parents
/// entirely onto existing nodes is one wave. An entry whose parent failed is
/// reported and skipped rather than attempted against a parent that was never
/// created.
///
/// The plan is shared with the wave tasks rather than handed out piecewise, so
/// nothing an entry carries is moved or copied per wave: a wave is a list of
/// indices.
async fn apply_plan(
    args: Arc<LoreRevisionTreeAddArgs>,
    state: Arc<State>,
    context: Arc<RepositoryContext>,
    planned: Vec<Planned>,
) -> Result<(), AddError> {
    let total = planned.len();
    let levels = entry_levels(&planned);
    let planned = Arc::new(planned);
    let mut created = vec![INVALID_NODE; total];
    let mut applied = 0usize;

    for level in &levels {
        let wave = take_wave(&planned, &created, level);
        if !wave.is_empty() {
            applied += apply_wave(&args, &planned, &state, &context, wave, &mut created).await;
        }
    }

    if applied < total {
        let failed = total - applied;
        return Err(AddError::internal(format!(
            "{failed}/{total} node adds failed"
        )));
    }
    Ok(())
}

/// Add a batch of nodes to the tree.
///
/// Each added entry emits `RevisionTreeAddComplete` carrying its own `entry_id`
/// and the new `node_id`, before the call's `Complete`. An entry parents onto
/// `parent_node_id`, or onto the node created by an earlier entry when
/// `parent_node_id` is the invalid-node sentinel and `parent_entry_index` is
/// that entry's position in the batch — so one call builds a subtree. Forward
/// references only, which keeps the batch acyclic. `kind` is a `LoreNodeType`
/// (`DIRECTORY = 0`, `FILE = 1`, `LINK = 2`); a `LINK` entry's `address` is its target
/// `(revision, repository)` and resolves to that revision's root. A file with a
/// zero `address.context` is assigned a generated file id, readable via
/// `node_info`. An empty batch succeeds.
///
/// The call as a whole reports on `RevisionTreeBatchComplete`, carrying the
/// call's own `batch_id` and firing exactly once — after any per-entry
/// terminals and before `Complete`. A failure that belongs to the call rather
/// than to one entry is reported only there: an unknown or closed handle, and an
/// apply task that died without reporting the entries it still held.
///
/// Every entry is checked before any node is created, and a single bad entry
/// rejects the whole call with `INVALID_ARGUMENTS` on that entry's `entry_id`,
/// leaving the tree untouched. The reason names the entry's batch index, since
/// `entry_id` may be `0` on several entries at once. Rejected are a name that is
/// empty or that the node name table would refuse — one holding `/` or `\`,
/// exactly `..`, a leading NUL, or over a thousand bytes; an unsupported `kind`;
/// a parent that is unknown, deleted, a link, not a directory, or not an earlier
/// entry; a name already taken under the parent, whether by an existing child or
/// another entry (case-insensitive); and a non-zero `entry_id` used by another
/// entry — `0` means "not correlating this entry" and may repeat. Fields a kind
/// does not carry are normalised rather than rejected: a directory stores no size
/// and no address, and a link stores no size. A link's target is not resolved
/// here, so an entry naming a revision that cannot be read is accepted and only
/// fails when something later reads through it.
///
/// A name that is not valid UTF-8 never reaches the verb: the entry point checks
/// every string the call carries and rejects the call before dispatching it, so
/// no per-entry terminal fires for it.
///
/// Atomicity covers the rules checked here, which is every rule a caller can
/// break through the arguments. A failure after the checks pass — a block that
/// cannot be read, a tree at its block limit — reports `INTERNAL` and may leave
/// part of the batch created: nothing is rolled back, the handle stays usable,
/// and no revision is published until `commit`.
///
/// Entries are created a depth level at a time — everything parented onto a node
/// that already exists, then everything parented onto those, and so on — and
/// within a level they are grouped by parent and the groups run concurrently, so
/// per-entry events are not ordered by entry index.
///
/// Concurrent calls are not serialized against each other. Two calls that add
/// the same name under the same parent each validate before either applies, so
/// both can succeed and leave duplicate siblings that commit later rejects.
/// Batch adds that may collide into one call, which does reject the duplicate.
///
/// Concurrency across parents covers initializing and publishing a node, not
/// allocating its slot: slot allocation is serialized per loaded tree, so a
/// large batch allocates one node at a time however its parents are spread.
pub async fn add(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeAddArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, add_impl).await
}

/// Plan and apply one batch. Split out of the dispatcher closure so the batch
/// terminal fires on every path the batch can take, including an early return.
async fn add_batch(
    internal: Arc<RevisionTreeInternal>,
    args: LoreRevisionTreeAddArgs,
) -> Result<(), AddError> {
    let args = Arc::new(args);
    if args.entries.is_empty() {
        return Ok(());
    }
    let context = internal.repository_context.clone();
    let planned = plan_entries(&internal.state, &context, args.entries.as_slice()).await?;
    apply_plan(args.clone(), internal.state.clone(), context, planned).await
}

async fn add_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeAddArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        add,
        |args: &LoreRevisionTreeAddArgs| {
            emit_batch_complete(args.batch_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal: Arc<RevisionTreeInternal>, args: LoreRevisionTreeAddArgs| {
            let call_id = args.batch_id;
            let result = add_batch(internal, args).await;
            emit_batch_complete(call_id, batch_error_code(&result));
            result
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
    use lore_revision::node::NodeBlock;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::list_children::LoreRevisionTreeListChildrenArgs;
    use crate::revision_tree::list_children::list_children;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
    use crate::revision_tree::node_info::node_info;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

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
        storage_handle::unregister(crate::storage::handle::LoreStore {
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

    async fn fetch_node_info(
        handle: LoreRevisionTree,
        id: u64,
        node_id: NodeID,
    ) -> Vec<CapturedEvent> {
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

        assert_eq!(status, 1, "a batch with an invalid entry must fail");
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
            status, 1,
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
        assert_eq!(status, 1, "colliding with an existing child must fail");
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
        assert_eq!(forward.0, 1, "a forward parent reference must fail");
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
        assert_eq!(leaf_parent.0, 1, "parenting onto a file entry must fail");
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
        assert_eq!(bad_kind.0, 1, "an unsupported kind must fail");
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
        assert_eq!(unknown.0, 1, "an unknown parent must fail");
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
        assert_eq!(shared_id.0, 1, "two entries sharing a caller id must fail");
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
                .state
                .block(internal.repository_context.clone(), block_index)
                .await
                .expect("the parent block must be readable");
            block.write().discard_node(block_index, Node::index(doomed));
        }

        let (status, events) =
            run_add(handle, vec![entry(2, doomed, "child", LoreNodeType::File)]).await;
        assert_eq!(
            status, 1,
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

        // Enough existing children that the snapshot has to be genuinely ordered
        // to be searchable: with only a couple, an unordered probe finds the
        // collision often enough to pass by luck.
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
                status, 1,
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

        assert_eq!(status, 1, "an unknown handle must fail");
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
        assert!(events.contains(&CapturedEvent::Complete(1)));
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

        let (status, events) =
            run_add(handle, vec![entry(2, ROOT_NODE, "", LoreNodeType::File)]).await;
        assert_eq!(status, 1, "got {events:?}");
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
}
