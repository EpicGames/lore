// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_commit` — freeze the handle's tree, write the 320-
//! byte revision record, and atomically advance the target branch tip. The
//! options struct carries the `remote_write` flag (`u8`, 0 or 1, not
//! `bool`) selecting between local-only and remote-uploading commits.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::error::InvalidArguments;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::commit::CommitError;
use lore_revision::commit::LoreRevisionCommitRevisionEventData;
use lore_revision::commit::commit_in_memory_revision;
use lore_revision::commit::resolve_commit_branch;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeCommitCompleteEventData;
use lore_revision::interface::LoreError;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::ExclusiveAccess;
use crate::revision_tree::handle::IN_MEMORY_MARKER;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;
use crate::storage::store::PerCallFlags;

/// Tuneables for `lore_revision_tree_commit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreRevisionTreeCommitOptions {
    /// Also upload the new revision to remote (local-only by default)
    pub remote_write: u8,
}

/// Arguments for `lore_revision_tree_commit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(commit_impl)]
pub struct LoreRevisionTreeCommitArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to freeze and commit
    pub handle: LoreRevisionTree,
    /// Commit tuneables (local-only vs remote-uploading)
    pub options: LoreRevisionTreeCommitOptions,
}

/// Two variants on purpose: every one a caller can act on through the arguments is
/// `InvalidArguments`, and everything else is `Internal` with the reason in the
/// error detail — which is what `CommitError::translated()` does for the
/// file-system commit, so the same failure reports the same code on both surfaces.
///
/// Nothing finer is worth adding. `LoreErrorCode` has five values and neither a tip
/// collision nor an empty commit is among them, so a third variant here would make
/// the completion status and the terminal's `error_code` disagree about one failure
/// while telling a caller nothing new.
#[error_set]
enum CommitVerbError {
    InvalidArguments,
}

impl EventError for CommitVerbError {
    fn translated(&self) -> LoreError {
        match self {
            CommitVerbError::InvalidArguments(_) => LoreError::InvalidArguments,
            CommitVerbError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// The code the terminal reports for a finished call, matching what the completion
/// status carries.
fn commit_error_code(error: &CommitVerbError) -> LoreErrorCode {
    match error {
        CommitVerbError::InvalidArguments(_) => LoreErrorCode::InvalidArguments,
        CommitVerbError::Internal(_) => LoreErrorCode::Internal,
    }
}

fn emit_commit_complete(
    id: u64,
    revision_hash: Hash,
    new_tip_hash: Hash,
    error_code: LoreErrorCode,
) {
    LoreEvent::RevisionTreeCommitComplete(LoreRevisionTreeCommitCompleteEventData {
        id,
        revision_hash,
        new_tip_hash,
        error_code,
    })
    .send();
}

/// Freeze the handle's tree into a new revision and advance its branch tip.
///
/// The branch is the revision's own, not an argument: `metadata_set("branch", …)`
/// names it, and a key that is set must be either the loaded revision's branch —
/// continuing it — or a branch whose branch point is exactly the loaded revision,
/// which is the first revision on a branch created from it. Unset, it resolves to
/// the loaded revision's branch. A handle loaded from the zero revision has no
/// parent to read one from and must set the key.
///
/// The commit writes exactly the metadata set on the handle and inherits nothing
/// from the revision it was loaded on, plus the three facts about the commit act
/// the caller did not supply: the branch, the timestamp if unset, and
/// `created-by` / `committed-by` if unset. A message is caller metadata like any
/// other — set it with `metadata_set("message", …)` before committing.
///
/// On success `LORE_EVENT_REVISION_TREE_COMMIT_COMPLETE` carries the new revision
/// and `error_code = NONE`, the handle's pending metadata is emptied, and the
/// handle stays usable: the state now *is* the new revision, so previously
/// captured node ids still resolve and further edits commit on top.
///
/// **A commit is all-or-nothing against the handle.** Either it succeeds and the
/// state is consistent on the new revision, or it fails and the state is consistent
/// on the state it had before the call. A failure the call is rejected on — nothing
/// staged, an unusable branch, a tree the validator refuses, or a branch tip that has
/// already moved — writes nothing at all. A failure once the freeze has begun leaves a
/// part-frozen tree, which is discarded and rebuilt from a snapshot taken before the
/// freeze started: the handle comes back on the revision it was on, dirty, with the
/// edits still staged and still committable. Either way the recovery is to fix what
/// the terminal reported and retry **on the same handle** — no close, no reload, no
/// re-applying edits.
///
/// The one failure that still poisons is a restore that itself fails, leaving the
/// handle on a tree neither committed nor restored. It reports `INTERNAL` saying so.
///
/// Neither a tip collision nor an empty commit has a `LoreErrorCode` of its own, so
/// both report `INTERNAL` with the reason in the completion detail — the same codes
/// the file-system commit returns. **A non-zero `new_tip_hash` on the terminal is
/// what identifies a tip collision**, and it carries the tip to reload from so the
/// recovery needs no extra query.
///
/// `options.remote_write = 1` uploads the revision within the call. It is a
/// request, not a guarantee: a handle whose store is bound offline or local-only,
/// or a call passing `globals.local`, silently commits local-only. So does a store
/// opened without a remote configuration — the upload is resolved as requested and
/// there is simply nothing to send it to, and the commit still reports success.
/// Per-call flags that contradict the store's bound flags reject the call.
///
/// **The commit holds the handle for the length of the call**, so no other call on it
/// runs while the tree is being frozen — an edit issued concurrently lands wholly
/// before the commit reads the tree or wholly after it finishes. Two commits on one
/// handle serialize, so the second sees what the first published rather than racing
/// it, and `metadata_set` can no longer lose an edit to the commit's metadata clone.
/// A commit on a large tree therefore blocks reads on that handle for its duration,
/// and a caller whose event callback re-enters the API on the same handle deadlocks —
/// which the callback contract already forbids.
///
/// `remote_write` is resolved onto the handle's shared repository context, so the
/// value outlives the call: the handle carries whatever the last commit resolved.
///
/// Commits from *different* handles, or different processes, still race: the tip
/// compare-and-swap decides which one publishes and the loser fails carrying the tip
/// the winner set, having possibly left orphan tree blocks behind. They are
/// content-addressed and no revision references them.
pub async fn commit(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCommitArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, commit_impl).await
}

async fn commit_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCommitArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        commit,
        |args: &LoreRevisionTreeCommitArgs| {
            emit_commit_complete(
                args.id,
                Hash::default(),
                Hash::default(),
                LoreErrorCode::InvalidArguments,
            );
        },
        async move |internal, args: LoreRevisionTreeCommitArgs| {
            commit_revision(internal, args).await
        },
    )
    .await
}

/// Reject a call whose per-call flags contradict the store's bound flags, and
/// otherwise report whether the commit may upload.
fn resolve_upload(
    internal: &RevisionTreeInternal,
    remote_write: u8,
) -> Result<bool, CommitVerbError> {
    let per_call = PerCallFlags::from_globals(lore_revision::lore::execution_context().globals());
    let effective = internal.store_internal.effective_flags(per_call)?;
    Ok(remote_write != 0 && !effective.no_remote)
}

/// Freeze and publish the revision, holding the handle exclusively for the call.
///
/// The upload flag is written to the handle's shared repository context after the
/// claim rather than before: a second commit resolving its own value while this one
/// runs is the race the exclusive access exists to remove. Resolving the flags stays
/// outside the claim, where a contradiction is rejected without waiting for the
/// handle.
async fn commit_revision(
    internal: Arc<RevisionTreeInternal>,
    args: LoreRevisionTreeCommitArgs,
) -> Result<(), CommitVerbError> {
    let id = args.id;

    let upload = match resolve_upload(&internal, args.options.remote_write) {
        Ok(upload) => upload,
        Err(error) => {
            emit_commit_complete(
                id,
                Hash::default(),
                Hash::default(),
                LoreErrorCode::InvalidArguments,
            );
            return Err(error);
        }
    };
    let repository_context = internal.repository_context.clone();
    let mut access = internal.access_exclusive().await;
    internal.repository_context.set_disable_upload(!upload);
    let state = access.state();
    let current_revision = state.revision();
    let metadata = internal.pending_metadata.read().clone();

    let branch = match resolve_commit_branch(
        repository_context.clone(),
        state.clone(),
        &metadata,
        current_revision,
    )
    .await
    {
        Ok(branch) => branch,
        Err(error) => {
            emit_commit_complete(
                id,
                Hash::default(),
                Hash::default(),
                LoreErrorCode::InvalidArguments,
            );
            return Err(CommitVerbError::from(InvalidArguments {
                reason: error.to_string(),
            }));
        }
    };

    let token = RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER);
    match commit_in_memory_revision(
        repository_context.clone(),
        &token,
        state.clone(),
        metadata,
        current_revision,
        branch,
    )
    .await
    {
        Ok(revision) => {
            *internal.pending_metadata.write() = Metadata::default();
            emit_commit_complete(id, revision, Hash::default(), LoreErrorCode::None);
            emit_commit_telemetry(
                &internal,
                state.revision_number(),
                branch,
                revision,
                current_revision,
            );
            Ok(())
        }
        Err(failure) => {
            let branch_advanced = failure.error.is_branch_advanced();
            let new_tip_hash = if branch_advanced {
                lore_revision::branch::load_latest(repository_context.clone(), branch)
                    .await
                    .unwrap_or_default()
            } else {
                Hash::default()
            };
            let restore_from = failure.restore_from;
            let mut error = map_commit_error(failure.error);
            if !restore_from.is_zero()
                && let Err(restore_failure) = restore_tree(
                    &internal,
                    &mut access,
                    repository_context,
                    restore_from,
                    current_revision,
                    &error,
                )
                .await
            {
                error = restore_failure;
            }
            emit_commit_complete(id, Hash::default(), new_tip_hash, commit_error_code(&error));
            Err(error)
        }
    }
}

/// Put the tree back the way it was before the freeze started rewriting it.
///
/// The part-frozen state is discarded rather than repaired: the snapshot is
/// deserialized into a new `State` and takes its place. `serialize` left the snapshot's
/// signature on itself and cleared its dirty flag, so both are restored — a handle with
/// unserialized edits sitting on the revision it was loaded at is exactly the state the
/// caller had.
///
/// A failure here is the one case that still poisons: the handle is holding a tree
/// neither committed nor restored, and no further call can be trusted against it. It
/// reports `cause` alongside its own failure, because that is what the caller would
/// have had to fix and it is otherwise lost behind the restore.
async fn restore_tree(
    internal: &Arc<RevisionTreeInternal>,
    access: &mut ExclusiveAccess<'_>,
    repository_context: Arc<RepositoryContext>,
    restore_from: Hash,
    current_revision: Hash,
    cause: &CommitVerbError,
) -> Result<(), CommitVerbError> {
    let restored = match State::deserialize(repository_context, restore_from).await {
        Ok(restored) => restored,
        Err(error) => {
            internal.invalid.store(true, Ordering::Release);
            return Err(CommitVerbError::internal_with_context(
                error,
                &format!(
                    "the handle is unusable: restoring the tree failed after the commit failed \
                     with: {cause}"
                ),
            ));
        }
    };
    restored.set_revision(current_revision);
    restored.mark_dirty();
    access.replace(restored);
    Ok(())
}

/// Emit the revision event file-system commit consumers already subscribe to, so a
/// pipeline watching `RevisionCommit*` sees revisions from this surface too.
fn emit_commit_telemetry(
    internal: &RevisionTreeInternal,
    revision_number: u64,
    branch: BranchId,
    revision: Hash,
    parent: Hash,
) {
    LoreEvent::RevisionCommitRevision(LoreRevisionCommitRevisionEventData {
        repository: internal.repository,
        branch,
        revision,
        revision_number,
        parent,
        parent_other: Hash::default(),
    })
    .send();
}

/// Carry a commit failure into the verb's error set: what a caller can fix through
/// the arguments stays `InvalidArguments`, everything else becomes `Internal` with
/// the reason attached. An oversized metadata buffer lands in the latter — the
/// reason names the size, which is what a caller needs to shed keys and retry.
///
/// The `Internal` arm keeps the source error so its trace reaches the completion
/// detail. This is the deepest chain in the namespace — the freeze walks a tree,
/// `rehash_directory` fans out, `serialize` spawns a task per dirty block — so the
/// locations are worth more here than anywhere else. The `InvalidArguments` arm
/// carries a reason rather than a source by construction, which is enough: those
/// are shallow rejections raised before any of that runs.
fn map_commit_error(error: CommitError) -> CommitVerbError {
    if error.is_invalid_arguments() {
        return CommitVerbError::from(InvalidArguments {
            reason: error.to_string(),
        });
    }
    CommitVerbError::internal_with_context(error, "commit_in_memory_revision")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::metadata::BRANCH;
    use lore_revision::metadata::MESSAGE;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::state::State;

    use super::*;
    use crate::revision_tree::add::LoreRevisionTreeAddArgs;
    use crate::revision_tree::add::LoreRevisionTreeAddEntry;
    use crate::revision_tree::add::add;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use crate::revision_tree::metadata_set::metadata_set;
    use crate::revision_tree::modify::LoreRevisionTreeModifyArgs;
    use crate::revision_tree::modify::LoreRevisionTreeModifyEntry;
    use crate::revision_tree::modify::modify;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum Captured {
        Complete(i32),
        Loaded(u64),
        AddComplete(u64, NodeID, LoreErrorCode),
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
                store: crate::storage::handle::LoreStore {
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
        storage_handle::unregister(crate::storage::handle::LoreStore {
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
    async fn modify_file(
        handle: LoreRevisionTree,
        entry_id: u64,
        node_id: NodeID,
        content_hash: u64,
    ) {
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
}
