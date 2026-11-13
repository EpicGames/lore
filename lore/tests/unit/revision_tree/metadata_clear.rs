// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::metadata_clear::*;
use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
use lore::revision_tree::metadata_get::metadata_get;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
use lore::revision_tree::metadata_set::metadata_set;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;

use crate::call::test_support::Recorded;
use crate::call::test_support::complete_message;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;
use crate::revision_tree::support::assert_entries_then_batch_then_complete;
use crate::revision_tree::support::batch_outcomes;
use crate::revision_tree::support::load_handle;
use crate::revision_tree::support::release;

/// Call-level id every test batch is submitted under, distinct from the
/// per-entry ids so the two cannot be confused in an assertion.
const CALL_ID: u64 = 900;

fn clear_outcomes(events: &[LoreEvent]) -> Vec<(u64, u8, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeMetadataClearComplete(data) => {
                Some((data.entry_id, data.removed, data.error_code))
            }
            _ => None,
        })
        .collect()
}

fn present_keys(events: &[LoreEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeMetadataGetComplete(data) => Some(data.key.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn clear_entry(entry_id: u64, key: &str) -> LoreRevisionTreeMetadataClearEntry {
    LoreRevisionTreeMetadataClearEntry {
        entry_id,
        key: LoreString::from_str(key),
    }
}

async fn seed(handle: LoreRevisionTree, keys: &[&str]) {
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
        None,
    )
    .await;
    assert_eq!(status, 0, "seeding metadata must succeed");
}

async fn run_clear(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeMetadataClearEntry>,
) -> (i32, Recorded) {
    let (sink, callback) = recording_callback();
    let status = metadata_clear(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMetadataClearArgs {
            batch_id: CALL_ID,
            handle,
            entries: LoreArray::from_vec(entries),
        },
        callback,
    )
    .await;
    let events = take_recorded(&sink);
    (status, events)
}

async fn keys_present(handle: LoreRevisionTree, keys: &[&str]) -> Vec<String> {
    let (sink, callback) = recording_callback();
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
        callback,
    )
    .await;
    let events = take_recorded(&sink);
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
    internal.state_for_tests().set_metadata_hash(hash);

    let (status, events) = run_clear(handle, vec![clear_entry(10, "parent-key")]).await;
    assert_eq!(status, 0, "clearing an unset key is a no-op success");
    assert_eq!(
        clear_outcomes(&events),
        vec![(10, 0, LoreErrorCode::None)],
        "the key was never set on this handle, so nothing was removed"
    );

    let (sink, callback) = recording_callback();
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
        callback,
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

    let (status, events) = run_clear(handle, vec![clear_entry(10, "a"), clear_entry(11, "")]).await;
    assert_ne!(status, 0, "an empty key must reject");
    assert_eq!(
        clear_outcomes(&events),
        vec![(11, 0, LoreErrorCode::InvalidArguments)],
        "only the offending entry reports; the valid one was never applied"
    );
    assert!(complete_message(&events).contains("entry 1: key must not be empty"));
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
    assert!(complete_message(&events).contains("two entries share one caller id"));

    let (status, events) = run_clear(handle, vec![clear_entry(0, "a"), clear_entry(0, "b")]).await;
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
    assert_entries_then_batch_then_complete(&events, |event| {
        matches!(event, LoreEvent::RevisionTreeMetadataClearComplete(_))
    });
    release(handle, store_handle_id);
}
