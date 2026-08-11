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
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;

/// One metadata key to remove.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreRevisionTreeMetadataClearEntry {
    /// Caller-chosen id echoed back as `entry_id` on this entry's `METADATA_CLEAR_COMPLETE`
    pub entry_id: u64,
    /// Metadata key to remove; a key that is not set is a no-op
    pub key: LoreString,
}

/// Arguments for `lore_revision_tree_metadata_clear`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
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
        // Sound because the entry point checked every string the call carries
        // before dispatching it.
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
            let result = metadata_clear_batch(&internal, &args);
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
    use lore_revision::metadata::Metadata;
    use lore_revision::metadata::MetadataType;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
    use crate::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
    use crate::revision_tree::metadata_get::metadata_get;
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
        ClearComplete(u64, u8, LoreErrorCode),
        GetComplete(u64, String),
        BatchComplete(u64, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Complete(data) => {
                    Self::Complete(data.status, data.error.message.as_str().to_string())
                }
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeMetadataClearComplete(data) => {
                    Self::ClearComplete(data.entry_id, data.removed, data.error_code)
                }
                LoreEvent::RevisionTreeMetadataGetComplete(data) => {
                    Self::GetComplete(data.entry_id, data.key.as_str().to_string())
                }
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

    fn clear_outcomes(events: &[CapturedEvent]) -> Vec<(u64, u8, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::ClearComplete(id, removed, code) => Some((*id, *removed, *code)),
                _ => None,
            })
            .collect()
    }

    fn present_keys(events: &[CapturedEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::GetComplete(_, key) => Some(key.clone()),
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

    fn clear_entry(entry_id: u64, key: &str) -> LoreRevisionTreeMetadataClearEntry {
        LoreRevisionTreeMetadataClearEntry {
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

    async fn seed(handle: LoreRevisionTree, keys: &[&str]) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let entries: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| LoreRevisionTreeMetadataSetEntry {
                entry_id: index as u64 + 1,
                key: LoreString::from_str(key),
                value: LoreMetadata::String(LoreString::from_str("value")),
            })
            .collect();
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

    async fn run_clear(
        handle: LoreRevisionTree,
        entries: Vec<LoreRevisionTreeMetadataClearEntry>,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_clear(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataClearArgs {
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

    async fn keys_present(handle: LoreRevisionTree, keys: &[&str]) -> Vec<String> {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let entries: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| LoreRevisionTreeMetadataGetEntry {
                entry_id: index as u64 + 1,
                key: LoreString::from_str(key),
            })
            .collect();
        metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: 2,
                handle,
                include_revision: 0,
                entries: LoreArray::from_vec(entries),
            },
            make_callback(sink.clone()),
        )
        .await;
        let events = sink.lock().unwrap().clone();
        present_keys(&events)
    }

    /// Clearing edits the revision being built and cannot reach the one the
    /// handle was loaded on. A key that lives only in that revision reports
    /// `removed = 0` — it was never set here — and still reads back when the
    /// caller asks for the revision.
    #[tokio::test]
    async fn clearing_cannot_touch_the_loaded_revision() {
        let partition = Partition::from([0x49u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-parent", partition).await;

        let internal = rt_handle::lookup(handle).expect("the handle must resolve");
        let mut metadata = Metadata::new();
        metadata
            .set_typed("parent-key", b"parent-value", MetadataType::String)
            .expect("seeding the fragment must succeed");
        let hash = metadata
            .serialize(internal.repository_context.clone())
            .await
            .expect("serializing the fragment must succeed");
        internal.state.set_metadata_hash(hash);

        let (status, events) = run_clear(handle, vec![clear_entry(10, "parent-key")]).await;
        assert_eq!(status, 0, "clearing an unset key is a no-op success");
        assert_eq!(
            clear_outcomes(&events),
            vec![(10, 0, LoreErrorCode::None)],
            "the key was never set on this handle, so nothing was removed"
        );

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                batch_id: 3,
                handle,
                include_revision: 1,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataGetEntry {
                    entry_id: 30,
                    key: LoreString::from_str("parent-key"),
                }]),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(
            present_keys(&sink.lock().unwrap()),
            vec!["parent-key".to_string()],
            "the loaded revision still carries the key; clearing did not and cannot remove it"
        );
        release(handle, store_handle_id);
    }

    /// A cleared key stops reading back, and the entry says it was there.
    #[tokio::test]
    async fn clearing_a_set_key_removes_it() {
        let partition = Partition::from([0x41u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear", partition).await;
        seed(handle, &["keep", "drop"]).await;

        let (status, events) = run_clear(handle, vec![clear_entry(10, "drop")]).await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            clear_outcomes(&events),
            vec![(10, 1, LoreErrorCode::None)],
            "the entry must report that the key was removed"
        );
        assert_eq!(
            keys_present(handle, &["keep", "drop"]).await,
            vec!["keep".to_string()],
            "only the cleared key stops reading back"
        );
        release(handle, store_handle_id);
    }

    /// Clearing a key that was never set is a no-op success, distinguished from
    /// a real removal only by `removed`.
    #[tokio::test]
    async fn clearing_an_absent_key_is_a_no_op_success() {
        let partition = Partition::from([0x42u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-absent", partition).await;

        let (status, events) = run_clear(handle, vec![clear_entry(10, "never-set")]).await;
        assert_eq!(status, 0, "an absent key must not fail the call");
        assert_eq!(
            clear_outcomes(&events),
            vec![(10, 0, LoreErrorCode::None)],
            "the entry must succeed and report that nothing was removed"
        );
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)]
        );
        release(handle, store_handle_id);
    }

    /// A repeated key is not a rejection: the second entry finds it already gone
    /// and reports the no-op, matching what separate calls would do.
    #[tokio::test]
    async fn a_repeated_key_clears_once_and_then_no_ops() {
        let partition = Partition::from([0x43u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-repeat", partition).await;
        seed(handle, &["target"]).await;

        let (status, events) = run_clear(
            handle,
            vec![clear_entry(10, "target"), clear_entry(11, "target")],
        )
        .await;
        assert_eq!(status, 0, "a repeated key must not reject, got {events:?}");
        assert_eq!(
            clear_outcomes(&events),
            vec![(10, 1, LoreErrorCode::None), (11, 0, LoreErrorCode::None)],
            "the first removes it and the second is a no-op"
        );
        release(handle, store_handle_id);
    }

    /// Many keys in one call, mixing present and absent.
    #[tokio::test]
    async fn a_batch_clears_every_key_it_names() {
        let partition = Partition::from([0x44u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-batch", partition).await;
        seed(handle, &["a", "b", "c"]).await;

        let (status, events) = run_clear(
            handle,
            vec![
                clear_entry(10, "a"),
                clear_entry(11, "absent"),
                clear_entry(12, "c"),
            ],
        )
        .await;
        assert_eq!(status, 0, "got {events:?}");
        assert_eq!(
            clear_outcomes(&events),
            vec![
                (10, 1, LoreErrorCode::None),
                (11, 0, LoreErrorCode::None),
                (12, 1, LoreErrorCode::None),
            ],
            "each entry reports its own outcome, in index order"
        );
        assert_eq!(
            keys_present(handle, &["a", "b", "c"]).await,
            vec!["b".to_string()],
            "only the untouched key survives"
        );
        release(handle, store_handle_id);
    }

    /// Validation runs over the whole batch before anything is removed, so a bad
    /// entry anywhere leaves every key in place.
    #[tokio::test]
    async fn a_rejected_clear_batch_removes_nothing() {
        let partition = Partition::from([0x45u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-atomic", partition).await;
        seed(handle, &["a", "b"]).await;

        let (status, events) =
            run_clear(handle, vec![clear_entry(10, "a"), clear_entry(11, "")]).await;
        assert_ne!(status, 0, "an empty key must reject");
        assert_eq!(
            clear_outcomes(&events),
            vec![(11, 0, LoreErrorCode::InvalidArguments)],
            "only the offending entry reports; the valid one was never applied"
        );
        assert!(rejection_reason(&events).contains("entry 1: key must not be empty"));
        assert_eq!(
            keys_present(handle, &["a", "b"]).await,
            vec!["a".to_string(), "b".to_string()],
            "the entry ahead of the rejected one must not have been applied"
        );
        release(handle, store_handle_id);
    }

    /// A repeated non-zero id would make a reported id ambiguous; a repeated zero
    /// is an explicit opt-out.
    #[tokio::test]
    async fn clear_rejects_a_repeated_caller_id_but_accepts_repeated_zeros() {
        let partition = Partition::from([0x46u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-ids", partition).await;
        seed(handle, &["a", "b"]).await;

        let (status, events) =
            run_clear(handle, vec![clear_entry(10, "a"), clear_entry(10, "b")]).await;
        assert_ne!(status, 0, "a repeated non-zero caller id must reject");
        assert!(rejection_reason(&events).contains("two entries share one caller id"));

        let (status, events) =
            run_clear(handle, vec![clear_entry(0, "a"), clear_entry(0, "b")]).await;
        assert_eq!(
            status, 0,
            "repeated zero ids must be accepted, got {events:?}"
        );
        assert_eq!(clear_outcomes(&events).len(), 2);
        release(handle, store_handle_id);
    }

    /// An empty batch is a no-op that still reports the call.
    #[tokio::test]
    async fn an_empty_clear_batch_reports_the_batch_terminal() {
        let partition = Partition::from([0x47u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-empty", partition).await;

        let (status, events) = run_clear(handle, Vec::new()).await;
        assert_eq!(status, 0, "got {events:?}");
        assert!(clear_outcomes(&events).is_empty());
        assert_eq!(
            batch_outcomes(&events),
            vec![(CALL_ID, LoreErrorCode::None)]
        );
        release(handle, store_handle_id);
    }

    /// An unknown handle is the call's failure, not any entry's, so it reports on
    /// the batch terminal alone.
    #[tokio::test]
    async fn clear_on_unknown_handle_reports_only_the_batch_terminal() {
        let (status, events) = run_clear(
            LoreRevisionTree::INVALID,
            vec![clear_entry(10, "a"), clear_entry(11, "b")],
        )
        .await;
        assert_ne!(status, 0);
        assert!(
            clear_outcomes(&events).is_empty(),
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
    async fn clear_reports_entries_then_the_batch_terminal_then_complete() {
        let partition = Partition::from([0x48u8; 16]);
        let (handle, store_handle_id) = load_handle("md-clear-order", partition).await;
        seed(handle, &["a", "b"]).await;

        let (_, events) = run_clear(handle, vec![clear_entry(10, "a"), clear_entry(11, "b")]).await;
        let last_entry = events
            .iter()
            .rposition(|event| matches!(event, CapturedEvent::ClearComplete(..)))
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
