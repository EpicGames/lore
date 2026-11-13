// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle as rt_handle;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::metadata_get::*;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
use lore::revision_tree::metadata_set::metadata_set;
use lore_base::types::Hash;
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

async fn seed(handle: LoreRevisionTree, entries: Vec<LoreRevisionTreeMetadataSetEntry>) {
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

async fn run_get(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeMetadataGetEntry>,
) -> (i32, Recorded) {
    run_get_with(handle, entries, 0).await
}

/// `include_revision = 1` also falls back to the revision the handle loaded.
async fn run_get_including_revision(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeMetadataGetEntry>,
) -> (i32, Recorded) {
    run_get_with(handle, entries, 1).await
}

async fn run_get_with(
    handle: LoreRevisionTree,
    entries: Vec<LoreRevisionTreeMetadataGetEntry>,
    include_revision: u8,
) -> (i32, Recorded) {
    let (sink, callback) = recording_callback();
    let status = metadata_get(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMetadataGetArgs {
            batch_id: CALL_ID,
            handle,
            include_revision,
            entries: LoreArray::from_vec(entries),
        },
        callback,
    )
    .await;
    let events = take_recorded(&sink);
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
    internal.state_for_tests().set_metadata_hash(hash);
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
        .state_for_tests()
        .set_metadata_hash(Hash::from_u64(0xdead_beef));
    seed(handle, vec![set_entry(1, "mine", "value")]).await;

    let (status, events) = run_get(handle, vec![get_entry(20, "mine")]).await;
    assert_eq!(
        status, 0,
        "not asked for: the fragment is never touched, got {events:?}"
    );
    assert_eq!(get_outcomes(&events).len(), 1, "the handle's key resolves");

    let (status, events) = run_get_including_revision(handle, vec![get_entry(21, "mine")]).await;
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
        .state_for_tests()
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
/// not — the exemption is from atomicity, not from argument checking. The
/// rejection carries empty text for "no value": the event has no absent
/// variant, and a numeric zero would read as a key that really holds zero.
#[tokio::test]
async fn get_rejects_bad_arguments_despite_tolerating_absent_keys() {
    let partition = Partition::from([0x3au8; 16]);
    let (handle, store_handle_id) = load_handle("md-get-bad-args", partition).await;

    let (status, events) = run_get(handle, vec![get_entry(20, "")]).await;
    assert_ne!(status, 0, "an empty key must reject");
    assert!(complete_message(&events).contains("key must not be empty"));

    let (status, events) = run_get(handle, vec![get_entry(20, "a"), get_entry(20, "b")]).await;
    assert_ne!(status, 0, "a repeated non-zero caller id must reject");
    assert!(complete_message(&events).contains("two entries share one caller id"));
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
