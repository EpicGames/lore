// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetArgs;
use lore::revision_tree::metadata_get::LoreRevisionTreeMetadataGetEntry;
use lore::revision_tree::metadata_get::metadata_get;
use lore::revision_tree::metadata_set::*;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBinary;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::METADATA_MAX_SIZE;

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

fn set_outcomes(events: &[LoreEvent]) -> Vec<(u64, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeMetadataSetComplete(data) => {
                Some((data.entry_id, data.error_code))
            }
            _ => None,
        })
        .collect()
}

fn get_outcomes(events: &[LoreEvent]) -> Vec<(u64, String, LoreMetadataValue, LoreErrorCode)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeMetadataGetComplete(data) => Some((
                data.entry_id,
                data.key.as_str().to_string(),
                LoreMetadataValue::from(&data.value),
                data.error_code,
            )),
            _ => None,
        })
        .collect()
}

fn set_entry(entry_id: u64, key: &str, value: &str) -> LoreRevisionTreeMetadataSetEntry {
    typed_entry(
        entry_id,
        key,
        LoreMetadata::String(LoreString::from_str(value)),
    )
}

fn typed_entry(entry_id: u64, key: &str, value: LoreMetadata) -> LoreRevisionTreeMetadataSetEntry {
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

async fn run_set(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeMetadataSetEntry>,
) -> (i32, Recorded) {
    let (sink, callback) = recording_callback();
    let status = metadata_set(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMetadataSetArgs {
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

async fn run_get(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeMetadataGetEntry>,
) -> (i32, Recorded) {
    let (sink, callback) = recording_callback();
    let status = metadata_get(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMetadataGetArgs {
            batch_id: CALL_ID,
            handle,
            include_revision: 0,
            entries: LoreArray::from_vec(entries),
        },
        callback,
    )
    .await;
    let events = take_recorded(&sink);
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
    let reason = complete_message(&events);
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
    assert!(complete_message(&events).contains("key must not be empty"));

    let (status, events) = run_set(
        handle,
        vec![set_entry(10, "a", "1"), set_entry(10, "b", "2")],
    )
    .await;
    assert_ne!(status, 0, "a repeated non-zero caller id must reject");
    assert!(complete_message(&events).contains("two entries share one caller id"));

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
        complete_message(&events).contains("does not fit"),
        "the reason must say the entry cannot fit, got {:?}",
        complete_message(&events)
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
    assert_entries_then_batch_then_complete(&events, |event| {
        matches!(event, LoreEvent::RevisionTreeMetadataSetComplete(_))
    });
    release(handle, store_handle_id);
}
