// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_modify` — rewrite a batch of file nodes' `mode`, `size`
//! and `address` in place. Entries name nodes that already exist and touch no
//! parent or sibling chain, so the whole batch applies concurrently with no
//! sequential phase.

use std::collections::HashSet;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::runtime::processor_count;
use lore_base::types::Address;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeBatchCompleteEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeModifyCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One node to rewrite. The node must already exist and be a file.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode,
)]
pub struct LoreRevisionTreeModifyEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `MODIFY_COMPLETE`
    pub entry_id: u64,
    /// Leaf node to rewrite; non-leaf targets are rejected
    pub node_id: NodeID,
    /// New POSIX permission bits
    pub mode: u16,
    /// New content size in bytes
    pub size: u64,
    /// New content address; a zero `context` preserves the node's file id
    pub address: Address,
}

/// Arguments for `lore_revision_tree_modify`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(modify_impl)]
pub struct LoreRevisionTreeModifyArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Nodes to rewrite; each emits its own `MODIFY_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeModifyEntry>,
}

#[lore_macro::test_pub]
#[error_set]
enum ModifyError {
    InvalidArguments,
}

impl ModifyError {
    /// A rejection the arguments earned, alongside the generated `internal`
    /// constructor for a failure of ours.
    fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }
}

impl EventError for ModifyError {
    fn translated(&self) -> LoreError {
        match self {
            ModifyError::InvalidArguments(_) => LoreError::InvalidArguments,
            ModifyError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_modify_complete(entry_id: u64, node_id: NodeID, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeModifyComplete(LoreRevisionTreeModifyCompleteEventData {
        entry_id,
        node_id,
        error_code,
    })
    .send();
}

/// Emit the `entry_id`-carrying terminal for a failed entry.
fn emit_modify_error(entry_id: u64, error_code: LoreErrorCode) {
    emit_modify_complete(entry_id, INVALID_NODE, error_code);
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
fn batch_error_code(result: &Result<(), ModifyError>) -> LoreErrorCode {
    match result {
        Ok(()) => LoreErrorCode::None,
        Err(ModifyError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(ModifyError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// Reject the whole batch as a bad argument, attributing it to `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
fn reject(entry_id: u64, entry_index: usize, reason: &str) -> ModifyError {
    emit_modify_error(entry_id, LoreErrorCode::InvalidArguments);
    ModifyError::invalid(format!("entry {entry_index}: {reason}"))
}

/// A validated entry, ready to apply without further checks.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct Planned {
    entry_id: u64,
    node_id: NodeID,
    mode: u16,
    size: u64,
    address: Address,
    /// The staged and dirty change the rewrite records, decided while the plan
    /// phase had the node in hand.
    staged: NodeFlags,
    dirty: NodeFlags,
}

/// Check every entry against the tree and against the rest of the batch,
/// producing the apply plan. Mutates nothing; the first invalid entry rejects
/// the batch.
///
/// A discarded slot and a slot the allocator never handed out both read back as
/// ordinary directories, so each is refused on its own terms rather than left to
/// the kind check, which would report the wrong reason. Since every allocated
/// node has a name, a zero name length is what separates the second from a real
/// node.
#[lore_macro::test_pub]
async fn plan_entries(
    state: &Arc<State>,
    context: &Arc<RepositoryContext>,
    entries: &[LoreRevisionTreeModifyEntry],
) -> Result<Vec<Planned>, ModifyError> {
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
                "two entries modify one node; send the intended final value once",
            ));
        }

        let Ok(node) = state.node(context.clone(), entry.node_id).await else {
            return Err(reject(entry_id, index, "node id is unknown"));
        };
        if node.is_discarded() {
            return Err(reject(entry_id, index, "node has been deleted"));
        }
        if node.is_staged_delete() {
            return Err(reject(entry_id, index, "node is staged for deletion"));
        }
        if entry.node_id != ROOT_NODE && node.name_length == 0 {
            return Err(reject(
                entry_id,
                index,
                "node id does not resolve to a named node",
            ));
        }
        if !node.is_file() {
            return Err(reject(
                entry_id,
                index,
                "only a file carries content to modify: a directory's is derived at commit, and a link's address is its target",
            ));
        }

        let (staged, dirty) = State::staged_edit_flags(&node);
        planned.push(Planned {
            entry_id,
            node_id: entry.node_id,
            mode: entry.mode,
            size: entry.size,
            address: entry.address,
            staged,
            dirty,
        });
    }

    Ok(planned)
}

/// Rewrite every planned node.
///
/// Entries are independent — each rewrites fields on a slot that already exists
/// and touches no parent or sibling chain — so they run over at most one task per
/// processor with no barrier between them. Capping the tasks keeps a large batch
/// from spawning one per entry for work that is a short critical section under a
/// block lock.
///
/// The plan is shared with the tasks rather than handed out piecewise, and a task
/// takes a contiguous range of it, so dividing the work moves no entry data and
/// allocates nothing per entry.
#[lore_macro::test_pub]
async fn apply_plan(
    state: Arc<State>,
    context: Arc<RepositoryContext>,
    planned: Vec<Planned>,
) -> Result<(), ModifyError> {
    let total = planned.len();
    let task_count = processor_count().min(total).max(1);
    let chunk = total.div_ceil(task_count);
    let planned = Arc::new(planned);

    let mut tasks: JoinSet<usize> = JoinSet::new();
    for start in (0..total).step_by(chunk) {
        let end = (start + chunk).min(total);
        let planned = planned.clone();
        let state = state.clone();
        let context = context.clone();
        lore_spawn!(tasks, async move {
            let mut applied = 0;
            for item in &planned[start..end] {
                let rewritten = state
                    .node_modify(
                        context.clone(),
                        item.node_id,
                        item.mode,
                        item.size,
                        item.address,
                    )
                    .await;
                let outcome = match rewritten {
                    Ok(()) => {
                        state
                            .node_mark_staged(
                                context.clone(),
                                item.node_id,
                                item.staged,
                                item.dirty,
                            )
                            .await
                    }
                    Err(error) => Err(error),
                };
                match outcome {
                    Ok(()) => {
                        emit_modify_complete(item.entry_id, item.node_id, LoreErrorCode::None);
                        applied += 1;
                    }
                    Err(_) => emit_modify_error(item.entry_id, LoreErrorCode::Internal),
                }
            }
            applied
        });
    }

    let mut applied = 0usize;
    while let Some(result) = tasks.join_next().await {
        if let Ok(count) = result {
            applied += count;
        }
    }

    if applied < total {
        let failed = total - applied;
        return Err(ModifyError::internal(format!(
            "{failed}/{total} node modifies failed"
        )));
    }
    Ok(())
}

/// Rewrite a batch of file nodes' content fields.
///
/// Each entry emits `RevisionTreeModifyComplete` carrying its own `entry_id` and
/// the node it names, before the call's `Complete`; on failure the reported node is
/// the invalid-node sentinel. `mode`, `size` and `address` take the values the
/// entry supplies. Only a file is modifiable: a directory's size and address are
/// derived at commit and a link's address is its target, so neither holds
/// content to rewrite. A zero `address.context` preserves the node's existing
/// file id — unlike `add`, which generates one — because the node already
/// carries an identity and replacing it would record the edit as a move. An
/// empty batch succeeds.
///
/// The call as a whole reports on `RevisionTreeBatchComplete`, carrying the
/// call's own `batch_id` and firing exactly once — after any per-entry
/// terminals and before `Complete`. A failure that belongs to the call rather
/// than to one entry is reported only there: an unknown or closed handle, and an
/// apply task that died without reporting the entries it still held.
///
/// Every entry is checked before any node is rewritten, and a single bad entry
/// rejects the whole call with `INVALID_ARGUMENTS` on that entry's `entry_id`,
/// leaving every target untouched. The reason names the entry's batch index, since
/// `entry_id` may be `0` on several entries at once. Rejected are a node id
/// that is
/// unknown, that addresses a slot holding no node, that has been deleted, or
/// that names a directory or a link; a node id another entry in the same batch
/// also names; and a non-zero `entry_id` used by another entry — `0` means
/// "not correlating this entry" and may repeat. A deleted node and an
/// unallocated slot both read back as an ordinary directory, so each is refused
/// under its own reason rather than as the wrong kind.
///
/// Atomicity covers the rules checked here, which is every rule a caller can
/// break through the arguments. A failure after the checks pass — a block that
/// cannot be read, or a target deleted between its check and its write — reports
/// `INTERNAL` and may leave part of the batch rewritten: nothing is rolled back,
/// the handle stays usable, and no revision is published until `commit`.
///
/// Entries are independent, since a rewrite touches no parent or sibling chain,
/// so the batch is split into contiguous ranges over at most one task per
/// processor and runs with no barrier between them: per-entry events are not
/// ordered by entry index.
///
/// Concurrent calls are not serialized against each other. Two calls rewriting
/// one node race and it keeps whichever wrote last; batch edits that may collide
/// into one call, which rejects the duplicate.
///
/// Concurrency covers entries in different node blocks: each rewrite takes its
/// target's block write lock for the field write, so entries sharing a block —
/// which sequentially allocated node ids usually do — serialize on it.
pub async fn modify(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeModifyArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, modify_impl).await
}

/// Plan and apply one batch. Split out of the dispatcher closure so the batch
/// terminal fires on every path the batch can take, including an early return.
async fn modify_batch(
    internal: Arc<RevisionTreeInternal>,
    args: LoreRevisionTreeModifyArgs,
) -> Result<(), ModifyError> {
    if args.entries.is_empty() {
        return Ok(());
    }
    let context = internal.repository_context.clone();
    let access = internal.access_shared().await;
    let state = access.state();
    let planned = plan_entries(&state, &context, args.entries.as_slice()).await?;
    apply_plan(state, context, planned).await
}

async fn modify_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeModifyArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        modify,
        |args: &LoreRevisionTreeModifyArgs| {
            emit_batch_complete(args.batch_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal: Arc<RevisionTreeInternal>, args: LoreRevisionTreeModifyArgs| {
            let call_id = args.batch_id;
            let result = modify_batch(internal, args).await;
            emit_batch_complete(call_id, batch_error_code(&result));
            result
        },
    )
    .await
}
