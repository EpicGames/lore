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
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One metadata pair to record. `value` is a typed value that carries its own
/// kind, so there is no separate format tag and nothing to parse.
#[repr(C)]
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ValidateText)]
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
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Mutex;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::interface::LoreBinary;
    use lore_revision::interface::LoreMetadata;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
    use crate::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
    use crate::revision_tree::metadata_get::metadata_get;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    /// Call-level id every test batch is submitted under, distinct from the
    /// per-entry ids so the two cannot be confused in an assertion.
    const CALL_ID: u64 = 900;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Complete(i32, String),
        RevisionTreeLoaded(u64),
        SetComplete(u64, LoreErrorCode),
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
        Boolean(u8),
        Address(String),
        Hash(String),
        Context(String),
        Binary(Vec<u8>),
    }

    impl From<&LoreMetadata> for LoreMetadataValue {
        fn from(value: &LoreMetadata) -> Self {
            match value {
                LoreMetadata::String(text) => Self::Text(text.as_str().to_string()),
                LoreMetadata::Numeric(number) => Self::Number(*number),
                LoreMetadata::Boolean(flag) => Self::Boolean(*flag),
                LoreMetadata::Address(address) => Self::Address(address.to_string()),
                LoreMetadata::Hash(hash) => Self::Hash(hash.to_string()),
                LoreMetadata::Context(context) => Self::Context(context.to_string()),
                LoreMetadata::Binary(bytes) => Self::Binary(bytes.as_bytes().to_vec()),
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
                LoreEvent::RevisionTreeMetadataSetComplete(data) => {
                    Self::SetComplete(data.entry_id, data.error_code)
                }
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

    fn set_outcomes(events: &[CapturedEvent]) -> Vec<(u64, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::SetComplete(id, code) => Some((*id, *code)),
                _ => None,
            })
            .collect()
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
        typed_entry(
            entry_id,
            key,
            LoreMetadata::String(LoreString::from_str(value)),
        )
    }

    fn typed_entry(
        entry_id: u64,
        key: &str,
        value: LoreMetadata,
    ) -> LoreRevisionTreeMetadataSetEntry {
        LoreRevisionTreeMetadataSetEntry {
            entry_id,
            key: LoreString::from_str(key),
            value,
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

    async fn run_set(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataSetEntry>,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
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

    async fn run_get(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataGetEntry>,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: CALL_ID,
                handle,
                include_revision: 0,
                entries: LoreArray::from_vec(entries),
            },
            make_callback(sink.clone()),
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    /// The pair that matters: what a set records is what a get on the same
    /// handle reads back, carrying the format each entry declared.
    #[tokio::test]
    async fn a_set_batch_reads_back_through_get() {
        let partition = Partition::from([0x31u8; 16]);
        let (handle, store_handle_id) = load_handle("md-round-trip", partition).await;

        let (status, events) = run_set(
            handle,
            vec![
                set_entry(10, "author", "mattias"),
                typed_entry(11, "build", LoreMetadata::Numeric(4207)),
            ],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            set_outcomes(&events),
            vec![(10, LoreErrorCode::None), (11, LoreErrorCode::None)],
            "every entry reports, in index order"
        );

        let (status, events) = run_get(
            handle,
            vec![get_entry(20, "author"), get_entry(21, "build")],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            get_outcomes(&events),
            vec![
                (
                    20,
                    "author".to_string(),
                    LoreMetadataValue::Text("mattias".to_string()),
                    LoreErrorCode::None
                ),
                (
                    21,
                    "build".to_string(),
                    LoreMetadataValue::Number(4207),
                    LoreErrorCode::None
                ),
            ],
            "each key reports its own value under its own format"
        );
        release(handle, store_handle_id);
    }

    /// Every type the format enum offers survives a set and a get, carrying its
    /// own tag: a value written as an address reads back as an address, not as
    /// the text it was written from. Before these types existed on the surface a
    /// caller had to encode them as strings, which lost the tag and doubled the
    /// stored size for the hex forms. The binary case carries an embedded NUL and
    /// a byte that is not valid UTF-8, expressible only because the value is typed
    /// rather than text.
    #[tokio::test]
    async fn every_metadata_type_round_trips() {
        let partition = Partition::from([0x3fu8; 16]);
        let (handle, store_handle_id) = load_handle("md-all-types", partition).await;

        let hash_text = "ab".repeat(32);
        let context_text = "cd".repeat(16);
        let address_text = format!("{hash_text}-{context_text}");

        let cases: Vec<(&str, LoreMetadata, LoreMetadataValue)> = vec![
            (
                "text",
                LoreMetadata::String(LoreString::from_str("hello")),
                LoreMetadataValue::Text("hello".to_string()),
            ),
            (
                "count",
                LoreMetadata::Numeric(4207),
                LoreMetadataValue::Number(4207),
            ),
            (
                "flag",
                LoreMetadata::Boolean(1),
                LoreMetadataValue::Boolean(1),
            ),
            (
                "blob",
                LoreMetadata::Binary(LoreBinary::from_bytes(&[0x00, 0xff, 0x01])),
                LoreMetadataValue::Binary(vec![0x00, 0xff, 0x01]),
            ),
            (
                "hash",
                LoreMetadata::Hash(Hash::from_str(&hash_text).expect("hash")),
                LoreMetadataValue::Hash(hash_text.clone()),
            ),
            (
                "context",
                LoreMetadata::Context(Context::from_str(&context_text).expect("context")),
                LoreMetadataValue::Context(context_text.clone()),
            ),
            (
                "address",
                LoreMetadata::Address(Address::from_str(&address_text).expect("address")),
                LoreMetadataValue::Address(address_text.clone()),
            ),
        ];

        let entries: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(index, (key, value, _))| typed_entry(index as u64 + 1, key, value.clone()))
            .collect();
        let (status, events) = run_set(handle, entries).await;
        assert_eq!(status, 0, "every type must be settable, got {events:?}");
        assert_eq!(set_outcomes(&events).len(), cases.len());

        let reads: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(index, (key, _, _))| get_entry(index as u64 + 1, key))
            .collect();
        let (status, events) = run_get(handle, reads).await;
        assert_eq!(status, 0, "every type must be gettable, got {events:?}");

        let expected: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(index, (key, _, want))| {
                (
                    index as u64 + 1,
                    (*key).to_string(),
                    want.clone(),
                    LoreErrorCode::None,
                )
            })
            .collect();
        let mut got = get_outcomes(&events);
        got.sort_by_key(|(id, _, _, _)| *id);
        assert_eq!(
            got, expected,
            "each value must read back under its own type"
        );
        release(handle, store_handle_id);
    }

    /// A repeated key is the one duplicate the batch verbs permit: the last
    /// entry wins, because a batch is a compressed sequence of sets and separate
    /// calls already behave that way.
    #[tokio::test]
    async fn a_repeated_key_resolves_to_the_last_entry() {
        let partition = Partition::from([0x32u8; 16]);
        let (handle, store_handle_id) = load_handle("md-repeat-key", partition).await;

        let (status, events) = run_set(
            handle,
            vec![
                set_entry(10, "stage", "first"),
                set_entry(11, "stage", "second"),
                set_entry(12, "stage", "third"),
            ],
        )
        .await;
        assert_eq!(status, 0, "a repeated key must not reject, got {events:?}");
        assert_eq!(set_outcomes(&events).len(), 3, "every entry still reports");

        let (_, events) = run_get(handle, vec![get_entry(20, "stage")]).await;
        assert_eq!(
            get_outcomes(&events),
            vec![(
                20,
                "stage".to_string(),
                LoreMetadataValue::Text("third".to_string()),
                LoreErrorCode::None
            )],
            "the last entry naming the key wins"
        );
        release(handle, store_handle_id);
    }

    /// A later call overwrites an earlier one, the same as a later entry inside
    /// one call.
    #[tokio::test]
    async fn a_later_call_overwrites_an_earlier_value() {
        let partition = Partition::from([0x33u8; 16]);
        let (handle, store_handle_id) = load_handle("md-overwrite", partition).await;

        run_set(handle, vec![set_entry(10, "key", "before")]).await;
        run_set(handle, vec![set_entry(11, "key", "after")]).await;

        let (_, events) = run_get(handle, vec![get_entry(20, "key")]).await;
        assert_eq!(
            get_outcomes(&events),
            vec![(
                20,
                "key".to_string(),
                LoreMetadataValue::Text("after".to_string()),
                LoreErrorCode::None
            )]
        );
        release(handle, store_handle_id);
    }

    /// Validation runs over the whole batch before anything is recorded, so a
    /// bad entry anywhere leaves the pending metadata untouched.
    #[tokio::test]
    async fn a_rejected_set_batch_records_nothing() {
        let partition = Partition::from([0x34u8; 16]);
        let (handle, store_handle_id) = load_handle("md-atomic", partition).await;

        let (status, events) = run_set(
            handle,
            vec![set_entry(10, "good", "value"), set_entry(11, "", "no key")],
        )
        .await;
        assert_ne!(status, 0, "an entry with no key must reject");
        assert_eq!(
            set_outcomes(&events),
            vec![(11, LoreErrorCode::InvalidArguments)],
            "only the offending entry reports; the valid one was never applied"
        );
        let reason = rejection_reason(&events);
        assert!(
            reason.contains("entry 1: key must not be empty"),
            "the reason must name the entry index and the rule, got {reason:?}"
        );

        let (_, events) = run_get(handle, vec![get_entry(20, "good")]).await;
        assert!(
            get_outcomes(&events).is_empty(),
            "the entry ahead of the rejected one must not have been recorded"
        );

        let (status, _) = run_set(handle, vec![set_entry(12, "good", "value")]).await;
        assert_eq!(status, 0, "the handle must stay usable after a rejection");
        release(handle, store_handle_id);
    }

    /// An empty key names nothing, and a repeated non-zero id would make a
    /// reported id ambiguous; a repeated zero is an explicit opt-out.
    #[tokio::test]
    async fn set_rejects_an_empty_key_and_a_repeated_caller_id() {
        let partition = Partition::from([0x35u8; 16]);
        let (handle, store_handle_id) = load_handle("md-bad-args", partition).await;

        let (status, events) = run_set(handle, vec![set_entry(10, "", "value")]).await;
        assert_ne!(status, 0, "an empty key must reject");
        assert!(rejection_reason(&events).contains("key must not be empty"));

        let (status, events) = run_set(
            handle,
            vec![set_entry(10, "a", "1"), set_entry(10, "b", "2")],
        )
        .await;
        assert_ne!(status, 0, "a repeated non-zero caller id must reject");
        assert!(rejection_reason(&events).contains("two entries share one caller id"));

        let (status, events) =
            run_set(handle, vec![set_entry(0, "a", "1"), set_entry(0, "b", "2")]).await;
        assert_eq!(
            status, 0,
            "repeated zero ids must be accepted, got {events:?}"
        );
        assert_eq!(set_outcomes(&events).len(), 2);
        release(handle, store_handle_id);
    }

    /// An entry that alone exceeds what a revision's metadata may hold is
    /// refused during validation, not part-way through the writes: rejecting it
    /// at the write would leave the entries ahead of it recorded.
    #[tokio::test]
    async fn set_rejects_an_entry_larger_than_the_metadata_cap() {
        let partition = Partition::from([0x7du8; 16]);
        let (handle, store_handle_id) = load_handle("md-oversized", partition).await;

        let oversized = vec![0xabu8; METADATA_MAX_SIZE];
        let (status, events) = run_set(
            handle,
            vec![
                set_entry(10, "small", "value"),
                typed_entry(
                    11,
                    "blob",
                    LoreMetadata::Binary(LoreBinary::from_bytes(&oversized)),
                ),
            ],
        )
        .await;
        assert_ne!(status, 0, "an entry past the cap must reject");
        assert_eq!(
            set_outcomes(&events),
            vec![(11, LoreErrorCode::InvalidArguments)],
            "only the offending entry reports"
        );
        assert!(
            rejection_reason(&events).contains("does not fit"),
            "the reason must say the entry cannot fit, got {:?}",
            rejection_reason(&events)
        );

        let (_, events) = run_get(handle, vec![get_entry(20, "small")]).await;
        assert!(
            get_outcomes(&events).is_empty(),
            "the entry ahead of the rejected one must not have been recorded"
        );
        release(handle, store_handle_id);
    }

    /// An empty batch is a no-op that still reports the call, so a caller
    /// waiting on the batch terminal is not left hanging.
    #[tokio::test]
    async fn an_empty_set_batch_reports_the_batch_terminal() {
        let partition = Partition::from([0x36u8; 16]);
        let (handle, store_handle_id) = load_handle("md-empty", partition).await;

        let (status, events) = run_set(handle, Vec::new()).await;
        assert_eq!(status, 0, "got {events:?}");
        assert!(set_outcomes(&events).is_empty());
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)]
        );
        release(handle, store_handle_id);
    }

    /// An unknown handle is the call's failure, not any entry's, so it reports
    /// on the batch terminal alone.
    #[tokio::test]
    async fn set_on_unknown_handle_reports_only_the_batch_terminal() {
        let (status, events) = run_set(
            LoreRevisionTree::INVALID,
            vec![set_entry(10, "a", "1"), set_entry(11, "b", "2")],
        )
        .await;
        assert_ne!(status, 0);
        assert!(
            set_outcomes(&events).is_empty(),
            "a handle miss must fire no per-entry terminal"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::InvalidArguments)]
        );
    }

    /// A caller must be able to treat the batch terminal as the end of the call,
    /// which only holds if it fires after every entry and before `Complete`.
    #[tokio::test]
    async fn set_reports_entries_then_the_batch_terminal_then_complete() {
        let partition = Partition::from([0x37u8; 16]);
        let (handle, store_handle_id) = load_handle("md-ordering", partition).await;

        let (_, events) = run_set(
            handle,
            vec![set_entry(10, "a", "1"), set_entry(11, "b", "2")],
        )
        .await;
        let last_entry = events
            .iter()
            .rposition(|event| matches!(event, CapturedEvent::SetComplete(..)))
            .expect("both entries must report");
        let batch = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::BatchComplete(..)))
            .expect("the batch terminal must fire");
        let complete = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::Complete(..)))
            .expect("Complete must fire");
        assert!(
            last_entry < batch && batch < complete,
            "order must be entries, then the batch terminal, then Complete: {events:?}"
        );
        release(handle, store_handle_id);
    }
}
