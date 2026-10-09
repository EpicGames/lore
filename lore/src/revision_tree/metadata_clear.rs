// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_metadata_clear` — remove a batch of keys from the
//! in-progress revision's metadata. Revision-level rather than per-node, so
//! there is no tree structure to validate and nothing to fan out: the whole
//! batch applies under a single write lock on the handle's pending metadata.

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
use lore_revision::event::revision_tree::LoreRevisionTreeMetadataClearCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One metadata key to remove.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode)]
pub struct LoreRevisionTreeMetadataClearEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `METADATA_CLEAR_COMPLETE`
    pub entry_id: u64,
    /// Metadata key to remove; a key that is not set is a no-op
    pub key: LoreString,
}

/// Arguments for `lore_revision_tree_metadata_clear`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(metadata_clear_impl)]
pub struct LoreRevisionTreeMetadataClearArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Keys to remove; each emits its own `METADATA_CLEAR_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeMetadataClearEntry>,
}

#[error_set]
enum MetadataClearError {
    InvalidArguments,
}

impl MetadataClearError {
    /// A rejection the arguments earned, alongside the generated `internal`
    /// constructor for a failure of ours.
    fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }
}

impl EventError for MetadataClearError {
    fn translated(&self) -> LoreError {
        match self {
            MetadataClearError::InvalidArguments(_) => LoreError::InvalidArguments,
            MetadataClearError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_clear_complete(entry_id: u64, removed: bool, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeMetadataClearComplete(LoreRevisionTreeMetadataClearCompleteEventData {
        entry_id,
        removed: u8::from(removed),
        error_code,
    })
    .send();
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
fn batch_error_code(result: &Result<(), MetadataClearError>) -> LoreErrorCode {
    match result {
        Ok(()) => LoreErrorCode::None,
        Err(MetadataClearError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(MetadataClearError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// Reject the whole batch as a bad argument, attributing it to `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
fn reject(entry_id: u64, entry_index: usize, reason: &str) -> MetadataClearError {
    emit_clear_complete(entry_id, false, LoreErrorCode::InvalidArguments);
    MetadataClearError::invalid(format!("entry {entry_index}: {reason}"))
}

/// Check every entry against the rest of the batch. Mutates nothing; the first
/// invalid entry rejects the batch.
///
/// A repeated key is **not** a rejection: the second removal of a key finds it
/// already gone and reports the no-op, which is the same outcome those entries
/// would have had as separate calls.
fn validate_entries(
    entries: &[LoreRevisionTreeMetadataClearEntry],
) -> Result<(), MetadataClearError> {
    let mut ids: HashSet<u64> = HashSet::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let entry_id = entry.entry_id;
        if entry_id != 0 && !ids.insert(entry_id) {
            return Err(reject(entry_id, index, "two entries share one caller id"));
        }
        if entry.key.as_str().is_empty() {
            return Err(reject(entry_id, index, "key must not be empty"));
        }
    }
    Ok(())
}

/// Remove every named key under one write lock.
///
/// Holding the lock across the whole batch is what makes it atomic: no reader
/// sees a half-applied batch, and no concurrent metadata write interleaves with
/// this one. There is nothing to fan out — these are in-memory buffer edits, not
/// storage.
fn apply_entries(
    internal: &Arc<RevisionTreeInternal>,
    entries: &[LoreRevisionTreeMetadataClearEntry],
) {
    let mut pending = internal.pending_metadata.write();
    for entry in entries {
        let removed = pending.remove_key(entry.key.as_str());
        emit_clear_complete(entry.entry_id, removed, LoreErrorCode::None);
    }
}

/// Remove a batch of keys from the in-progress revision's metadata.
///
/// Each entry emits `RevisionTreeMetadataClearComplete` carrying its own
/// `entry_id`, before the call's `Complete`. An empty batch succeeds.
///
/// **Clearing a key that is not set is a no-op success**, not a failure. The
/// terminal's `removed` field says which happened: `1` when the key was there
/// and is now gone, `0` when there was nothing to remove. A caller that only
/// wants the key absent can ignore it; one reconciling state can use it.
///
/// The call as a whole reports on `RevisionTreeBatchComplete`, carrying the
/// call's own `batch_id` and firing exactly once — after any per-entry
/// terminals and before `Complete`. A failure that belongs to the call rather
/// than to one entry is reported only there: an unknown or closed handle.
///
/// Every entry is checked before any key is removed, and a single bad entry
/// rejects the whole call with `INVALID_ARGUMENTS` on that entry's `entry_id`,
/// leaving the pending metadata untouched. The reason names the entry's batch
/// index, since `entry_id` may be `0` on several entries at once. Rejected are
/// an empty key and a non-zero `entry_id` used by another entry — `0` means
/// "not correlating this entry" and may repeat.
///
/// A **repeated key is not** rejected, matching `metadata_set`: the second entry
/// naming it finds it already gone and reports `removed = 0`, which is what those
/// entries would have done as separate calls.
///
/// This clears the metadata of the **revision being built** — the same buffer
/// `metadata_set` records into and `commit` writes. There is nothing inherited
/// to mask: a handle starts with no metadata of its own regardless of what the
/// revision it was loaded on carries, so `removed = 0` means the key was never
/// set here, not that it is hiding in the parent. Reading what the parent
/// recorded is `metadata_get`'s `include_revision` flag, and clearing does not
/// and cannot affect it — that revision is immutable.
///
/// The whole batch applies under one write lock on the pending metadata, which
/// is what makes it atomic and is why there is no fan-out: the work is buffer
/// edits, not I/O. Per-entry events are therefore ordered by entry index.
///
/// The handle is claimed even though this edits no tree: that pending buffer is
/// what a commit clones and then empties, so a clear landing inside a commit would
/// apply to metadata the commit has already taken.
pub async fn metadata_clear(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_clear_impl).await
}

/// Validate and apply one batch. Split out of the dispatcher closure so the
/// batch terminal fires on every path the batch can take, including an early
/// return.
fn metadata_clear_batch(
    internal: &Arc<RevisionTreeInternal>,
    args: &LoreRevisionTreeMetadataClearArgs,
) -> Result<(), MetadataClearError> {
    let entries = args.entries.as_slice();
    if entries.is_empty() {
        return Ok(());
    }
    validate_entries(entries)?;
    apply_entries(internal, entries);
    Ok(())
}

async fn metadata_clear_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataClearArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        metadata_clear,
        |args: &LoreRevisionTreeMetadataClearArgs| {
            emit_batch_complete(args.batch_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal: Arc<RevisionTreeInternal>,
                    args: LoreRevisionTreeMetadataClearArgs| {
            let batch_id = args.batch_id;
            let _access = internal.access_shared().await;
            let result = metadata_clear_batch(&internal, &args);
            emit_batch_complete(batch_id, batch_error_code(&result));
            result
        },
    )
    .await
}
