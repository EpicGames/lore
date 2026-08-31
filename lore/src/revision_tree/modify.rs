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
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One node to rewrite. The node must already exist and be a file.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
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
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(modify_impl)]
pub struct LoreRevisionTreeModifyArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Nodes to rewrite; each emits its own `MODIFY_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeModifyEntry>,
}

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
fn emit_batch_complete(batch_id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeBatchComplete(LoreRevisionTreeBatchCompleteEventData {
        batch_id,
        error_code,
    })
    .send();
}

/// The code the batch terminal reports for a finished call.
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::node::Node;
    use lore_revision::node::NodeBlock;
    use lore_revision::node::ROOT_NODE;

    use super::*;
    use crate::revision_tree::add::LoreRevisionTreeAddArgs;
    use crate::revision_tree::add::LoreRevisionTreeAddEntry;
    use crate::revision_tree::add::add;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::node_info::LoreRevisionTreeNodeInfoArgs;
    use crate::revision_tree::node_info::node_info;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

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
        storage_handle::unregister(crate::storage::handle::LoreStore {
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
            .write()
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

        let (status, events) =
            run_modify(handle, vec![entry(10, node_id), entry(11, node_id)]).await;
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
                    .write()
                    .discard_node(block_index, Node::index(node_id));

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
}
