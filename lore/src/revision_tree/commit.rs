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
#[derive(
    Copy, Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode,
)]
pub struct LoreRevisionTreeCommitOptions {
    /// Also upload the new revision to remote (local-only by default)
    pub remote_write: u8,
}

/// Arguments for `lore_revision_tree_commit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
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
