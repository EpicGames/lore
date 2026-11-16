// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore::interface::LoreGlobalArgs;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::branch_metadata_set::Applied;
use lore::revision_graph::branch_metadata_set::LoreRevisionGraphBranchMetadataEntry;
use lore::revision_graph::branch_metadata_set::LoreRevisionGraphBranchMetadataSetArgs;
use lore::revision_graph::branch_metadata_set::StoredEntry;
use lore::revision_graph::branch_metadata_set::apply;
use lore::revision_graph::branch_metadata_set::branch_metadata_set;
use lore::revision_graph::branch_metadata_set::validate;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::AddressNotFound;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchNotFound;
use lore_base::error::Disconnected;
use lore_base::error::InvalidArguments;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::error::Oversized;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::Internal;
use lore_error_set::prelude::*;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBinary;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::METADATA_MAX_SIZE;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::ImmutableCall;
use lore_storage::test_util::StoreCall;
use lore_storage::test_util::answered;
use lore_transport::ProtocolError;
use lore_transport::test_util::MemoryPeer;

use crate::call::test_support::recording_callback;
use crate::call::test_support::without_log_lines;
use crate::revision_graph::support::MetadataAnswer;
use crate::revision_graph::support::NamingRevision;
use crate::revision_graph::support::SetAnswer;
use crate::revision_graph::support::archive;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::create_in_store;
use crate::revision_graph::support::entries_at;
use crate::revision_graph::support::handle_failing;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::local_metadata;
use crate::revision_graph::support::location_outcomes;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::revision_graph::support::partial_metadata;
use crate::revision_graph::support::put_metadata_blob;
use crate::revision_graph::support::reported;
use crate::revision_graph::support::store_metadata;
use crate::revision_graph::support::store_with_remote_storage;
use crate::revision_graph::support::with_mutable_store;
use crate::revision_graph::support::with_stores;

/// The keys a set refuses in `entries` and in `remove`, as the amendment lists them.
const READ_ONLY: [&str; 12] = [
    "id",
    "name",
    "category",
    "creator",
    "created",
    "stack",
    "parent",
    "branch-point",
    "archived",
    "restored",
    "restoring",
    "released",
];

fn entry(key: &str, value: LoreMetadata) -> LoreRevisionGraphBranchMetadataEntry {
    LoreRevisionGraphBranchMetadataEntry {
        key: LoreString::from(key),
        value,
    }
}

fn string(text: &str) -> LoreMetadata {
    LoreMetadata::String(LoreString::from(text))
}

fn keys(names: &[&str]) -> Vec<LoreString> {
    names.iter().map(|name| LoreString::from(*name)).collect()
}

fn stored_entry<'a>(key: &'a str, value: &'a [u8], value_type: MetadataType) -> StoredEntry<'a> {
    StoredEntry {
        key,
        value: Cow::Borrowed(value),
        value_type,
    }
}

/// Asserts that a set of `entries` and `remove` on a new branch fails with `InvalidArguments`.
fn assert_refused(entries: &[LoreRevisionGraphBranchMetadataEntry], remove: &[LoreString]) {
    let checked = validate(new_branch(), entries, remove);
    assert!(
        checked
            .as_ref()
            .is_err_and(RevisionGraphError::is_invalid_arguments),
        "{entries:?} and {remove:?} must fail with InvalidArguments, got {checked:?}"
    );
}

/// The FFI code `result` fails with, `None` where it succeeds.
fn failure_code<T>(result: &Result<T, RevisionGraphError>) -> Option<i32> {
    result.as_ref().err().map(|err| err.ffi_code())
}

/// Every entry of `metadata` as (key, value, type), sorted by key.
fn entries_of(metadata: &Metadata) -> Vec<(String, Vec<u8>, MetadataType)> {
    let mut entries = Vec::new();
    metadata.walk(|key, value, value_type| {
        entries.push((
            String::from_utf8(key.to_vec()).expect("a UTF-8 key"),
            value.to_vec(),
            value_type,
        ));
    });
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn owned(key: &str, value: &[u8], value_type: MetadataType) -> (String, Vec<u8>, MetadataType) {
    (key.to_owned(), value.to_vec(), value_type)
}

/// Metadata holding the string `label` "old", the number `count` 1 and the string `owner` "ana".
fn sample() -> Metadata {
    let mut metadata = Metadata::new();
    metadata.set_string("label", "old").expect("sets label");
    metadata.set_u64("count", 1).expect("sets count");
    metadata.set_string("owner", "ana").expect("sets owner");
    metadata
}

/// A zero branch is refused, with or without edits; the same edits on a branch pass.
#[test]
fn a_zero_branch_is_refused() {
    let edits = [
        (Vec::new(), Vec::new()),
        (vec![entry("label", string("x"))], keys(&["old"])),
    ];
    for (entries, remove) in edits {
        let checked = validate(BranchId::default(), &entries, &remove);
        assert!(
            checked
                .as_ref()
                .is_err_and(RevisionGraphError::is_invalid_arguments),
            "{checked:?}"
        );
        validate(new_branch(), &entries, &remove).expect("the edits pass on a branch");
    }
}

/// An empty key is refused in `entries` and in `remove`, alone or beside a valid one.
#[test]
fn an_empty_key_is_refused_in_entries_and_remove() {
    assert_refused(&[entry("", string("x"))], &[]);
    assert_refused(&[entry("label", string("x")), entry("", string("y"))], &[]);
    assert_refused(&[], &keys(&[""]));
    assert_refused(&[], &keys(&["old", ""]));
}

/// Each read-only key is refused in `entries` and in `remove`.
#[test]
fn read_only_keys_are_refused_in_entries_and_remove() {
    for key in READ_ONLY {
        assert_refused(&[entry(key, string("x"))], &[]);
        assert_refused(&[], &keys(&[key]));
    }
}

/// `protect` passes as a Boolean, stored as one byte, and is refused with another type or in
/// `remove`. The stored form of each entry follows `entries`.
#[test]
fn protect_is_set_as_a_boolean_and_never_removed() {
    let entries = [
        entry("protect", LoreMetadata::Boolean(7)),
        entry("label", string("x")),
    ];
    let checked = validate(new_branch(), &entries, &[]).expect("a Boolean protect passes");
    let stored: Vec<_> = checked
        .iter()
        .map(|stored| (stored.key, stored.value.as_ref(), stored.value_type))
        .collect();
    assert_eq!(
        stored,
        [
            ("protect", &[1u8][..], MetadataType::Boolean),
            ("label", &b"x"[..], MetadataType::String),
        ]
    );

    for value in [
        string("true"),
        LoreMetadata::Numeric(1),
        LoreMetadata::Binary(LoreBinary::from_bytes(&[1])),
    ] {
        assert_refused(&[entry("protect", value)], &[]);
    }
    assert_refused(&[], &keys(&["protect"]));
}

/// A key named twice is refused within `entries`, within `remove` and across the two; keys that
/// differ only in case are distinct.
#[test]
fn a_key_named_twice_is_refused() {
    assert_refused(
        &[
            entry("label", string("x")),
            entry("other", string("y")),
            entry("label", string("z")),
        ],
        &[],
    );
    assert_refused(&[], &keys(&["label", "other", "label"]));
    assert_refused(&[entry("label", string("x"))], &keys(&["other", "label"]));

    validate(
        new_branch(),
        &[entry("label", string("x")), entry("Label", string("y"))],
        &keys(&["LABEL"]),
    )
    .expect("keys that differ in case pass");
}

/// A set replaces a value of the same and of another length, adds a key, and removes a key; a
/// key to remove that the metadata lacks is skipped.
#[test]
fn apply_sets_replaces_and_removes() {
    let entries = [
        stored_entry("label", b"new", MetadataType::String),
        stored_entry("owner", b"bo", MetadataType::String),
        stored_entry("note", b"longer text", MetadataType::String),
    ];

    let applied = apply(sample(), &entries, &keys(&["count", "absent"])).expect("applies");

    let Applied::Changed(metadata) = applied else {
        panic!("the edits change the metadata: {applied:?}");
    };
    assert_eq!(
        entries_of(&metadata),
        [
            owned("label", b"new", MetadataType::String),
            owned("note", b"longer text", MetadataType::String),
            owned("owner", b"bo", MetadataType::String),
        ]
    );
}

/// No edits, and removals of keys the metadata lacks, leave the metadata unchanged.
#[test]
fn apply_reports_unchanged_metadata() {
    for remove in [keys(&[]), keys(&["absent", "missing"])] {
        let applied = apply(sample(), &[], &remove).expect("applies");

        let Applied::Unchanged(metadata) = applied else {
            panic!("{remove:?} leaves the metadata unchanged: {applied:?}");
        };
        assert_eq!(entries_of(&metadata), entries_of(&sample()));
    }
}

/// A removal changes the metadata alone and beside a set of the value a key holds.
#[test]
fn apply_reports_a_removal_as_changed() {
    let held = [stored_entry("label", b"old", MetadataType::String)];
    for entries in [&[][..], &held[..]] {
        let applied = apply(sample(), entries, &keys(&["count"])).expect("applies");

        let Applied::Changed(metadata) = applied else {
            panic!("removing count changes the metadata: {applied:?}");
        };
        assert_eq!(
            entries_of(&metadata),
            [
                owned("label", b"old", MetadataType::String),
                owned("owner", b"ana", MetadataType::String),
            ]
        );
    }
}

/// Setting keys to the values and types they hold leaves the metadata unchanged; the same bytes
/// under another type change it.
#[test]
fn apply_reports_a_set_of_the_value_a_key_holds_as_unchanged() {
    let one = 1u64.to_le_bytes();
    let held = [
        stored_entry("label", b"old", MetadataType::String),
        stored_entry("count", &one, MetadataType::Numeric),
    ];

    let applied = apply(sample(), &held, &keys(&["absent"])).expect("applies");

    let Applied::Unchanged(metadata) = applied else {
        panic!("values the keys hold leave the metadata unchanged: {applied:?}");
    };
    assert_eq!(entries_of(&metadata), entries_of(&sample()));

    let retyped = [stored_entry("label", b"old", MetadataType::Binary)];

    let applied = apply(sample(), &retyped, &[]).expect("applies");

    let Applied::Changed(metadata) = applied else {
        panic!("another type changes the metadata: {applied:?}");
    };
    assert_eq!(
        metadata.get_typed("label").expect("holds label"),
        (&b"old"[..], MetadataType::Binary)
    );
}

/// Metadata of exactly 1 MiB after the edits passes; one byte more fails with `Oversized`, as
/// does an entry too large to fit alone.
#[test]
fn apply_refuses_a_result_over_one_mebibyte() {
    let bulk = vec![0u8; METADATA_MAX_SIZE / 2];
    let mut half = Metadata::new();
    half.set_binary("bulk", &bulk).expect("sets bulk");
    let room = METADATA_MAX_SIZE
        - Metadata::stored_size("bulk".len(), bulk.len())
        - Metadata::ITEM_SIZE
        - "more".len();
    let fits = vec![1u8; room];
    let over = vec![1u8; room + 1];
    let whole = vec![2u8; METADATA_MAX_SIZE];

    let at_limit = apply(
        half.clone(),
        &[stored_entry("more", &fits, MetadataType::Binary)],
        &[],
    );
    let past_limit = apply(
        half,
        &[stored_entry("more", &over, MetadataType::Binary)],
        &[],
    );
    let alone = apply(
        Metadata::new(),
        &[stored_entry("bulk", &whole, MetadataType::Binary)],
        &[],
    );

    assert_eq!(failure_code(&at_limit), None);
    assert!(matches!(at_limit, Ok(Applied::Changed(_))));
    assert_eq!(failure_code(&past_limit), Some(Oversized::FFI_CODE));
    assert_eq!(failure_code(&alone), Some(Oversized::FFI_CODE));
}

/// Entries that fit alone but together fill the metadata to exactly 1 MiB pass; one byte more
/// fails with `Oversized`.
#[test]
fn apply_refuses_entries_that_together_exceed_one_mebibyte() {
    let first = vec![1u8; METADATA_MAX_SIZE / 2];
    let room = METADATA_MAX_SIZE
        - Metadata::stored_size("first".len(), first.len())
        - Metadata::ITEM_SIZE
        - "second".len();
    let fits = vec![2u8; room];
    let over = vec![2u8; room + 1];

    let at_limit = apply(
        Metadata::new(),
        &[
            stored_entry("first", &first, MetadataType::Binary),
            stored_entry("second", &fits, MetadataType::Binary),
        ],
        &[],
    );
    let past_limit = apply(
        sample(),
        &[
            stored_entry("first", &first, MetadataType::Binary),
            stored_entry("second", &over, MetadataType::Binary),
        ],
        &[],
    );

    let Ok(Applied::Changed(metadata)) = at_limit else {
        panic!("entries filling the metadata to the limit pass: {at_limit:?}");
    };
    assert_eq!(metadata.buffer.len(), METADATA_MAX_SIZE);
    assert_eq!(failure_code(&past_limit), Some(Oversized::FFI_CODE));
}

/// The `id` every verb call in this file passes.
const CALL_ID: u64 = 66;

/// Runs `branch_metadata_set` on `store` for `branch` in `repository` under `globals`, setting
/// `entries` and removing `remove`, and returns its status and every event it sent.
async fn set_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    entries: Vec<LoreRevisionGraphBranchMetadataEntry>,
    remove: &[&str],
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_metadata_set(
        globals,
        LoreRevisionGraphBranchMetadataSetArgs {
            id: CALL_ID,
            store,
            repository,
            branch,
            entries: LoreArray::from_vec(entries),
            remove: LoreArray::from_vec(keys(remove)),
        },
        callback,
    )
    .await;
    let events = std::mem::take(&mut *events.lock().unwrap());
    (status, events)
}

/// [`set_with`] under default globals.
async fn set(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    entries: Vec<LoreRevisionGraphBranchMetadataEntry>,
    remove: &[&str],
) -> (i32, Vec<LoreEvent>) {
    set_with(
        LoreGlobalArgs::default(),
        store,
        repository,
        branch,
        entries,
        remove,
    )
    .await
}

/// Globals with the `remote` flag.
fn remote_only() -> LoreGlobalArgs {
    LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    }
}

/// The value of the metadata key of `branch` in the local store of `store`, `None` where it's
/// absent.
async fn metadata_key(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
) -> Option<Hash> {
    in_setup_context(branch::mutable_value(
        local_read_context(store, repository),
        branch::METADATA,
        branch,
    ))
    .await
    .expect("loads the metadata key")
}

/// Edits the metadata of `branch` in the local store of `store` with `edit`, as another writer
/// would, and returns the metadata the branch then holds.
async fn edit_local(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    edit: impl FnOnce(&mut Metadata),
) -> Metadata {
    let mut metadata = local_metadata(store, repository, branch).await;
    edit(&mut metadata);
    in_setup_context(store_metadata(
        &local_write_context(store, repository),
        branch,
        metadata.clone(),
    ))
    .await;
    metadata
}

/// Writes `metadata` as a blob to the local store of `store`, without pointing any key at it.
async fn local_blob(store: &StoreInternal, repository: Partition, metadata: &Metadata) -> Hash {
    let write = local_write_context(store, repository);
    in_setup_context(metadata.serialize(write))
        .await
        .expect("writes the blob")
}

/// `content` stored in the local store of `store`, with its payload where `payload` is set, and
/// otherwise as the entry an eviction leaves.
async fn stored_locally(
    store: &StoreInternal,
    repository: Partition,
    content: &'static [u8],
    payload: bool,
) -> Address {
    let address = Address {
        hash: lore_storage::hash::hash_slice(content),
        context: Context::default(),
    };
    let fragment = Fragment {
        flags: if payload {
            0
        } else {
            FragmentFlags::PayloadStoredDurable.bits()
        },
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    store
        .immutable
        .clone()
        .put(
            repository,
            address,
            fragment,
            payload.then(|| Bytes::from_static(content)),
            false,
        )
        .await
        .expect("the local store takes the entry");
    address
}

/// A handle over `store` whose mutable store, after the first load of the metadata key of
/// `branch` and after each swap of that key, stores the next of `writes` in it, as another writer
/// would, while any remain. Returns the handle and the number of swaps of the key.
fn overtaking(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    writes: Vec<Hash>,
) -> (LoreStore, Arc<AtomicUsize>) {
    let key = branch::mutable_key(
        local_read_context(store, repository).salt(),
        branch::METADATA,
        repository,
        branch,
    )
    .0;
    let writes = Mutex::new(VecDeque::from(writes));
    let loaded = AtomicBool::new(false);
    let swaps = Arc::new(AtomicUsize::new(0));
    let counted = swaps.clone();
    let mutable = HookedStore::wrap(
        store.mutable.clone(),
        |_, _, _| Ok(()),
        answered(move |inner, partition, call, answered_key, key_type| {
            let overtakes = answered_key == key
                && match call {
                    StoreCall::Load => !loaded.swap(true, Ordering::SeqCst),
                    StoreCall::CompareAndSwap => {
                        counted.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                    _ => false,
                };
            let write = overtakes
                .then(|| writes.lock().unwrap().pop_front())
                .flatten();
            async move {
                if let Some(value) = write {
                    inner
                        .store(partition, answered_key, value, key_type)
                        .await?;
                }
                Ok(())
            }
        }),
    );
    (
        handle::register(Arc::new(with_mutable_store(store, mutable))),
        swaps,
    )
}

/// A handle over `store` that counts the writes reaching its stores: each store or swap of a
/// mutable key, and each put of immutable content.
fn counting_writes(store: &StoreInternal) -> (LoreStore, Arc<AtomicUsize>) {
    let writes = Arc::new(AtomicUsize::new(0));
    let (mutable_writes, immutable_writes) = (writes.clone(), writes.clone());
    let mutable = HookedStore::wrap(
        store.mutable.clone(),
        move |call, _, _| {
            if matches!(call, StoreCall::Store | StoreCall::CompareAndSwap) {
                mutable_writes.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        },
        None,
    );
    let immutable = HookedImmutableStore::wrap(store.immutable.clone(), move |call, _, _| {
        if call == ImmutableCall::Put {
            immutable_writes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    });
    (
        handle::register(Arc::new(with_stores(store, immutable, mutable))),
        writes,
    )
}

/// The metadata of a branch at the remote: its id, its name and an `owner`.
fn remote_metadata(branch: BranchId) -> Metadata {
    partial_metadata(
        Some(branch),
        &[(branch::NAME, "feature"), ("owner", "remote")],
    )
}

/// `metadata` with the string `owner` set to "ci", the edit most tests here make.
fn owned_by_ci(metadata: &Metadata) -> Metadata {
    let mut edited = metadata.clone();
    edited.set_string("owner", "ci").expect("sets owner");
    edited
}

/// The entries of the edit [`owned_by_ci`] makes.
fn owner_ci() -> Vec<LoreRevisionGraphBranchMetadataEntry> {
    vec![entry("owner", string("ci"))]
}

/// A set edits the local metadata: it replaces a value, adds keys of other types and removes a
/// key, keeps the entries it doesn't name, and reports every entry the branch then holds, built-in
/// ones included, in stored order, followed by the location complete event that counts them.
#[tokio::test]
async fn a_set_edits_the_local_metadata_and_reports_every_entry() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let before = edit_local(&store, repository, feature, |metadata| {
        metadata.set_string("label", "old").expect("sets label");
        metadata.set_string("stale", "x").expect("sets stale");
    })
    .await;

    let (status, events) = set(
        handle,
        repository,
        feature,
        vec![
            entry("label", string("new")),
            entry("count", LoreMetadata::Numeric(3)),
            entry("protect", LoreMetadata::Boolean(1)),
        ],
        &["stale"],
    )
    .await;
    close_handle(handle).await;

    let mut expected = before.clone();
    expected.set_string("label", "new").expect("sets label");
    expected.set_u64("count", 3).expect("sets count");
    expected.set_bool("protect", true).expect("sets protect");
    expected.remove_key("stale");
    let after = local_metadata(&store, repository, feature).await;
    assert_eq!(status, 0);
    assert_eq!(
        entries_at(LoreBranchLocation::Local, &after),
        entries_at(LoreBranchLocation::Local, &expected)
    );
    let records = reported(&events, CALL_ID, feature);
    assert_eq!(records, entries_at(LoreBranchLocation::Local, &expected));
    assert!(records.iter().any(|(_, key, _)| key == branch::NAME));
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, records.len() as u64, 0)]
    );
    let [
        ..,
        LoreEvent::RevisionGraphLocationComplete(_),
        LoreEvent::Complete(_),
        LoreEvent::End(_),
    ] = &without_log_lines(&events)[..]
    else {
        panic!("expected the location complete event, then Complete, then End, last");
    };
}

/// A set without edits, and one of the values its keys hold, writes nothing to either store and
/// reports every entry the branch holds.
#[tokio::test]
async fn an_unchanged_set_writes_nothing_and_reports_the_metadata() {
    let store = in_memory_for_tests("graph-branch-metadata-set-unchanged").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let metadata = edit_local(&store, repository, feature, |metadata| {
        metadata.set_string("owner", "ci").expect("sets owner");
    })
    .await;
    let blob = metadata_key(&store, repository, feature).await;
    let (handle, writes) = counting_writes(&store);

    for (edits, entries, remove) in [
        ("no edit", Vec::new(), &[][..]),
        ("the values the keys hold", owner_ci(), &["absent"][..]),
    ] {
        let (status, events) = set(handle, repository, feature, entries, remove).await;

        let records = reported(&events, CALL_ID, feature);
        assert_eq!(status, 0, "{edits}");
        assert_eq!(
            records,
            entries_at(LoreBranchLocation::Local, &metadata),
            "{edits}"
        );
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, records.len() as u64, 0)],
            "{edits}"
        );
    }
    close_handle(handle).await;
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(metadata_key(&store, repository, feature).await, blob);
}

/// A swap another writer overtook reads the metadata the swap answered and applies the edit to it
/// again, so the other writer's entry stays beside the set's, and the records report the merged
/// metadata.
#[tokio::test]
async fn a_lost_local_swap_reapplies_over_the_other_writers_entries() {
    let store = in_memory_for_tests("graph-branch-metadata-set-lost-swap").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let mut other = local_metadata(&store, repository, feature).await;
    other.set_string("other", "writer").expect("sets other");
    let other_blob = local_blob(&store, repository, &other).await;
    let (handle, swaps) = overtaking(&store, repository, feature, vec![other_blob]);

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let expected = owned_by_ci(&other);
    let records = reported(&events, CALL_ID, feature);
    assert_eq!(status, 0);
    assert_eq!(records, entries_at(LoreBranchLocation::Local, &expected));
    assert_eq!(
        entries_at(
            LoreBranchLocation::Local,
            &local_metadata(&store, repository, feature).await
        ),
        records
    );
    assert_eq!(swaps.load(Ordering::SeqCst), 2);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, records.len() as u64, 0)]
    );
}

/// A swap lost to a metadata key another writer removed, as a create that loses its name claim
/// rolls it back, is a miss: the call fails with `BranchNotFound` and the key stays absent.
#[tokio::test]
async fn a_swap_lost_to_a_removed_metadata_key_is_a_miss() {
    let store = in_memory_for_tests("graph-branch-metadata-set-removed-key").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let (handle, swaps) = overtaking(&store, repository, feature, vec![Hash::default()]);

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
    assert_eq!(swaps.load(Ordering::SeqCst), 1);
    assert_eq!(metadata_key(&store, repository, feature).await, None);
}

/// A swap another writer overtook with a hash whose blob the local store doesn't hold leaves the
/// store without metadata for the branch: a miss after one swap, and the key keeps the other
/// writer's value.
#[tokio::test]
async fn a_lost_local_swap_to_a_blob_the_store_lacks_is_a_miss() {
    let store = in_memory_for_tests("graph-branch-metadata-set-lost-to-a-missing-blob").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let (handle, swaps) = overtaking(&store, repository, feature, vec![hash(9)]);

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
    assert_eq!(swaps.load(Ordering::SeqCst), 1);
    assert_eq!(
        metadata_key(&store, repository, feature).await,
        Some(hash(9))
    );
}

/// Another writer that changes the metadata before each of the set's swaps fails the location
/// with `BranchAdvanced` at the eighth, and the key keeps the other writer's value.
#[tokio::test]
async fn the_eighth_lost_swap_fails_branch_advanced() {
    let store = in_memory_for_tests("graph-branch-metadata-set-eighth-swap").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let original = local_metadata(&store, repository, feature).await;
    let mut blobs = Vec::new();
    for value in ["first", "second"] {
        let mut other = original.clone();
        other.set_string("other", value).expect("sets other");
        blobs.push(local_blob(&store, repository, &other).await);
    }
    let writes: Vec<Hash> = (0..8).map(|write| blobs[write % 2]).collect();
    let (handle, swaps) = overtaking(&store, repository, feature, writes);

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, BranchAdvanced::FFI_CODE)]
    );
    assert_eq!(swaps.load(Ordering::SeqCst), 8);
    assert_eq!(
        metadata_key(&store, repository, feature).await,
        Some(blobs[1])
    );
}

/// An address the entries set whose payload the local store lacks fails the location with
/// `AddressNotFound` before any write, whether the store never held it or holds its entry without
/// the payload, as after an eviction; a failed query of an address fails the location. An address
/// the store holds passes, and one with a zero hash names empty content and needs no check.
#[tokio::test]
async fn an_address_the_local_store_lacks_fails_address_not_found() {
    let store = in_memory_for_tests("graph-branch-metadata-set-local-address").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let blob = metadata_key(&store, repository, feature).await;
    let held = stored_locally(&store, repository, b"a payload the store holds", true).await;
    let evicted = stored_locally(&store, repository, b"a payload the store evicted", false).await;
    let never = Address {
        hash: hash(9),
        context: Context::default(),
    };
    let (plain, writes) = counting_writes(&store);
    let failing_query = handle::register(Arc::new(with_stores(
        &store,
        HookedImmutableStore::wrap(store.immutable.clone(), move |call, _, address| {
            if call == ImmutableCall::Query && address == held {
                return Err(StoreError::from(Disconnected));
            }
            Ok(())
        }),
        store.mutable.clone(),
    )));

    for (lacking, handle, address, code) in [
        ("never held", plain, never, AddressNotFound::FFI_CODE),
        ("evicted", plain, evicted, AddressNotFound::FFI_CODE),
        ("unreadable", failing_query, held, Disconnected::FFI_CODE),
    ] {
        let entries = vec![
            entry("owner", string("ci")),
            entry("asset", LoreMetadata::Address(address)),
        ];
        let (status, events) = set(handle, repository, feature, entries, &[]).await;

        assert_eq!(status, code, "{lacking}");
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, 0, code)],
            "{lacking}"
        );
        assert_eq!(
            metadata_key(&store, repository, feature).await,
            blob,
            "{lacking}"
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0, "{lacking}");
    }
    close_handle(failing_query).await;

    let empty = Address {
        hash: Hash::default(),
        context: Context::from([5u8; 16]),
    };
    let entries = vec![
        entry("asset", LoreMetadata::Address(held)),
        entry("empty", LoreMetadata::Address(empty)),
    ];
    let (status, _) = set(plain, repository, feature, entries, &[]).await;
    close_handle(plain).await;
    let after = local_metadata(&store, repository, feature).await;
    assert_eq!(status, 0);
    assert_eq!(after.get_address("asset").expect("holds asset"), held);
    assert_eq!(after.get_address("empty").expect("holds empty"), empty);
}

/// The local store checks only the addresses the entries set: an address another writer set,
/// whose payload the store never held, stays and doesn't fail the set.
#[tokio::test]
async fn an_address_another_writer_set_is_not_checked_locally() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let other = Address {
        hash: hash(9),
        context: Context::default(),
    };
    let before = edit_local(&store, repository, feature, |metadata| {
        metadata.set_address("asset", other).expect("sets asset");
    })
    .await;

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let after = local_metadata(&store, repository, feature).await;
    assert_eq!(status, 0);
    assert_eq!(after.get_address("asset").expect("holds asset"), other);
    assert_eq!(
        reported(&events, CALL_ID, feature),
        entries_at(LoreBranchLocation::Local, &owned_by_ci(&before))
    );
}

/// Metadata the edit leaves over 1 MiB fails the location with `Oversized` before any check or
/// write: no content reaches the immutable store and the metadata key keeps its value.
#[tokio::test]
async fn metadata_over_one_mebibyte_fails_oversized_and_writes_nothing() {
    let store = in_memory_for_tests("graph-branch-metadata-set-oversized").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    edit_local(&store, repository, feature, |metadata| {
        metadata
            .set_binary("bulk", &vec![0u8; METADATA_MAX_SIZE / 2])
            .expect("sets bulk");
    })
    .await;
    let blob = metadata_key(&store, repository, feature).await;
    let (handle, writes) = counting_writes(&store);

    let more = LoreMetadata::Binary(LoreBinary::from_bytes(&vec![1u8; METADATA_MAX_SIZE / 2]));
    let (status, events) = set(handle, repository, feature, vec![entry("more", more)], &[]).await;
    close_handle(handle).await;

    assert_eq!(status, Oversized::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, Oversized::FFI_CODE)]
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(metadata_key(&store, repository, feature).await, blob);
}

/// A branch the local store doesn't hold is a miss there: the call fails with `BranchNotFound`,
/// reports no record, and writes no metadata key.
#[tokio::test]
async fn a_branch_the_local_store_lacks_is_a_miss() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
    assert_eq!(metadata_key(&store, repository, feature).await, None);
}

/// An archived branch is edited like a live one: the set keeps the entries its archive wrote and
/// reports them beside its own.
#[tokio::test]
async fn an_archived_branch_is_edited_like_a_live_one() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    archive(&store, repository, feature, "feature").await;
    let archived = local_metadata(&store, repository, feature).await;

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let after = local_metadata(&store, repository, feature).await;
    let records = reported(&events, CALL_ID, feature);
    assert_eq!(status, 0);
    assert_eq!(
        entries_at(LoreBranchLocation::Local, &after),
        entries_at(LoreBranchLocation::Local, &owned_by_ci(&archived))
    );
    assert_eq!(records, entries_at(LoreBranchLocation::Local, &after));
    assert!(records.iter().any(|(_, key, _)| key == branch::ARCHIVED));
}

/// A failure at the remote, a refused metadata get or a metadata blob the remote lacks, fails the
/// call before it reaches the local store: no local complete event follows the remote's, and the
/// local metadata keeps its entries.
#[tokio::test]
async fn a_remote_failure_stops_the_call_before_the_local_store() {
    let repository = new_repository();
    let feature = new_branch();

    for (answered, answer, code) in [
        (
            "SlowDown",
            MetadataAnswer::Fails(ProtocolError::from(SlowDown)),
            SlowDown::FFI_CODE,
        ),
        (
            "a blob it lacks",
            MetadataAnswer::Hash(hash(9)),
            AddressNotFound::FFI_CODE,
        ),
    ] {
        let service = NamingRevision::setting_metadata(answer, [SetAnswer::Won]);
        let store =
            store_with_remote_storage(repository, service.clone(), vec![MemoryPeer::new()]).await;
        create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
        let before = local_metadata(&store, repository, feature).await;
        let store = Arc::new(store);
        let handle = handle::register(store.clone());

        let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
        close_handle(handle).await;

        assert_eq!(status, code, "the remote answers {answered}");
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Remote, 0, code)],
            "the remote answers {answered}"
        );
        assert!(service.metadata_sets.lock().unwrap().is_empty());
        assert_eq!(
            entries_at(
                LoreBranchLocation::Local,
                &local_metadata(&store, repository, feature).await
            ),
            entries_at(LoreBranchLocation::Local, &before),
            "the remote answers {answered}"
        );
    }
}

/// With no flag, a remote that doesn't hold the branch is a miss there, and the call still edits
/// the local store and succeeds, with no swap at the remote.
#[tokio::test]
async fn a_remote_miss_still_edits_the_local_store() {
    let repository = new_repository();
    let feature = new_branch();
    let service = NamingRevision::setting_metadata(MetadataAnswer::NotFound, [SetAnswer::Won]);
    let store = Arc::new(
        store_with_remote_storage(repository, service.clone(), vec![MemoryPeer::new()]).await,
    );
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let handle = handle::register(store.clone());

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let after = local_metadata(&store, repository, feature).await;
    let records = reported(&events, CALL_ID, feature);
    assert_eq!(status, 0);
    assert_eq!(after.get_string("owner").expect("holds owner"), "ci");
    assert_eq!(records, entries_at(LoreBranchLocation::Local, &after));
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, records.len() as u64, 0)
        ]
    );
    assert!(service.metadata_sets.lock().unwrap().is_empty());
}

/// With no flag, a failure in the local store after the remote's edit succeeded fails the call
/// with the local store's error, while the remote's complete event counts its records.
#[tokio::test]
async fn a_local_failure_after_a_remote_success_fails_the_call() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let service = NamingRevision::setting_metadata(MetadataAnswer::Hash(blob), [SetAnswer::Won]);
    let store = store_with_remote_storage(repository, service.clone(), vec![peer]).await;
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let handle = handle_failing(&store, |call, _, _| call == StoreCall::CompareAndSwap);

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let remote = entries_at(
        LoreBranchLocation::Remote,
        &owned_by_ci(&remote_metadata(feature)),
    );
    assert_eq!(status, Disconnected::FFI_CODE);
    assert_eq!(reported(&events, CALL_ID, feature), remote);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, remote.len() as u64, 0),
            (LoreBranchLocation::Local, 0, Disconnected::FFI_CODE)
        ]
    );
    assert_eq!(service.metadata_sets.lock().unwrap().len(), 1);
}

/// A `protect` change the remote refuses for want of repository administration fails the call
/// with `NotAuthorized` in the remote's complete event, and no local complete event follows: the
/// local store stays unprotected.
#[tokio::test]
async fn a_protect_change_the_remote_refuses_fails_not_authorized() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let service = NamingRevision::setting_metadata(
        MetadataAnswer::Hash(blob),
        [SetAnswer::Refused(ProtocolError::from(NotAuthorized))],
    );
    let store = Arc::new(store_with_remote_storage(repository, service.clone(), vec![peer]).await);
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let handle = handle::register(store.clone());

    let entries = vec![entry("protect", LoreMetadata::Boolean(1))];
    let (status, events) = set(handle, repository, feature, entries, &[]).await;
    close_handle(handle).await;

    assert_eq!(status, NotAuthorized::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, NotAuthorized::FFI_CODE)]
    );
    let sets = service.metadata_sets.lock().unwrap().clone();
    assert!(matches!(sets[..], [(expected, _)] if expected == blob));
    assert!(!branch::protected(
        &local_metadata(&store, repository, feature).await
    ));
}

/// An upload the remote refuses doesn't stop the set: the swap is still asked for, and its refusal
/// is the remote's outcome.
#[tokio::test]
async fn an_upload_the_remote_refuses_still_reaches_the_swap() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    peer.refuse_puts(ProtocolError::from(NotAuthorized));
    let puts = peer.puts();
    let service = NamingRevision::setting_metadata(
        MetadataAnswer::Hash(blob),
        [SetAnswer::Refused(ProtocolError::from(NotAuthorized))],
    );
    let store = store_with_remote_storage(repository, service.clone(), vec![peer.clone()]).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) =
        set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, NotAuthorized::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, NotAuthorized::FFI_CODE)]
    );
    assert!(peer.puts() > puts, "the upload was tried");
    let sets = service.metadata_sets.lock().unwrap().clone();
    assert!(matches!(sets[..], [(expected, _)] if expected == blob));
}

/// A remote swap lost to zero, where the branch's metadata went, is a miss at the remote: with the
/// `remote` flag the call fails with `BranchNotFound`, and the set isn't asked for again.
#[tokio::test]
async fn a_remote_swap_lost_to_zero_is_a_miss() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let service = NamingRevision::setting_metadata(
        MetadataAnswer::Hash(blob),
        [SetAnswer::Lost(Hash::default()), SetAnswer::Won],
    );
    let store = store_with_remote_storage(repository, service.clone(), vec![peer]).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) =
        set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, 0)]
    );
    assert_eq!(service.metadata_sets.lock().unwrap().len(), 1);
}

/// A remote swap another writer overtook with a hash whose blob the remote lacks fails the remote
/// with `AddressNotFound`, from the read of that blob, after one swap.
#[tokio::test]
async fn a_lost_remote_swap_to_a_blob_the_remote_lacks_fails_address_not_found() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let service = NamingRevision::setting_metadata(
        MetadataAnswer::Hash(blob),
        [SetAnswer::Lost(hash(9)), SetAnswer::Won],
    );
    let store = store_with_remote_storage(repository, service.clone(), vec![peer]).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) =
        set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, AddressNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, AddressNotFound::FFI_CODE)]
    );
    assert_eq!(service.metadata_sets.lock().unwrap().len(), 1);
}

/// A `NotFound` from the remote's swap whose metadata get still finds the branch's metadata fails
/// the remote with `Internal`.
#[tokio::test]
async fn a_remote_not_found_whose_metadata_remains_fails_internal() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let service = NamingRevision::setting_metadata(
        MetadataAnswer::Hash(blob),
        [SetAnswer::Refused(ProtocolError::from(NotFound))],
    );
    let store = store_with_remote_storage(repository, service.clone(), vec![peer]).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) =
        set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, Internal::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, Internal::FFI_CODE)]
    );
    assert_eq!(service.metadata_sets.lock().unwrap().len(), 1);
    assert_eq!(*service.metadata_gets.lock().unwrap(), [feature, feature]);
}

/// A remote whose swap another writer overtakes every time fails with `BranchAdvanced` after
/// exactly eight swaps, the first from the metadata it read and the others from the metadata each
/// lost swap answered.
#[tokio::test]
async fn the_eighth_lost_remote_swap_fails_branch_advanced() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let other = put_metadata_blob(
        &peer,
        &partial_metadata(Some(feature), &[("other", "writer")]),
    )
    .await;
    let service = NamingRevision::setting_metadata(
        MetadataAnswer::Hash(blob),
        (0..9).map(|_| SetAnswer::Lost(other)),
    );
    let store = store_with_remote_storage(repository, service.clone(), vec![peer]).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) =
        set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, BranchAdvanced::FFI_CODE)]
    );
    let expected: Vec<Hash> = service
        .metadata_sets
        .lock()
        .unwrap()
        .iter()
        .map(|(expected, _)| *expected)
        .collect();
    assert_eq!(expected, [vec![blob], vec![other; 7]].concat());
}

/// Seven lost swaps at the remote, then a won one, succeed: the records report the metadata the
/// last lost swap answered, with the set's entry.
#[tokio::test]
async fn seven_lost_swaps_then_a_win_succeed() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let other_metadata = partial_metadata(Some(feature), &[("other", "writer")]);
    let other = put_metadata_blob(&peer, &other_metadata).await;
    let answers = (0..7)
        .map(|_| SetAnswer::Lost(other))
        .chain([SetAnswer::Won]);
    let service = NamingRevision::setting_metadata(MetadataAnswer::Hash(blob), answers);
    let store = store_with_remote_storage(repository, service.clone(), vec![peer]).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) =
        set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let records = reported(&events, CALL_ID, feature);
    assert_eq!(status, 0);
    assert_eq!(
        records,
        entries_at(LoreBranchLocation::Remote, &owned_by_ci(&other_metadata))
    );
    assert_eq!(service.metadata_sets.lock().unwrap().len(), 8);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, records.len() as u64, 0)]
    );
}

/// An entry no record can carry, a hash value one byte short, is left out of each location's
/// records while the set keeps it in the metadata it writes, and the location complete events
/// count only the entries reported.
#[tokio::test]
async fn a_set_skipping_entries_leaves_them_out_at_each_location() {
    let repository = new_repository();
    let feature = new_branch();
    let mut metadata = partial_metadata(Some(feature), &[(branch::NAME, "feature")]);
    metadata
        .set_typed("short", &[1; 31], MetadataType::Hash)
        .expect("sets the entry");
    let readable = partial_metadata(Some(feature), &[(branch::NAME, "feature"), ("owner", "ci")]);
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &metadata).await;
    let service = NamingRevision::setting_metadata(MetadataAnswer::Hash(blob), [SetAnswer::Won]);
    let store = Arc::new(store_with_remote_storage(repository, service, vec![peer]).await);
    in_setup_context(store_metadata(
        &local_write_context(&store, repository),
        feature,
        metadata,
    ))
    .await;
    let handle = handle::register(store.clone());

    let (status, events) = set(handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let (remote, local) = (
        entries_at(LoreBranchLocation::Remote, &readable),
        entries_at(LoreBranchLocation::Local, &readable),
    );
    assert_eq!(status, 0);
    assert_eq!(
        reported(&events, CALL_ID, feature),
        [remote.clone(), local.clone()].concat()
    );
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, remote.len() as u64, 0),
            (LoreBranchLocation::Local, local.len() as u64, 0)
        ]
    );
    assert_eq!(
        local_metadata(&store, repository, feature)
            .await
            .get_typed("short")
            .expect("keeps the entry"),
        (&[1u8; 31][..], MetadataType::Hash)
    );
}

/// A remote swap another writer overtook reads the metadata the swap answered from the remote and
/// swaps again from it: the records report the other writer's entry beside the set's.
#[tokio::test]
async fn a_lost_remote_swap_reapplies_over_the_returned_metadata() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let mut other_metadata = remote_metadata(feature);
    other_metadata
        .set_string("other", "writer")
        .expect("sets other");
    let other = put_metadata_blob(&peer, &other_metadata).await;
    let service = NamingRevision::setting_metadata(
        MetadataAnswer::Hash(blob),
        [SetAnswer::Lost(other), SetAnswer::Won],
    );
    let store = store_with_remote_storage(repository, service.clone(), vec![peer]).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) =
        set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    close_handle(handle).await;

    let records = reported(&events, CALL_ID, feature);
    assert_eq!(status, 0);
    assert_eq!(
        records,
        entries_at(LoreBranchLocation::Remote, &owned_by_ci(&other_metadata))
    );
    let sets = service.metadata_sets.lock().unwrap().clone();
    let [(first, first_updated), (second, second_updated)] = sets[..] else {
        panic!("expected two sets, got {sets:?}");
    };
    assert_eq!((first, second), (blob, other));
    assert_ne!(first_updated, second_updated);
}

/// A `NotFound` from the remote's swap after the existence query found every address is a miss
/// once the remote's metadata get finds no metadata either.
#[tokio::test]
async fn a_remote_not_found_after_the_existence_query_is_a_miss() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let payload = Address {
        hash: put_metadata_blob(&peer, &partial_metadata(None, &[("asset", "payload")])).await,
        context: Context::default(),
    };
    let service =
        NamingRevision::setting_metadata(MetadataAnswer::Hash(blob), [SetAnswer::NotFound]);
    let store = store_with_remote_storage(repository, service.clone(), vec![peer.clone()]).await;
    let handle = handle::register(Arc::new(store));

    let entries = vec![entry("asset", LoreMetadata::Address(payload))];
    let (status, events) = set_with(remote_only(), handle, repository, feature, entries, &[]).await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, 0)]
    );
    assert_eq!(peer.queries(), 1);
    assert_eq!(service.metadata_sets.lock().unwrap().len(), 1);
    assert_eq!(*service.metadata_gets.lock().unwrap(), [feature, feature]);
}

/// The remote checks every address of the new metadata, one the entries set and one the metadata
/// already held alike, and fails with `AddressNotFound` for one it lacks before any write or swap.
#[tokio::test]
async fn an_address_the_remote_lacks_fails_address_not_found_before_the_swap() {
    let repository = new_repository();
    let feature = new_branch();
    let lacking = Address {
        hash: hash(9),
        context: Context::default(),
    };
    let mut held_by_metadata = remote_metadata(feature);
    held_by_metadata
        .set_address("asset", lacking)
        .expect("sets asset");

    for (named, metadata, entries) in [
        (
            "by the entries",
            remote_metadata(feature),
            vec![entry("asset", LoreMetadata::Address(lacking))],
        ),
        ("by the metadata", held_by_metadata, owner_ci()),
    ] {
        let peer = MemoryPeer::new();
        let blob = put_metadata_blob(&peer, &metadata).await;
        let puts = peer.puts();
        let service =
            NamingRevision::setting_metadata(MetadataAnswer::Hash(blob), [SetAnswer::Won]);
        let store =
            store_with_remote_storage(repository, service.clone(), vec![peer.clone()]).await;
        let handle = handle::register(Arc::new(store));

        let (status, events) =
            set_with(remote_only(), handle, repository, feature, entries, &[]).await;
        close_handle(handle).await;

        assert_eq!(status, AddressNotFound::FFI_CODE, "named {named}");
        assert_eq!(peer.puts(), puts, "named {named}");
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Remote, 0, AddressNotFound::FFI_CODE)],
            "named {named}"
        );
        assert!(
            service.metadata_sets.lock().unwrap().is_empty(),
            "named {named}"
        );
    }
}

/// The work at the remote makes no call on the local mutable store, and on the local immutable
/// store only the write pipeline's query and put of the blob's one fragment, which the store then
/// records without its payload.
#[tokio::test]
async fn the_remote_work_leaves_only_the_pipelines_entries_locally() {
    let repository = new_repository();
    let feature = new_branch();
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata(feature)).await;
    let service = NamingRevision::setting_metadata(MetadataAnswer::Hash(blob), [SetAnswer::Won]);
    let base = store_with_remote_storage(repository, service.clone(), vec![peer.clone()]).await;
    let mutable_calls = Arc::new(AtomicUsize::new(0));
    let counted = mutable_calls.clone();
    let mutable = HookedStore::wrap(
        base.mutable.clone(),
        move |_, _, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        None,
    );
    let immutable_calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = immutable_calls.clone();
    let immutable = HookedImmutableStore::wrap(base.immutable.clone(), move |call, _, address| {
        recorded.lock().unwrap().push((call, address));
        Ok(())
    });
    let handle = handle::register(Arc::new(with_stores(&base, immutable, mutable)));

    let (status, _) = set_with(remote_only(), handle, repository, feature, owner_ci(), &[]).await;
    let (mutable_calls, immutable_calls) = (
        mutable_calls.load(Ordering::SeqCst),
        immutable_calls.lock().unwrap().clone(),
    );
    close_handle(handle).await;

    assert_eq!(status, 0);
    let sets = service.metadata_sets.lock().unwrap().clone();
    let [(_, updated)] = sets[..] else {
        panic!("expected one set, got {sets:?}");
    };
    let written = Address {
        hash: updated,
        context: Context::default(),
    };
    assert_eq!(mutable_calls, 0);
    assert_eq!(
        immutable_calls,
        [
            (ImmutableCall::Query, written),
            (ImmutableCall::Put, written)
        ]
    );
    assert!(peer.held(&written).is_some(), "the remote holds the blob");
    let mut local = [StoreMatchResult::default()];
    base.immutable
        .clone()
        .query(repository, &[written], &mut local)
        .await
        .expect("the local store answers");
    assert_ne!(local[0].match_made, StoreMatch::MatchNone);
    assert!(
        local[0].stored_durable && !local[0].stored_local,
        "the local entry records the upload without a payload: {:?}",
        local[0]
    );
}

/// Refused arguments fail the call with `InvalidArguments` before it consults any location: no
/// `NoRemote` for the `remote` flag on a handle without a remote, no location complete event, and
/// no request to a remote.
#[tokio::test]
async fn refused_arguments_fail_before_any_location() {
    let (_store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (status, events) = set_with(
        remote_only(),
        handle,
        repository,
        BranchId::default(),
        owner_ci(),
        &[],
    )
    .await;
    close_handle(handle).await;
    assert_eq!(status, InvalidArguments::FFI_CODE);
    let [LoreEvent::Complete(_), LoreEvent::End(_)] = &without_log_lines(&events)[..] else {
        panic!("expected Complete, then End");
    };

    let service = NamingRevision::setting_metadata(MetadataAnswer::NotFound, [SetAnswer::Won]);
    let store =
        store_with_remote_storage(repository, service.clone(), vec![MemoryPeer::new()]).await;
    let handle = handle::register(Arc::new(store));
    let feature = new_branch();
    let (status, events) = set(
        handle,
        repository,
        feature,
        vec![entry(branch::NAME, string("renamed"))],
        &[],
    )
    .await;
    close_handle(handle).await;
    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert!(location_outcomes(&events, CALL_ID).is_empty());
    assert!(service.metadata_gets.lock().unwrap().is_empty());
    assert!(service.metadata_sets.lock().unwrap().is_empty());
}
