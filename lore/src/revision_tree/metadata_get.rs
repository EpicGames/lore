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
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One metadata key to read.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreRevisionTreeMetadataGetEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `METADATA_GET_COMPLETE`
    pub entry_id: u64,
    /// Metadata key to read
    pub key: LoreString,
}

/// Arguments for `lore_revision_tree_metadata_get`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
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
        // Sound because the entry point checked every string the call carries
        // before dispatching it.
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
    entries: &[LoreRevisionTreeMetadataGetEntry],
    resolved: &mut [Resolved],
) -> Result<(), MetadataGetError> {
    if !resolved.iter().any(|slot| matches!(slot, Resolved::Absent)) {
        return Ok(());
    }
    let metadata_hash = internal.state.metadata_hash();
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
        resolve_from_revision(&internal, entries, &mut resolved).await?;
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::interface::LoreMetadata;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use crate::revision_tree::metadata_set::metadata_set;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    /// Call-level id every test batch is submitted under, distinct from the
    /// per-entry ids so the two cannot be confused in an assertion.
    const CALL_ID: u64 = 900;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Complete(i32, String),
        RevisionTreeLoaded(u64),
        GetComplete(u64, String, LoreMetadataValue, LoreErrorCode),
        BatchComplete(u64, LoreErrorCode),
        Other(u32),
    }

    /// The parts of a `LoreMetadata` a test compares; the type carries FFI
    /// pointers that do not compare meaningfully once copied out of the event.
    #[derive(Debug, Clone, PartialEq)]
    enum LoreMetadataValue {
        Text(String),
        Number(u64),
        Other,
    }

    impl From<&LoreMetadata> for LoreMetadataValue {
        fn from(value: &LoreMetadata) -> Self {
            match value {
                LoreMetadata::String(text) => Self::Text(text.as_str().to_string()),
                LoreMetadata::Numeric(number) => Self::Number(*number),
                _ => Self::Other,
            }
        }
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Complete(data) => {
                    Self::Complete(data.status, data.error.message.as_str().to_string())
                }
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeMetadataGetComplete(data) => Self::GetComplete(
                    data.entry_id,
                    data.key.as_str().to_string(),
                    LoreMetadataValue::from(&data.value),
                    data.error_code,
                ),
                LoreEvent::RevisionTreeBatchComplete(data) => {
                    Self::BatchComplete(data.batch_id, data.error_code)
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

    fn get_outcomes(
        events: &[CapturedEvent],
    ) -> Vec<(u64, String, LoreMetadataValue, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::GetComplete(id, key, value, code) => {
                    Some((*id, key.clone(), value.clone(), *code))
                }
                _ => None,
            })
            .collect()
    }

    fn batch_outcomes(events: &[CapturedEvent]) -> Vec<(u64, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::BatchComplete(id, code) => Some((*id, *code)),
                _ => None,
            })
            .collect()
    }

    fn rejection_reason(events: &[CapturedEvent]) -> String {
        events
            .iter()
            .find_map(|event| match event {
                CapturedEvent::Complete(_, message) => Some(message.clone()),
                _ => None,
            })
            .expect("the call must complete")
    }

    fn set_entry(entry_id: u64, key: &str, value: &str) -> LoreRevisionTreeMetadataSetEntry {
        LoreRevisionTreeMetadataSetEntry {
            entry_id,
            key: LoreString::from_str(key),
            value: LoreMetadata::String(LoreString::from_str(value)),
        }
    }

    fn get_entry(entry_id: u64, key: &str) -> LoreRevisionTreeMetadataGetEntry {
        LoreRevisionTreeMetadataGetEntry {
            entry_id,
            key: LoreString::from_str(key),
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

    async fn seed(handle: LoreRevisionTree, entries: Vec<LoreRevisionTreeMetadataSetEntry>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                batch_id: 1,
                handle,
                entries: LoreArray::from_vec(entries),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "seeding metadata must succeed");
    }

    async fn run_get(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataGetEntry>,
    ) -> (i32, Vec<CapturedEvent>) {
        run_get_with(handle, entries, 0).await
    }

    /// `include_revision = 1` also falls back to the revision the handle loaded.
    async fn run_get_including_revision(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataGetEntry>,
    ) -> (i32, Vec<CapturedEvent>) {
        run_get_with(handle, entries, 1).await
    }

    async fn run_get_with(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataGetEntry>,
        include_revision: u8,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: CALL_ID,
                handle,
                include_revision,
                entries: LoreArray::from_vec(entries),
            },
            make_callback(sink.clone()),
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    /// Freeze `pairs` into a metadata fragment and point the handle's state at
    /// it, standing in for the revision a `load` of a committed hash would have
    /// brought. Nothing else reaches the frozen path until `commit` exists.
    async fn freeze_metadata(handle: LoreRevisionTree, pairs: &[(&str, &str)]) {
        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        let mut metadata = Metadata::new();
        for (key, value) in pairs {
            metadata
                .set_typed(key, value.as_bytes(), MetadataType::String)
                .expect("seeding the fragment must succeed");
        }
        let hash = metadata
            .serialize(internal.repository_context.clone())
            .await
            .expect("serializing the fragment must succeed");
        internal.state.set_metadata_hash(hash);
    }

    /// The loaded revision is read only when it is both asked for and needed.
    /// A fragment that cannot be read proves it: the call succeeds whenever the
    /// verb never reaches for it, and fails only when it does.
    #[tokio::test]
    async fn the_revision_is_read_only_when_asked_for_and_needed() {
        let partition = Partition::from([0x40u8; 16]);
        let (handle, store_handle_id) = load_handle("md-not-read", partition).await;
        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        internal
            .state
            .set_metadata_hash(Hash::from_u64(0xdead_beef));
        seed(handle, vec![set_entry(1, "mine", "value")]).await;

        let (status, events) = run_get(handle, vec![get_entry(20, "mine")]).await;
        assert_eq!(
            status, 0,
            "not asked for: the fragment is never touched, got {events:?}"
        );
        assert_eq!(get_outcomes(&events).len(), 1, "the handle's key resolves");

        let (status, events) =
            run_get_including_revision(handle, vec![get_entry(21, "mine")]).await;
        assert_eq!(
            status, 0,
            "asked for but not needed: every key already resolved, so the fragment \
             is still not read, got {events:?}"
        );
        assert_eq!(get_outcomes(&events).len(), 1);

        let (status, _) = run_get_including_revision(handle, vec![get_entry(22, "absent")]).await;
        assert_ne!(
            status, 0,
            "asked for and needed: the unreadable fragment now fails the call"
        );
        release(handle, store_handle_id);
    }

    /// A key in neither source is absent whether or not the revision is read;
    /// asking for the revision adds answers, it never invents them.
    #[tokio::test]
    async fn a_key_in_neither_source_reports_nothing_either_way() {
        let partition = Partition::from([0x4au8; 16]);
        let (handle, store_handle_id) = load_handle("md-neither", partition).await;
        freeze_metadata(handle, &[("elsewhere", "value")]).await;

        for (label, events) in [
            (
                "default",
                run_get(handle, vec![get_entry(20, "nowhere")]).await,
            ),
            (
                "including the revision",
                run_get_including_revision(handle, vec![get_entry(21, "nowhere")]).await,
            ),
        ] {
            assert_eq!(events.0, 0, "{label}: an absent key is not a failure");
            assert!(
                get_outcomes(&events.1).is_empty(),
                "{label}: no event may fire for a key in neither source"
            );
        }
        release(handle, store_handle_id);
    }

    /// A revision that froze no metadata at all answers nothing, which is an
    /// ordinary outcome rather than the unreadable-fragment failure.
    #[tokio::test]
    async fn a_revision_with_no_metadata_answers_nothing() {
        let partition = Partition::from([0x4bu8; 16]);
        let (handle, store_handle_id) = load_handle("md-no-fragment", partition).await;

        let (status, events) =
            run_get_including_revision(handle, vec![get_entry(20, "anything")]).await;
        assert_eq!(status, 0, "a revision with no metadata is not a failure");
        assert!(get_outcomes(&events).is_empty());
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)]
        );
        release(handle, store_handle_id);
    }

    /// The default answers about the revision being built, not the one loaded.
    /// A key the parent carries is absent here, because a commit will not
    /// inherit it — reporting it would promise a value the new revision has not
    /// got.
    #[tokio::test]
    async fn the_loaded_revision_is_not_read_unless_asked_for() {
        let partition = Partition::from([0x3fu8; 16]);
        let (handle, store_handle_id) = load_handle("md-no-inherit", partition).await;
        freeze_metadata(handle, &[("parent-key", "parent-value")]).await;

        let (status, events) = run_get(handle, vec![get_entry(20, "parent-key")]).await;
        assert_eq!(status, 0, "an absent key is not a failure, got {events:?}");
        assert!(
            get_outcomes(&events).is_empty(),
            "the parent's key must not resolve by default, got {events:?}"
        );

        let (status, events) =
            run_get_including_revision(handle, vec![get_entry(21, "parent-key")]).await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            get_outcomes(&events).len(),
            1,
            "the same key resolves once the caller asks for the revision"
        );
        release(handle, store_handle_id);
    }

    /// The half of the lookup that only a loaded revision exercises: a key the
    /// handle never set still resolves, out of the revision's frozen fragment.
    #[tokio::test]
    async fn get_reads_a_value_frozen_in_the_loaded_revision() {
        let partition = Partition::from([0x3cu8; 16]);
        let (handle, store_handle_id) = load_handle("md-frozen", partition).await;
        freeze_metadata(handle, &[("frozen-key", "frozen-value")]).await;

        let (status, events) =
            run_get_including_revision(handle, vec![get_entry(20, "frozen-key")]).await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            get_outcomes(&events),
            vec![(
                20,
                "frozen-key".to_string(),
                LoreMetadataValue::Text("frozen-value".to_string()),
                LoreErrorCode::None
            )],
            "with the flag set, a key present only in the revision resolves"
        );
        release(handle, store_handle_id);
    }

    /// Pending edits take precedence: a key set on the handle reads back as the
    /// value just set, not the one the revision froze under the same key.
    #[tokio::test]
    async fn a_pending_edit_shadows_the_frozen_value() {
        let partition = Partition::from([0x3du8; 16]);
        let (handle, store_handle_id) = load_handle("md-shadow", partition).await;
        freeze_metadata(handle, &[("key", "from-revision"), ("only-frozen", "kept")]).await;
        seed(handle, vec![set_entry(1, "key", "from-handle")]).await;

        let (status, events) = run_get_including_revision(
            handle,
            vec![get_entry(20, "key"), get_entry(21, "only-frozen")],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            get_outcomes(&events),
            vec![
                (
                    20,
                    "key".to_string(),
                    LoreMetadataValue::Text("from-handle".to_string()),
                    LoreErrorCode::None
                ),
                (
                    21,
                    "only-frozen".to_string(),
                    LoreMetadataValue::Text("kept".to_string()),
                    LoreErrorCode::None
                ),
            ],
            "the pending edit wins its key while the frozen-only key still resolves"
        );
        release(handle, store_handle_id);
    }

    /// A fragment the store cannot produce is the call's failure, not any
    /// entry's: it reports on the batch terminal and no key reports at all.
    #[tokio::test]
    async fn an_unreadable_metadata_fragment_fails_the_call() {
        let partition = Partition::from([0x3eu8; 16]);
        let (handle, store_handle_id) = load_handle("md-unreadable", partition).await;
        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        internal
            .state
            .set_metadata_hash(Hash::from_u64(0xdead_beef));

        let (status, events) = run_get_including_revision(handle, vec![get_entry(20, "any")]).await;
        assert_ne!(status, 0, "an unreadable fragment must fail the call");
        assert!(
            get_outcomes(&events).is_empty(),
            "no key may report when the fragment could not be read, got {events:?}"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::Internal)],
            "the failure belongs to the call, not to an entry"
        );
        release(handle, store_handle_id);
    }

    /// The read exception: an absent key emits nothing and does not fail the
    /// call, so a batch of mixed keys reports only the ones that resolved.
    #[tokio::test]
    async fn get_reports_only_the_keys_that_resolve() {
        let partition = Partition::from([0x38u8; 16]);
        let (handle, store_handle_id) = load_handle("md-mixed", partition).await;

        seed(handle, vec![set_entry(10, "present", "yes")]).await;

        let (status, events) = run_get(
            handle,
            vec![
                get_entry(20, "absent"),
                get_entry(21, "present"),
                get_entry(22, "also-absent"),
            ],
        )
        .await;
        assert_eq!(
            status, 0,
            "absent keys are an ordinary outcome, not a failure, got {events:?}"
        );
        assert_eq!(
            get_outcomes(&events),
            vec![(
                21,
                "present".to_string(),
                LoreMetadataValue::Text("yes".to_string()),
                LoreErrorCode::None
            )],
            "only the key that resolved reports"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)]
        );
        release(handle, store_handle_id);
    }

    /// A value whose stored bytes do not match its type tag cannot be decoded.
    /// It reports internal rather than staying silent, so it is never mistaken
    /// for an absent key — silence is how this verb says the key is not there.
    #[tokio::test]
    async fn get_reports_a_value_it_cannot_decode_rather_than_staying_silent() {
        let partition = Partition::from([0x39u8; 16]);
        let (handle, store_handle_id) = load_handle("md-undecodable", partition).await;

        // A boolean is one byte; three bytes under that tag is not decodable.
        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        internal
            .pending_metadata
            .write()
            .set_typed("broken", b"xyz", MetadataType::Boolean)
            .expect("writing the malformed value must succeed");

        let (status, events) = run_get(handle, vec![get_entry(20, "broken")]).await;
        assert_eq!(status, 0, "one undecodable value does not fail the call");
        assert_eq!(
            get_outcomes(&events)
                .iter()
                .map(|(id, key, _, code)| (*id, key.clone(), *code))
                .collect::<Vec<_>>(),
            vec![(20, "broken".to_string(), LoreErrorCode::Internal)],
            "the entry must report internal, distinguishable from an absent key"
        );
        release(handle, store_handle_id);
    }

    /// A string value holding bytes that are not text reaches the caller as the
    /// undecodable outcome, not as an empty string. The entry point checks the
    /// text a call carries, so this can only arrive from a value already stored
    /// — and an empty string is a value a key may legitimately hold.
    #[tokio::test]
    async fn get_reports_a_string_value_that_is_not_text() {
        let partition = Partition::from([0x4cu8; 16]);
        let (handle, store_handle_id) = load_handle("md-string-not-text", partition).await;

        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        internal
            .pending_metadata
            .write()
            .set_typed("label", b"\xff\xfe", MetadataType::String)
            .expect("writing the malformed value must succeed");

        let (status, events) = run_get(handle, vec![get_entry(20, "label")]).await;
        assert_eq!(status, 0, "one undecodable value does not fail the call");
        assert_eq!(
            get_outcomes(&events)
                .iter()
                .map(|(id, key, _, code)| (*id, key.clone(), *code))
                .collect::<Vec<_>>(),
            vec![(20, "label".to_string(), LoreErrorCode::Internal)],
            "the entry must report internal rather than an empty string"
        );
        release(handle, store_handle_id);
    }

    /// Bad arguments still reject the whole read, even though an absent key does
    /// not — the exemption is from atomicity, not from argument checking.
    #[tokio::test]
    async fn get_rejects_bad_arguments_despite_tolerating_absent_keys() {
        let partition = Partition::from([0x3au8; 16]);
        let (handle, store_handle_id) = load_handle("md-get-bad-args", partition).await;

        let (status, events) = run_get(handle, vec![get_entry(20, "")]).await;
        assert_ne!(status, 0, "an empty key must reject");
        assert!(rejection_reason(&events).contains("key must not be empty"));

        let (status, events) = run_get(handle, vec![get_entry(20, "a"), get_entry(20, "b")]).await;
        assert_ne!(status, 0, "a repeated non-zero caller id must reject");
        assert!(rejection_reason(&events).contains("two entries share one caller id"));
        // The rejection names the key it was about, and carries no value: this
        // event has no absent variant, and a numeric zero would read as a key
        // that really holds zero.
        assert_eq!(
            get_outcomes(&events),
            vec![(
                20,
                "b".to_string(),
                LoreMetadataValue::Text(String::new()),
                LoreErrorCode::InvalidArguments
            )],
            "the rejection terminal must identify the offending key"
        );
        release(handle, store_handle_id);
    }

    /// An empty read batch still reports the call.
    #[tokio::test]
    async fn an_empty_get_batch_reports_the_batch_terminal() {
        let partition = Partition::from([0x3bu8; 16]);
        let (handle, store_handle_id) = load_handle("md-get-empty", partition).await;

        let (status, events) = run_get(handle, Vec::new()).await;
        assert_eq!(status, 0, "got {events:?}");
        assert!(get_outcomes(&events).is_empty());
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)]
        );
        release(handle, store_handle_id);
    }

    /// An unknown handle is the call's failure, not any entry's, so it reports
    /// on the batch terminal alone.
    #[tokio::test]
    async fn get_on_unknown_handle_reports_only_the_batch_terminal() {
        let (status, events) = run_get(LoreRevisionTree::INVALID, vec![get_entry(20, "a")]).await;
        assert_ne!(status, 0);
        assert!(get_outcomes(&events).is_empty());
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)]
        );
    }
}
