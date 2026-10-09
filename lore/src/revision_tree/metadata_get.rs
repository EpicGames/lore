// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_metadata_get` — read a batch of metadata values by key.
//! Each key is looked up in the handle's pending edits first, then in the loaded
//! revision's frozen Metadata fragment. An absent key emits no value event.

use std::collections::HashSet;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::LoreMetadataEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeBatchCompleteEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeMetadataGetCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataError;
use lore_revision::metadata::MetadataType;
use lore_revision::state::State;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One metadata key to read.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode)]
pub struct LoreRevisionTreeMetadataGetEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `METADATA_GET_COMPLETE`
    pub entry_id: u64,
    /// Metadata key to read
    pub key: LoreString,
}

/// Arguments for `lore_revision_tree_metadata_get`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(metadata_get_impl)]
pub struct LoreRevisionTreeMetadataGetArgs {
    /// Caller-chosen id echoed back as `batch_id` on `BATCH_COMPLETE`
    pub batch_id: u64,
    /// Loaded revision-tree handle to read from
    pub handle: LoreRevisionTree,
    /// `0` reads only the revision being built; `1` also falls back to the
    /// loaded revision for a key the handle has no entry for
    pub include_revision: u8,
    /// Keys to read; a key that resolves emits its own `METADATA_GET_COMPLETE`
    pub entries: LoreArray<LoreRevisionTreeMetadataGetEntry>,
}

#[error_set]
enum MetadataGetError {
    InvalidArguments,
}

impl MetadataGetError {
    /// A rejection the arguments earned, alongside the generated `internal`
    /// constructor for a failure of ours.
    fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }
}

impl EventError for MetadataGetError {
    fn translated(&self) -> LoreError {
        match self {
            MetadataGetError::InvalidArguments(_) => LoreError::InvalidArguments,
            MetadataGetError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_get_complete(entry_id: u64, key: &str, value: LoreMetadata, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeMetadataGetComplete(LoreRevisionTreeMetadataGetCompleteEventData {
        entry_id,
        key: LoreString::from(key),
        value,
        error_code,
    })
    .send();
}

/// The value an entry carries when it has none to report.
///
/// This event's value field has no "absent" variant, and every kind it does
/// have is a value some key could legitimately hold — so no placeholder can be
/// read as "nothing". `error_code` is what says the entry carries no value; one
/// placeholder used on every such path is what keeps a caller from reading
/// meaning into which one it got.
fn no_value() -> LoreMetadata {
    LoreMetadata::String(LoreString::default())
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
fn batch_error_code(result: &Result<(), MetadataGetError>) -> LoreErrorCode {
    match result {
        Ok(()) => LoreErrorCode::None,
        Err(MetadataGetError::InvalidArguments(_)) => LoreErrorCode::InvalidArguments,
        Err(MetadataGetError::Internal(_)) => LoreErrorCode::Internal,
    }
}

/// Reject the whole batch as a bad argument, attributing it to `entry_id`.
///
/// The batch index goes into the reason as well, because a caller may leave
/// `entry_id` at zero — which any number of entries may share — so the id on its
/// own need not say which entry was at fault.
///
/// The terminal carries the offending key so a caller can tell which entry it
/// was about, and [`no_value`] in place of a value it has not got.
fn reject(entry_id: u64, entry_index: usize, key: &str, reason: &str) -> MetadataGetError {
    emit_get_complete(entry_id, key, no_value(), LoreErrorCode::InvalidArguments);
    MetadataGetError::invalid(format!("entry {entry_index}: {reason}"))
}

/// Check every entry against the rest of the batch. Reads nothing; the first
/// invalid entry rejects the call.
///
/// Nothing is mutated so there is nothing to roll back, but a bad argument is
/// the caller's mistake rather than an absent key, so it still fails the call.
fn validate_entries(entries: &[LoreRevisionTreeMetadataGetEntry]) -> Result<(), MetadataGetError> {
    let mut ids: HashSet<u64> = HashSet::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let entry_id = entry.entry_id;
        let key = entry.key.as_str();
        if entry_id != 0 && !ids.insert(entry_id) {
            return Err(reject(
                entry_id,
                index,
                key,
                "two entries share one caller id",
            ));
        }
        if key.is_empty() {
            return Err(reject(entry_id, index, key, "key must not be empty"));
        }
    }
    Ok(())
}

/// What one key resolved to, owned so the pending read lock is released before
/// the loaded revision's fragment is fetched.
enum Resolved {
    /// The key is in neither source.
    Absent,
    /// The key's value and the kind it is stored under.
    Value(Vec<u8>, MetadataType),
    /// The key is there, under a kind this build cannot read.
    Undecodable,
}

/// Read one key, keeping a key that is not there apart from one whose kind this
/// build does not know. Both fail the lookup, and only the first of them means
/// the caller may be told nothing at all.
fn resolve_key(metadata: &Metadata, key: &str) -> Resolved {
    match metadata.get_typed(key) {
        Ok((value, value_type)) => Resolved::Value(value.to_vec(), value_type),
        Err(MetadataError::FileNotFound(_)) => Resolved::Absent,
        Err(_) => Resolved::Undecodable,
    }
}

/// Look every key up in the revision being built, copying out what it finds.
///
/// The values are copied so the lock is not held across the loaded revision's
/// `await`; only a key that resolves here pays for a copy. Kept even on the
/// default path, which never awaits: every reported key already allocates an
/// owning copy of itself for the event, so borrowing the value instead would
/// save one allocation of the pair and cost a second code path.
fn resolve_from_pending(
    internal: &Arc<RevisionTreeInternal>,
    entries: &[LoreRevisionTreeMetadataGetEntry],
) -> Vec<Resolved> {
    let pending = internal.pending_metadata.read();
    entries
        .iter()
        .map(|entry| resolve_key(&pending, entry.key.as_str()))
        .collect()
}

/// Fill in the keys the revision being built did not answer, from the revision
/// the handle was loaded on.
///
/// Only runs when the caller asked for it: the two are different revisions, and
/// a value the parent carries is not one this revision will have unless it is
/// set here too. The fragment is deserialized once for the whole batch — that,
/// rather than any fan-out, is what batching buys here. A revision carrying no
/// metadata fragment answers nothing, which is not a failure.
async fn resolve_from_revision(
    internal: &Arc<RevisionTreeInternal>,
    state: &State,
    entries: &[LoreRevisionTreeMetadataGetEntry],
    resolved: &mut [Resolved],
) -> Result<(), MetadataGetError> {
    if !resolved.iter().any(|slot| matches!(slot, Resolved::Absent)) {
        return Ok(());
    }
    let metadata_hash = state.metadata_hash();
    if metadata_hash.is_zero() {
        return Ok(());
    }
    let frozen = Metadata::deserialize(internal.repository_context.clone(), metadata_hash)
        .await
        .map_err(|error| MetadataGetError::internal_with_context(error, "Metadata::deserialize"))?;

    for (entry, slot) in entries.iter().zip(resolved.iter_mut()) {
        if matches!(slot, Resolved::Absent) {
            *slot = resolve_key(&frozen, entry.key.as_str());
        }
    }
    Ok(())
}

/// Report a key whose value this build cannot turn back into a typed value,
/// whether the kind itself is unknown or the bytes do not match it.
fn emit_undecodable(entry_id: u64, key: &str) {
    emit_get_complete(entry_id, key, no_value(), LoreErrorCode::Internal);
}

/// Report each key in entry order: a value event for one that resolved, nothing
/// for one that did not.
///
/// A value that cannot be decoded reports `INTERNAL` on that entry rather than
/// staying silent, because silence is how this verb says "no such key".
fn emit_resolved(entries: &[LoreRevisionTreeMetadataGetEntry], resolved: &[Resolved]) {
    for (entry, slot) in entries.iter().zip(resolved.iter()) {
        let key = entry.key.as_str();
        match slot {
            Resolved::Absent => {}
            Resolved::Undecodable => emit_undecodable(entry.entry_id, key),
            Resolved::Value(value, value_type) => {
                match LoreMetadataEventData::new(key, value, *value_type) {
                    Ok(data) => {
                        emit_get_complete(entry.entry_id, key, data.value, LoreErrorCode::None);
                    }
                    Err(_) => emit_undecodable(entry.entry_id, key),
                }
            }
        }
    }
}

/// Read a batch of metadata values from the in-progress revision.
///
/// A key that resolves emits one `RevisionTreeMetadataGetComplete` carrying its
/// own `entry_id`, the key, and the value; a key present in neither the handle's
/// pending edits nor the loaded revision emits **nothing at all**, and the call
/// still succeeds. A caller detects an absent key by tracking whether a value
/// event arrived for it, matching `lore_revision_metadata_get`. An empty batch
/// succeeds.
///
/// By default this reads **only the revision being built** — what `metadata_set`
/// has recorded on this handle, which is exactly what `commit` will write.
/// Nothing is inherited from the revision the handle was loaded on, so a key the
/// parent carries does not resolve here unless it is set here too. That is the
/// point of the default: a caller asking "will my revision have this key" gets
/// an answer about their revision.
///
/// Set `include_revision` to `1` to also fall back to the loaded revision for a
/// key the handle has no entry for — for reading what the parent recorded. A
/// pending entry still wins, so the flag only ever adds answers, never changes
/// one. The two are different revisions and the flag is how a caller says which
/// question it is asking.
///
/// The call as a whole reports on `RevisionTreeBatchComplete`, carrying the
/// call's own `batch_id` and firing exactly once — after any per-entry
/// terminals and before `Complete`. A failure that belongs to the call rather
/// than to one entry is reported only there: an unknown or closed handle, and a
/// metadata fragment that cannot be read.
///
/// **This verb is not all-or-nothing**, unlike every other batch verb in the
/// namespace. It mutates nothing, so a key it cannot answer costs the other keys
/// nothing: each resolves independently, and an absent key is an ordinary
/// outcome rather than a failure. Bad *arguments* still reject the whole call —
/// an empty key, or a non-zero `entry_id` used by another entry, where `0` means
/// "not correlating this entry" and may repeat.
///
/// Values come back as the typed `LoreMetadata` that `metadata_set` takes, so a
/// value written through this API reads back unchanged without either side
/// encoding it as text. Every kind crosses the event, raw binary included.
///
/// A value that cannot be decoded reports `INTERNAL` on its own entry instead of
/// staying silent, so it is never mistaken for an absent key — that is a value
/// whose stored bytes do not match the tag stored beside them, or a tag this
/// build does not recognize, which a revision written by an older or broken
/// writer could carry.
///
/// With `include_revision` set, the loaded revision's metadata fragment is
/// deserialized once for the whole batch — that, rather than any fan-out, is
/// what batching buys here; the work per key is a buffer lookup. A fragment that
/// cannot be read fails the call. Keys are reported in entry order.
pub async fn metadata_get(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_get_impl).await
}

/// Resolve one batch. Split out of the dispatcher closure so the batch terminal
/// fires on every path the batch can take, including an early return.
async fn metadata_get_batch(
    internal: Arc<RevisionTreeInternal>,
    args: &LoreRevisionTreeMetadataGetArgs,
) -> Result<(), MetadataGetError> {
    let entries = args.entries.as_slice();
    if entries.is_empty() {
        return Ok(());
    }
    validate_entries(entries)?;

    let mut resolved = resolve_from_pending(&internal, entries);
    if args.include_revision != 0 {
        let access = internal.access_shared().await;
        resolve_from_revision(&internal, &access.state(), entries, &mut resolved).await?;
    }
    emit_resolved(entries, &resolved);
    Ok(())
}

async fn metadata_get_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        metadata_get,
        |args: &LoreRevisionTreeMetadataGetArgs| {
            emit_batch_complete(args.batch_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal: Arc<RevisionTreeInternal>, args: LoreRevisionTreeMetadataGetArgs| {
            let batch_id = args.batch_id;
            let result = metadata_get_batch(internal, &args).await;
            emit_batch_complete(batch_id, batch_error_code(&result));
            result
        },
    )
    .await
}
