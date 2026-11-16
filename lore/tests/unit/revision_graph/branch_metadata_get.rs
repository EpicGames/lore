// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_graph::branch_metadata_get::LoreRevisionGraphBranchMetadataGetArgs;
use lore::revision_graph::branch_metadata_get::branch_metadata_get;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::AddressNotFound;
use lore_base::error::BranchNotFound;
use lore_base::error::Disconnected;
use lore_base::error::InvalidArguments;
use lore_base::error::Oversized;
use lore_base::error::SlowDown;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::Internal;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::METADATA_MAX_SIZE;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_storage::StoreError;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::StoreCall;
use lore_transport::ProtocolError;
use lore_transport::test_util::MemoryPeer;

use crate::call::test_support::recording_callback;
use crate::call::test_support::without_log_lines;
use crate::revision_graph::support::MetadataAnswer;
use crate::revision_graph::support::NamingRevision;
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
use crate::revision_graph::support::put_root_fragment;
use crate::revision_graph::support::reported;
use crate::revision_graph::support::store_bytes_as_metadata;
use crate::revision_graph::support::store_corrupt_metadata;
use crate::revision_graph::support::store_metadata;
use crate::revision_graph::support::store_with_remote;
use crate::revision_graph::support::store_with_remote_storage;
use crate::revision_graph::support::with_stores;

/// The `id` every call in this file passes.
const CALL_ID: u64 = 64;

/// Runs `branch_metadata_get` on `store` for `branch` in `repository` under `globals`, for the
/// entry `key` names or, with an empty `key`, every entry, and returns its status and every event
/// it sent, log lines included.
async fn get_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    key: &str,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_metadata_get(
        globals,
        LoreRevisionGraphBranchMetadataGetArgs {
            id: CALL_ID,
            store,
            repository,
            branch,
            key: LoreString::from(key),
        },
        callback,
    )
    .await;
    let events = std::mem::take(&mut *events.lock().unwrap());
    (status, events)
}

/// [`get_with`] under default globals, for every entry.
async fn get(store: LoreStore, repository: Partition, branch: BranchId) -> (i32, Vec<LoreEvent>) {
    get_with(LoreGlobalArgs::default(), store, repository, branch, "").await
}

/// The location of each record and location complete event among `events`, in order, `true`
/// marking a location complete event.
fn sequence(events: &[LoreEvent]) -> Vec<(LoreBranchLocation, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranchMetadata(record) => Some((record.location, false)),
            LoreEvent::RevisionGraphLocationComplete(complete) => Some((complete.location, true)),
            _ => None,
        })
        .collect()
}

/// The sequence of `records` records at `location`, then its location complete event.
fn reported_at(location: LoreBranchLocation, records: usize) -> Vec<(LoreBranchLocation, bool)> {
    let mut sequence = vec![(location, false); records];
    sequence.push((location, true));
    sequence
}

/// Sets the string `entries` in the metadata of `branch` in the local store of `store`, and
/// returns the metadata the branch then holds.
async fn set_local_entries(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    entries: &[(&str, &str)],
) -> Metadata {
    let mut metadata = local_metadata(store, repository, branch).await;
    for (key, value) in entries {
        metadata.set_string(key, value).expect("sets an entry");
    }
    in_setup_context(store_metadata(
        &local_write_context(store, repository),
        branch,
        metadata.clone(),
    ))
    .await;
    metadata
}

/// Every entry of a branch's metadata in the local store is one record, built-in entries
/// included, in stored order, each carrying the call's id, the location and the branch. The
/// location complete event follows them and counts them.
#[tokio::test]
async fn every_entry_of_a_local_branch_is_one_record() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let metadata = set_local_entries(&store, repository, feature, &[("owner", "ci")]).await;

    let (status, events) = get(handle, repository, feature).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let records = reported(&events, CALL_ID, feature);
    assert_eq!(records, entries_at(LoreBranchLocation::Local, &metadata));
    for (key, value) in [(branch::NAME, "feature"), ("owner", "ci")] {
        let entry = (
            LoreBranchLocation::Local,
            key.to_owned(),
            LoreMetadata::String(LoreString::from(value)),
        );
        assert!(records.contains(&entry), "reports {key}");
    }
    assert_eq!(
        sequence(&events),
        reported_at(LoreBranchLocation::Local, records.len())
    );
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, records.len() as u64, 0)]
    );
    let [.., LoreEvent::Complete(_), LoreEvent::End(_)] = &without_log_lines(&events)[..] else {
        panic!("expected Complete, then End, last");
    };
}

/// A key reports that entry alone, in the local store and at the remote alike, and a key the
/// metadata lacks reports none: the call still succeeds, since a location holds the branch.
#[tokio::test]
async fn a_key_reports_that_entry_or_none() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;

    let (status, events) = get_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        feature,
        branch::NAME,
    )
    .await;
    assert_eq!(status, 0);
    assert_eq!(
        reported(&events, CALL_ID, feature),
        [(
            LoreBranchLocation::Local,
            branch::NAME.to_owned(),
            LoreMetadata::String(LoreString::from("feature"))
        )]
    );
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 1, 0)]
    );

    let (status, events) = get_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        feature,
        "absent",
    )
    .await;
    close_handle(handle).await;
    assert_eq!(status, 0);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );

    let peer = MemoryPeer::new();
    let remote_metadata =
        partial_metadata(Some(feature), &[(branch::NAME, "feature"), ("owner", "ci")]);
    let blob = put_metadata_blob(&peer, &remote_metadata).await;
    let service = NamingRevision::serving_metadata(MetadataAnswer::Hash(blob));
    let handle = handle::register(Arc::new(
        store_with_remote_storage(repository, service, vec![peer]).await,
    ));

    let (status, events) = get_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        feature,
        "owner",
    )
    .await;
    assert_eq!(status, 0);
    assert_eq!(
        reported(&events, CALL_ID, feature),
        [(
            LoreBranchLocation::Remote,
            "owner".to_owned(),
            LoreMetadata::String(LoreString::from("ci"))
        )]
    );
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 0, 0)
        ]
    );

    let (status, events) = get_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        feature,
        "absent",
    )
    .await;
    close_handle(handle).await;
    assert_eq!(status, 0);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 0, 0)
        ]
    );
}

/// A zero branch fails the call with `InvalidArguments` before it consults any location: no
/// record, no location complete event, no `NoRemote` for the `remote` flag on a handle without a
/// remote, and no request to a remote.
#[tokio::test]
async fn a_zero_branch_fails_invalid_arguments_before_any_read() {
    let (_store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let remote_only = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };
    for globals in [LoreGlobalArgs::default(), remote_only] {
        let (status, events) = get_with(globals, handle, repository, BranchId::default(), "").await;

        assert_eq!(status, InvalidArguments::FFI_CODE);
        let [LoreEvent::Complete(_), LoreEvent::End(_)] = &without_log_lines(&events)[..] else {
            panic!("expected Complete, then End");
        };
    }
    close_handle(handle).await;

    let service = NamingRevision::serving_metadata(MetadataAnswer::NotFound);
    let handle = handle::register(Arc::new(
        store_with_remote(repository, service.clone()).await,
    ));
    let (status, events) = get(handle, repository, BranchId::default()).await;
    close_handle(handle).await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert!(location_outcomes(&events, CALL_ID).is_empty());
    assert!(service.metadata_gets.lock().unwrap().is_empty());
}

/// A branch neither location holds fails the call with `BranchNotFound`, a miss at each location:
/// the remote answers `NotFound`, or a zero metadata hash, and the local store has no metadata key.
#[tokio::test]
async fn a_branch_no_location_holds_fails_branch_not_found() {
    let repository = new_repository();

    for (answered, answer) in [
        ("NotFound", MetadataAnswer::NotFound),
        ("a zero hash", MetadataAnswer::Hash(Hash::default())),
    ] {
        let service = NamingRevision::serving_metadata(answer);
        let handle = handle::register(Arc::new(
            store_with_remote(repository, service.clone()).await,
        ));
        let feature = new_branch();

        let (status, events) = get(handle, repository, feature).await;
        close_handle(handle).await;

        assert_eq!(
            status,
            BranchNotFound::FFI_CODE,
            "the remote answers {answered}"
        );
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [
                (LoreBranchLocation::Remote, 0, 0),
                (LoreBranchLocation::Local, 0, 0)
            ],
            "the remote answers {answered}"
        );
        assert_eq!(*service.metadata_gets.lock().unwrap(), [feature]);
    }
}

/// A metadata key naming a blob the local store doesn't hold is no metadata there, a miss rather
/// than a failure.
#[tokio::test]
async fn a_metadata_key_whose_blob_the_store_lacks_is_a_miss() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    in_setup_context(branch::mutable_store_metadata(
        local_write_context(&store, repository),
        feature,
        hash(9),
    ))
    .await
    .expect("stores the metadata key");

    let (status, events) = get(handle, repository, feature).await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
}

/// A failed read in the local store fails its location with the read's error rather than reading
/// as no metadata, and sends no record: a load of the metadata key, a read of the blob it names,
/// and a blob that isn't metadata.
#[tokio::test]
async fn a_failed_local_read_fails_the_local_location() {
    let store = in_memory_for_tests("graph-branch-metadata-get-failed-read").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let read = local_read_context(&store, repository);
    let key = branch::mutable_key(read.salt(), branch::METADATA, repository, feature).0;
    let blob = in_setup_context(branch::local_metadata(read, feature))
        .await
        .expect("reads the metadata")
        .expect("the branch has metadata")
        .0;
    let failing_load = handle_failing(&store, move |call, loaded, _| {
        call == StoreCall::Load && loaded == key
    });
    let failing_blob = HookedImmutableStore::wrap(store.immutable.clone(), move |_, _, address| {
        if address.hash == blob {
            return Err(StoreError::from(Disconnected));
        }
        Ok(())
    });
    let failing_blob = handle::register(Arc::new(with_stores(
        &store,
        failing_blob,
        store.mutable.clone(),
    )));

    for (failed, handle) in [
        ("the metadata key", failing_load),
        ("the blob", failing_blob),
    ] {
        let (status, events) = get(handle, repository, feature).await;
        close_handle(handle).await;

        assert_eq!(status, Disconnected::FFI_CODE, "a failed read of {failed}");
        assert!(
            reported(&events, CALL_ID, feature).is_empty(),
            "a failed read of {failed}"
        );
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, 0, Disconnected::FFI_CODE)],
            "a failed read of {failed}"
        );
    }

    let corrupt = new_branch();
    in_setup_context(store_corrupt_metadata(
        &local_write_context(&store, repository),
        corrupt,
    ))
    .await;
    let handle = handle::register(store.clone());
    let (status, events) = get(handle, repository, corrupt).await;
    close_handle(handle).await;
    assert_eq!(status, Internal::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, Internal::FFI_CODE)]
    );
}

/// A metadata blob larger than 1 MiB fails each location with `Oversized`, which the read finds
/// from the blob's root fragment alone, and the call with the remote's.
#[tokio::test]
async fn a_blob_over_one_mebibyte_fails_oversized_at_each_location() {
    let repository = new_repository();
    let feature = new_branch();
    let bytes = vec![7; METADATA_MAX_SIZE + 1];
    let scratch = in_memory_for_tests("graph-branch-metadata-get-oversized").await;
    let blob = in_setup_context(store_bytes_as_metadata(
        &local_write_context(&scratch, repository),
        feature,
        &bytes,
    ))
    .await;
    let peer = MemoryPeer::new();
    put_root_fragment(&peer, &scratch, repository, blob).await;
    let service = NamingRevision::serving_metadata(MetadataAnswer::Hash(blob.hash));
    let store = store_with_remote_storage(repository, service, vec![peer]).await;
    in_setup_context(store_bytes_as_metadata(
        &local_write_context(&store, repository),
        feature,
        &bytes,
    ))
    .await;
    let handle = handle::register(Arc::new(store));

    let (status, events) = get(handle, repository, feature).await;
    close_handle(handle).await;

    assert_eq!(status, Oversized::FFI_CODE);
    assert!(reported(&events, CALL_ID, feature).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, Oversized::FFI_CODE),
            (LoreBranchLocation::Local, 0, Oversized::FFI_CODE)
        ]
    );
}

/// An archived branch reads like a live one: every entry of the metadata its archive left is one
/// record, its `archived` entry included.
#[tokio::test]
async fn an_archived_branch_reads_like_a_live_one() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    archive(&store, repository, feature, "feature").await;
    let metadata = local_metadata(&store, repository, feature).await;

    let (status, events) = get(handle, repository, feature).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let records = reported(&events, CALL_ID, feature);
    assert_eq!(records, entries_at(LoreBranchLocation::Local, &metadata));
    assert!(records.iter().any(|(_, key, _)| key == branch::ARCHIVED));
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, records.len() as u64, 0)]
    );
}

/// An entry no record can carry, a hash value one byte short, is left out of each location's
/// records while the entries around it are reported, and the location complete events count only
/// those reported.
#[tokio::test]
async fn skipped_entries_are_left_out_at_each_location() {
    let repository = new_repository();
    let feature = new_branch();
    let mut metadata = partial_metadata(Some(feature), &[(branch::NAME, "feature")]);
    metadata
        .set_typed("short", &[1; 31], MetadataType::Hash)
        .expect("sets the entry");
    metadata.set_string("owner", "ci").expect("sets the entry");
    let readable = partial_metadata(Some(feature), &[(branch::NAME, "feature"), ("owner", "ci")]);
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &metadata).await;
    let service = NamingRevision::serving_metadata(MetadataAnswer::Hash(blob));
    let store = store_with_remote_storage(repository, service, vec![peer]).await;
    in_setup_context(store_metadata(
        &local_write_context(&store, repository),
        feature,
        metadata,
    ))
    .await;
    let handle = handle::register(Arc::new(store));

    let (status, events) = get(handle, repository, feature).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let (remote, local) = (
        entries_at(LoreBranchLocation::Remote, &readable),
        entries_at(LoreBranchLocation::Local, &readable),
    );
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
}

/// The remote's records and its location complete event come before the local store's. A failure
/// at the remote, a refused request or a blob the remote lacks, fails the call with the remote's
/// error though the local store's records follow: the remote's location complete event carries
/// the error and no record.
#[tokio::test]
async fn the_remote_record_comes_first_and_a_remote_failure_fails_the_call() {
    let repository = new_repository();
    let feature = new_branch();
    let remote_metadata = partial_metadata(
        Some(feature),
        &[(branch::NAME, "feature"), ("owner", "remote")],
    );
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata).await;

    for (answered, answer, code) in [
        ("its metadata", MetadataAnswer::Hash(blob), 0),
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
        let service = NamingRevision::serving_metadata(answer);
        let store = store_with_remote_storage(repository, service, vec![peer.clone()]).await;
        create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
        let local = entries_at(
            LoreBranchLocation::Local,
            &local_metadata(&store, repository, feature).await,
        );
        let handle = handle::register(Arc::new(store));

        let (status, events) = get(handle, repository, feature).await;
        close_handle(handle).await;

        let remote = match code {
            0 => entries_at(LoreBranchLocation::Remote, &remote_metadata),
            _ => Vec::new(),
        };
        assert_eq!(status, code, "the remote answers {answered}");
        assert_eq!(
            reported(&events, CALL_ID, feature),
            [remote.clone(), local.clone()].concat(),
            "the remote answers {answered}"
        );
        assert_eq!(
            sequence(&events),
            [
                reported_at(LoreBranchLocation::Remote, remote.len()),
                reported_at(LoreBranchLocation::Local, local.len())
            ]
            .concat(),
            "the remote answers {answered}"
        );
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [
                (LoreBranchLocation::Remote, remote.len() as u64, code),
                (LoreBranchLocation::Local, local.len() as u64, 0)
            ],
            "the remote answers {answered}"
        );
    }
}

/// With the `local` or the `offline` flag the call consults the local store alone, and asks the
/// remote nothing.
#[tokio::test]
async fn the_local_flag_consults_the_local_store_alone() {
    let repository = new_repository();
    let service = NamingRevision::serving_metadata(MetadataAnswer::NotFound);
    let store = store_with_remote(repository, service.clone()).await;
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let local = entries_at(
        LoreBranchLocation::Local,
        &local_metadata(&store, repository, feature).await,
    );
    let handle = handle::register(Arc::new(store));

    for (flag, globals) in [
        (
            "local",
            LoreGlobalArgs {
                local: 1,
                ..Default::default()
            },
        ),
        (
            "offline",
            LoreGlobalArgs {
                offline: 1,
                ..Default::default()
            },
        ),
    ] {
        let (status, events) = get_with(globals, handle, repository, feature, "").await;

        assert_eq!(status, 0, "{flag}");
        assert_eq!(reported(&events, CALL_ID, feature), local, "{flag}");
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, local.len() as u64, 0)],
            "{flag}"
        );
    }
    close_handle(handle).await;
    assert!(service.metadata_gets.lock().unwrap().is_empty());
}

/// With the `remote` flag the call consults the remote alone: the remote's records and its
/// location complete event, and none for the local store.
#[tokio::test]
async fn the_remote_flag_consults_the_remote_alone() {
    let repository = new_repository();
    let feature = new_branch();
    let remote_metadata = partial_metadata(
        Some(feature),
        &[(branch::NAME, "feature"), ("owner", "remote")],
    );
    let peer = MemoryPeer::new();
    let blob = put_metadata_blob(&peer, &remote_metadata).await;
    let service = NamingRevision::serving_metadata(MetadataAnswer::Hash(blob));
    let handle = handle::register(Arc::new(
        store_with_remote_storage(repository, service, vec![peer]).await,
    ));
    let remote_only = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };

    let (status, events) = get_with(remote_only, handle, repository, feature, "").await;
    close_handle(handle).await;

    let remote = entries_at(LoreBranchLocation::Remote, &remote_metadata);
    assert_eq!(status, 0);
    assert_eq!(reported(&events, CALL_ID, feature), remote);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, remote.len() as u64, 0)]
    );
}
