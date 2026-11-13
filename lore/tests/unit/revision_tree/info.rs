// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::info::*;
use lore_base::error::InvalidArguments;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeInfoEventData;
use lore_revision::metadata::CREATED_BY;
use lore_revision::metadata::MESSAGE;
use lore_revision::metadata::Metadata;

use crate::call::test_support::complete_statuses;
use crate::call::test_support::recording_callback;
use crate::call::test_support::take_recorded;
use crate::revision_tree::support::handle_state;
use crate::revision_tree::support::load_handle;
use crate::revision_tree::support::release;

fn info_event(events: &[LoreEvent]) -> Option<&LoreRevisionTreeInfoEventData> {
    events.iter().find_map(|event| match event {
        LoreEvent::RevisionTreeInfo(data) => Some(data),
        _ => None,
    })
}

#[tokio::test]
async fn info_returns_parents_timestamp_author_and_key_count() {
    let partition = Partition::from([0x22u8; 16]);
    let (handle, store_handle_id) = load_handle("info-full", partition).await;
    let (state, repository_context) = handle_state(handle);

    let mut metadata = Metadata::new();
    metadata
        .set_timestamp(1_700_000_000)
        .expect("set timestamp");
    metadata
        .set_string(CREATED_BY, "alice")
        .expect("set author");
    metadata
        .set_string(MESSAGE, "initial")
        .expect("set message");
    let metadata_hash = metadata
        .serialize(repository_context.clone())
        .await
        .expect("serialize metadata");
    state.set_metadata_hash(metadata_hash);
    let parent_self = Hash::from([0x01u8; 32]);
    let parent_other = Hash::from([0x02u8; 32]);
    state.set_parent_self(parent_self);
    state.set_parent_other(parent_other);

    let (sink, callback) = recording_callback();
    let status = info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeInfoArgs { id: 1, handle },
        callback,
    )
    .await;

    assert_eq!(status, 0);
    let events = take_recorded(&sink);
    let data = info_event(&events).expect("info event must fire");
    assert_eq!(data.id, 1);
    assert_eq!(data.error_code, LoreErrorCode::None);
    assert_eq!(data.repository, partition, "got {events:?}");
    assert_eq!(
        data.revision,
        Hash::default(),
        "info reports the handle's loaded revision, got {events:?}"
    );
    assert_eq!(
        data.parent,
        [parent_self, parent_other],
        "info must carry the parent revision signatures, got {events:?}"
    );
    assert_eq!(data.creation_timestamp, 1_700_000_000, "got {events:?}");
    assert_eq!(data.author_identity.as_str(), "alice", "got {events:?}");
    assert_eq!(
        data.metadata_key_count, 3,
        "timestamp + created-by + message = 3 keys, got {events:?}"
    );
    assert!(complete_statuses(&events).contains(&0));

    release(handle, store_handle_id);
}

#[tokio::test]
async fn info_without_metadata_reports_zeroed_fields() {
    let partition = Partition::from([0x33u8; 16]);
    let (handle, store_handle_id) = load_handle("info-empty", partition).await;

    let (sink, callback) = recording_callback();
    let status = info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeInfoArgs { id: 2, handle },
        callback,
    )
    .await;

    assert_eq!(status, 0);
    let events = take_recorded(&sink);
    let data = info_event(&events).expect("info event must fire");
    assert_eq!(data.error_code, LoreErrorCode::None);
    assert_eq!(data.repository, partition, "got {events:?}");
    assert_eq!(data.metadata_key_count, 0, "got {events:?}");
    assert_eq!(data.creation_timestamp, 0, "got {events:?}");
    assert_eq!(data.author_identity.as_str(), "", "got {events:?}");
    assert_eq!(
        data.parent,
        [Hash::default(), Hash::default()],
        "an uncommitted handle has no parent signatures, got {events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn info_with_unreadable_metadata_returns_internal() {
    let (handle, store_handle_id) =
        load_handle("info-corrupt", Partition::from([0x44u8; 16])).await;
    let (state, _repository_context) = handle_state(handle);
    state.set_metadata_hash(Hash::from([0x7Eu8; 32]));

    let (sink, callback) = recording_callback();
    let status = info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeInfoArgs { id: 3, handle },
        callback,
    )
    .await;

    assert_ne!(status, 0, "an unreadable metadata fragment must fail");
    let events = take_recorded(&sink);
    let data =
        info_event(&events).expect("a failure must still emit the info terminal carrying the id");
    assert_eq!(data.id, 3);
    assert_eq!(
        data.error_code,
        LoreErrorCode::Internal,
        "a present-but-unreadable metadata fragment must report Internal, got {events:?}"
    );
    assert!(
        complete_statuses(&events).contains(&status),
        "Complete must carry the failure status, got {events:?}"
    );

    release(handle, store_handle_id);
}

#[tokio::test]
async fn info_on_unknown_handle_emits_terminal_with_invalid_arguments() {
    let (sink, callback) = recording_callback();

    let status = info(
        LoreGlobalArgs::default(),
        LoreRevisionTreeInfoArgs {
            id: 4,
            handle: LoreRevisionTree::INVALID,
        },
        callback,
    )
    .await;

    assert_eq!(
        status,
        InvalidArguments::FFI_CODE,
        "an unknown handle must fail"
    );
    let events = take_recorded(&sink);
    let data = info_event(&events)
        .expect("a handle miss must still emit the info terminal carrying the id");
    assert_eq!(data.id, 4);
    assert_eq!(
        data.error_code,
        LoreErrorCode::InvalidArguments,
        "got {events:?}"
    );
    assert!(complete_statuses(&events).contains(&InvalidArguments::FFI_CODE));
}
