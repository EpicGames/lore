// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_metadata_set` — record a batch of `(key, value)` pairs
//! on the in-progress revision's metadata. Revision-level rather than per-node,
//! so there is no tree structure to validate and nothing to fan out: the whole
//! batch applies under a single write lock on the handle's pending metadata.

use std::borrow::Cow;
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
use lore_revision::event::revision_tree::LoreRevisionTreeMetadataSetCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::METADATA_MAX_SIZE;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One metadata pair to record. `value` is a typed value that carries its own
/// kind, so there is no separate format tag and nothing to parse.
#[repr(C)]
#[derive(Clone, Debug, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode)]
pub struct LoreRevisionTreeMetadataSetEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `METADATA_SET_COMPLETE`
    pub entry_id: u64,
    /// Metadata key; a later entry naming it overwrites this one
    pub key: LoreString,
    /// Value to store, stored under the kind it carries
    pub value: LoreMetadata,
}

impl Default for LoreRevisionTreeMetadataSetEntry {
    fn default() -> Self {
        Self {
            entry_id: 0,
            key: LoreString::default(),
            value: LoreMetadata::String(LoreString::default()),
        }
    }
}

/// Arguments for `lore_revision_tree_metadata_set`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(metadata_set_impl)]
pub struct LoreRevisionTreeMetadataSetArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Pairs to record; each emits its own `METADATA_SET_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeMetadataSetEntry>,
}

#[error_set]
enum MetadataSetError {
    InvalidArguments,
}

impl MetadataSetError {
    /// A rejection the arguments earned, alongside the generated `internal`
    /// constructor for a failure of ours.
    fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }
}

impl EventError for MetadataSetError {
    fn translated(&self) -> LoreError {
        match self {
            MetadataSetError::InvalidArguments(_) => LoreError::InvalidArguments,
            MetadataSetError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_set_complete(entry_id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeMetadataSetComplete(LoreRevisionTreeMetadataSetCompleteEventData {
        entry_id,
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
fn batch_error_code(result: &Result<(), MetadataSetError>) -> LoreErrorCode {
    match result {
        Ok(()) => LoreErrorCode::None,
        Err(MetadataSetError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(MetadataSetError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// Reject the whole batch as a bad argument, attributing it to `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
fn reject(entry_id: u64, entry_index: usize, reason: &str) -> MetadataSetError {
    emit_set_complete(entry_id, LoreErrorCode::InvalidArguments);
    MetadataSetError::invalid(format!("entry {entry_index}: {reason}"))
}

/// A validated entry, ready to apply without further checks. The value is
/// resolved to its stored bytes here so the apply phase holds the write lock for
/// the writes alone; a kind that already holds those bytes lends them from the
/// arguments rather than copying them.
struct Planned<'a> {
    entry_id: u64,
    entry_index: usize,
    value: Cow<'a, [u8]>,
    value_type: MetadataType,
}

/// Check every entry and encode its value, producing the apply plan. Mutates
/// nothing; the first invalid entry rejects the batch. Entries are held to the
/// size the write itself enforces, so a batch that would not fit fails here
/// rather than with the entries ahead of the offending one already recorded.
///
/// A repeated key is **not** a rejection: entries apply in index order and a
/// later one overwrites an earlier one, which is the contract two separate calls
/// already have. Only a repeated non-zero `entry_id` rejects, since that would
/// make a reported id ambiguous.
fn plan_entries(
    entries: &[LoreRevisionTreeMetadataSetEntry],
) -> Result<Vec<Planned<'_>>, MetadataSetError> {
    let mut planned: Vec<Planned<'_>> = Vec::with_capacity(entries.len());
    let mut ids: HashSet<u64> = HashSet::with_capacity(entries.len());

    for (index, entry) in entries.iter().enumerate() {
        let entry_id = entry.entry_id;
        if entry_id != 0 && !ids.insert(entry_id) {
            return Err(reject(entry_id, index, "two entries share one caller id"));
        }

        if entry.key.as_str().is_empty() {
            return Err(reject(entry_id, index, "key must not be empty"));
        }

        let (value, value_type) = entry.value.to_stored();

        if !Metadata::can_hold(entry.key.as_str(), &value) {
            return Err(reject(
                entry_id,
                index,
                &format!("entry does not fit in a revision's {METADATA_MAX_SIZE} byte metadata"),
            ));
        }

        planned.push(Planned {
            entry_id,
            entry_index: index,
            value,
            value_type,
        });
    }

    Ok(planned)
}

/// Record every planned pair under one write lock.
///
/// Holding the lock across the whole batch is what makes it atomic: no reader
/// sees a half-applied batch, and no concurrent `metadata_set` interleaves its
/// own keys into the middle of this one. There is nothing to fan out — these are
/// in-memory buffer writes, not storage.
fn apply_plan(
    internal: &Arc<RevisionTreeInternal>,
    args: &LoreRevisionTreeMetadataSetArgs,
    planned: Vec<Planned<'_>>,
) -> Result<(), MetadataSetError> {
    let entries = args.entries.as_slice();
    let mut pending = internal.pending_metadata.write();
    for item in planned {
        let key = entries[item.entry_index].key.as_str();
        match pending.set_typed(key, &item.value, item.value_type) {
            Ok(()) => emit_set_complete(item.entry_id, LoreErrorCode::None),
            Err(error) => {
                emit_set_complete(item.entry_id, LoreErrorCode::Internal);
                return Err(MetadataSetError::internal_with_context(
                    error,
                    &format!("entry {}: Metadata::set_typed", item.entry_index),
                ));
            }
        }
    }
    Ok(())
}

/// Record a batch of metadata pairs on the in-progress revision.
///
/// Each entry emits `RevisionTreeMetadataSetComplete` carrying its own
/// `entry_id`, before the call's `Complete`. An empty batch succeeds.
///
/// The call as a whole reports on `RevisionTreeBatchComplete`, carrying the
/// call's own `batch_id` and firing exactly once — after any per-entry
/// terminals and before `Complete`. A failure that belongs to the call rather
/// than to one entry is reported only there: an unknown or closed handle.
///
/// Each value is a typed `LoreMetadata` carrying its own kind, so there is no
/// format tag to disagree with it and no text to parse: a value cannot be stored
/// under a kind it is not, and a binary value can hold any bytes rather than
/// only the ones that happen to be valid text. The sibling verb `metadata_get`
/// returns the same type, so a value round-trips without either side encoding
/// it. The older `lore_revision_metadata_set` takes text plus a parallel format
/// array and parses; this verb deliberately does not.
///
/// Every entry is checked before any pair is recorded, and a single bad entry
/// rejects the whole call with `INVALID_ARGUMENTS` on that entry's `entry_id`,
/// leaving the pending metadata untouched. The reason names the entry's batch
/// index, since `entry_id` may be `0` on several entries at once. Rejected are
/// an empty key, a non-zero `entry_id` used by another entry — `0` means "not
/// correlating this entry" and may repeat — and an entry too large to fit in a
/// revision's metadata at all.
///
/// A **repeated key is not** rejected, unlike the duplicate-target rules on the
/// node verbs. Entries apply in index order, so the last entry naming a key
/// wins — the same result as sending those pairs as separate calls, which a
/// batch is only a compressed form of.
///
/// Nothing is written to storage here. The pairs live in the handle's pending
/// metadata until `commit` serializes them, so `metadata_get` on this handle
/// sees them and no other handle does.
///
/// That pending buffer **is** the new revision's metadata: a commit writes what
/// was set here and nothing else, inheriting no key from the revision the handle
/// was loaded on. A caller that wants a parent's key carried forward reads it
/// with `metadata_get`'s `include_revision` flag and sets it again here.
///
/// **A revision's whole metadata is capped.** The limit is
/// `lore_revision::metadata::METADATA_MAX_SIZE` (1 MiB) and counts the metadata
/// buffer itself — keys, values and per-entry overhead. It does not count
/// anything a value merely refers to: a value holding a content address costs
/// the address, not the content behind it, so the cap bounds how much metadata a
/// revision carries rather than how much data it points at.
///
/// A single entry larger than the whole cap is rejected here, since no amount of
/// removing other keys could make it fit. **The running total is not checked
/// here**, because what a revision ends up carrying is only known once every set
/// has run: a batch of individually legal entries that together push past the
/// limit reports each of them as recorded and fails later, at `commit`.
///
/// The whole batch applies under one write lock on that pending metadata, which
/// is what makes it atomic and is why there is no fan-out: the work is buffer
/// writes, not I/O. Per-entry events are therefore ordered by entry index, and a
/// concurrent `metadata_set` on the same handle cannot interleave its keys into
/// the middle of this batch — though which of two concurrent batches lands
/// second, and so wins a shared key, is not ordered.
///
/// The handle is claimed even though this edits no tree: that pending buffer is
/// what a commit clones and then empties, so an edit landing inside a commit would
/// be recorded and dropped without ever reaching a revision.
pub async fn metadata_set(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_set_impl).await
}

/// Plan and apply one batch. Split out of the dispatcher closure so the batch
/// terminal fires on every path the batch can take, including an early return.
fn metadata_set_batch(
    internal: &Arc<RevisionTreeInternal>,
    args: &LoreRevisionTreeMetadataSetArgs,
) -> Result<(), MetadataSetError> {
    if args.entries.is_empty() {
        return Ok(());
    }
    let planned = plan_entries(args.entries.as_slice())?;
    apply_plan(internal, args, planned)
}

async fn metadata_set_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        metadata_set,
        |args: &LoreRevisionTreeMetadataSetArgs| {
            emit_batch_complete(args.batch_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal: Arc<RevisionTreeInternal>, args: LoreRevisionTreeMetadataSetArgs| {
            let batch_id = args.batch_id;
            let _access = internal.access_shared().await;
            let result = metadata_set_batch(&internal, &args);
            emit_batch_complete(batch_id, batch_error_code(&result));
            result
        },
    )
    .await
}
