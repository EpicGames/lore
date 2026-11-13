// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_graph::branch_resolve::LoreRevisionGraphBranchResolveArgs;
use lore::revision_graph::branch_resolve::branch_resolve;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::BoundFlags;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::BranchNotFound;
use lore_base::error::Disconnected;
use lore_base::error::InvalidArguments;
use lore_base::error::NotConnected;
use lore_base::error::SlowDown;
use lore_base::test_util::TempDir;
use lore_base::test_util::closed_port_url;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_error_set::Internal;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::instance;
use lore_revision::instance::InstanceId;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_revision::protocol::RemoteEndpoint;
use lore_storage::test_util::StoreCall;

use crate::call::test_support::recording_callback;
use crate::call::test_support::take_without_log_lines;
use crate::revision_graph::support::CATEGORY;
use crate::revision_graph::support::CREATED;
use crate::revision_graph::support::NamingRevision;
use crate::revision_graph::support::QueryAnswer;
use crate::revision_graph::support::archive;
use crate::revision_graph::support::branch_records;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::create_in_store;
use crate::revision_graph::support::handle_failing;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::location_outcomes;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::revision_graph::support::register_live_instance;
use crate::revision_graph::support::store_corrupt_metadata;
use crate::revision_graph::support::store_with_remote;
use crate::storage::in_memory_store;

/// The `id` every call in this file passes.
const CALL_ID: u64 = 25;

/// Runs `branch_resolve` on `store` for `name` in `repository` under `globals`, and returns its
/// status and the events it sent other than log lines.
async fn resolve_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    repository: Partition,
    name: &str,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_resolve(
        globals,
        LoreRevisionGraphBranchResolveArgs {
            id: CALL_ID,
            store,
            repository,
            name: LoreString::from(name),
        },
        callback,
    )
    .await;
    (status, take_without_log_lines(&events))
}

/// [`resolve_with`] under default globals.
async fn resolve(store: LoreStore, repository: Partition, name: &str) -> (i32, Vec<LoreEvent>) {
    resolve_with(LoreGlobalArgs::default(), store, repository, name).await
}

/// The branch records of `events` as (location, branch), each echoing the call's id.
fn resolved(events: &[LoreEvent]) -> Vec<(LoreBranchLocation, BranchId)> {
    branch_records(events)
        .into_iter()
        .map(|record| {
            assert_eq!(record.id, CALL_ID);
            (record.location, record.branch)
        })
        .collect()
}

/// Stores `latest` as the latest revision of `branch` in the local store of `store`, with no
/// metadata or name.
async fn store_latest(
    store: &StoreInternal,
    repository: Partition,
    branch: BranchId,
    latest: Hash,
) {
    let write = local_write_context(store, repository);
    in_setup_context(branch::mutable_try_store(
        write,
        branch::LATEST,
        branch,
        Hash::default(),
        latest,
    ))
    .await
    .expect("stores the latest revision");
}

/// A name that is empty, longer than the limit, or reads as a branch id fails the call with
/// `InvalidArguments` before the call consults any location: no record and no location complete
/// event, and no `NoRemote` for the `remote` flag on a handle without a remote.
#[tokio::test]
async fn an_invalid_name_fails_invalid_arguments() {
    let (_store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let too_long = "n".repeat(branch::MAX_NAME_LEN + 1);
    let an_id = new_branch().to_string();
    let remote_only = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };

    for name in ["", too_long.as_str(), an_id.as_str()] {
        for globals in [LoreGlobalArgs::default(), remote_only.clone()] {
            let (status, events) = resolve_with(globals, handle, repository, name).await;

            assert_eq!(status, InvalidArguments::FFI_CODE, "{name:?}");
            let [LoreEvent::Complete(_), LoreEvent::End(_)] = &events[..] else {
                panic!("expected Complete, then End, for {name:?}");
            };
        }
    }
    close_handle(handle).await;
}

/// A name resolves to the branch it names in the local store regardless of case: one local record
/// with the branch as created, then the local store's location complete event, which counts it.
#[tokio::test]
async fn a_local_branch_resolves_case_insensitively() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "Feature", "ana", hash(2)).await;

    let (status, events) = resolve(handle, repository, "feature").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let [
        LoreEvent::RevisionGraphBranch(record),
        LoreEvent::RevisionGraphLocationComplete(_),
        LoreEvent::Complete(_),
        LoreEvent::End(_),
    ] = &events[..]
    else {
        panic!("expected a branch record, its location complete event, Complete and End");
    };
    assert_eq!(
        (record.id, record.location, record.branch),
        (CALL_ID, LoreBranchLocation::Local, feature)
    );
    assert_eq!(record.name.as_bytes(), b"Feature");
    assert_eq!(record.creator.as_bytes(), b"ana");
    assert_eq!((record.latest, record.archived), (hash(2), 0));
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 1, 0)]
    );
}

/// An archived branch's name no longer maps to it, so the name resolves to no branch: the call
/// fails with `BranchNotFound`, a miss at the local store.
#[tokio::test]
async fn an_archived_branch_does_not_resolve() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    archive(&store, repository, feature, "feature").await;

    let (status, events) = resolve(handle, repository, "feature").await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(resolved(&events).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
}

/// The instance records of every instance registered on the branch follow its local record, the
/// live one and one whose path is gone alike, and the location complete event counts all three
/// records. The call retires no registration: both still read afterwards.
#[tokio::test]
async fn instance_records_follow_the_local_record_and_retire_nothing() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    let live_dir = TempDir::new("graph-branch-resolve-live-");
    let gone_dir = TempDir::new("graph-branch-resolve-gone-");
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let live = in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(2),
        &live_dir,
    ))
    .await;
    let gone = in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(3),
        &gone_dir,
    ))
    .await;
    let live_path = lore_revision::util::path::clean(live_dir.path().display().to_string());
    let gone_path = lore_revision::util::path::clean(gone_dir.path().display().to_string());
    drop(gone_dir);

    let (status, events) = resolve(handle, repository, "feature").await;
    let registered = in_setup_context(instance::records_on_branch(
        &local_read_context(&store, repository),
        feature,
    ))
    .await
    .expect("reads the registrations");
    close_handle(handle).await;

    assert_eq!(status, 0);
    let [
        LoreEvent::RevisionGraphBranch(record),
        LoreEvent::RevisionGraphInstance(first),
        LoreEvent::RevisionGraphInstance(second),
        LoreEvent::RevisionGraphLocationComplete(_),
        LoreEvent::Complete(_),
        LoreEvent::End(_),
    ] = &events[..]
    else {
        panic!("expected a branch record, two instance records, the location complete event");
    };
    assert_eq!(
        (record.location, record.branch),
        (LoreBranchLocation::Local, feature)
    );
    let mut instances: Vec<(InstanceId, Vec<u8>, BranchId, Hash, u8)> = [first, second]
        .into_iter()
        .map(|data| {
            assert_eq!(
                (data.id, data.location),
                (CALL_ID, LoreBranchLocation::Local)
            );
            (
                data.instance_id,
                data.path.as_bytes().to_vec(),
                data.branch,
                data.revision,
                data.stale,
            )
        })
        .collect();
    instances.sort_unstable();
    let mut expected = vec![
        (live, live_path.into_bytes(), feature, hash(2), 0),
        (gone, gone_path.into_bytes(), feature, hash(3), 1),
    ];
    expected.sort_unstable();
    assert_eq!(instances, expected);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 3, 0)]
    );
    let mut still_registered: Vec<InstanceId> = registered
        .into_iter()
        .map(|record| record.instance_id)
        .collect();
    still_registered.sort_unstable();
    let mut registered_before = vec![live, gone];
    registered_before.sort_unstable();
    assert_eq!(still_registered, registered_before);
}

/// A name no branch holds fails the call with `BranchNotFound`, a miss at the local store.
#[tokio::test]
async fn a_missing_branch_fails_branch_not_found() {
    let (_store, handle) = open_in_memory_handle().await;

    let (status, events) = resolve(handle, new_repository(), "feature").await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(resolved(&events).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
}

/// Creates, in the local store of `store`, the branch whose id derives from `name`, named so, at a
/// latest revision, and releases its name, as an older version's archive leaves it. Returns the id.
async fn create_unmapped_derived(
    store: &StoreInternal,
    repository: Partition,
    name: &str,
) -> BranchId {
    let derived = branch::fallback_id(name);
    create_in_store(store, repository, derived, name, "ana", hash(2)).await;
    in_setup_context(branch::release_name(
        local_write_context(store, repository),
        name,
        derived,
    ))
    .await
    .expect("releases the name");
    derived
}

/// A name with no mapping resolves to the branch whose id derives from it, where that branch has a
/// latest revision and metadata that names it so. An older version's archive, which releases the
/// name and leaves the metadata as it is, leaves a branch so, and it resolves: the known limit.
#[tokio::test]
async fn a_live_derived_branch_resolves() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let derived = create_unmapped_derived(&store, repository, "legacy").await;

    let (status, events) = resolve(handle, repository, "legacy").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(resolved(&events), [(LoreBranchLocation::Local, derived)]);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 1, 0)]
    );
}

/// The id of a branch no name maps to derives from its name as given, so only that name finds it:
/// another case of the name, whose mapping would be the same, derives another id.
#[tokio::test]
async fn an_unmapped_derived_branch_is_found_only_by_its_name_as_given() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let derived = create_unmapped_derived(&store, repository, "Legacy").await;

    let (status, events) = resolve(handle, repository, "Legacy").await;
    assert_eq!(status, 0);
    assert_eq!(resolved(&events), [(LoreBranchLocation::Local, derived)]);
    let (status, events) = resolve(handle, repository, "legacy").await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
}

/// The id derived from a name names no branch under that name once the branch has left it:
/// archived, or restored under another name, which then resolves to it.
#[tokio::test]
async fn a_derived_branch_that_left_its_name_does_not_resolve() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (archived, restored) = (branch::fallback_id("gone"), branch::fallback_id("legacy"));
    create_in_store(&store, repository, archived, "gone", "ana", hash(2)).await;
    archive(&store, repository, archived, "gone").await;
    create_in_store(&store, repository, restored, "legacy", "ana", hash(3)).await;
    archive(&store, repository, restored, "legacy").await;
    let write = local_write_context(&store, repository);
    in_setup_context(branch::create(
        write.clone(),
        write
            .try_write_token()
            .expect("a write context carries a token"),
        restored,
        "renamed",
        CATEGORY,
        "ana",
        CREATED,
        Vec::new(),
        false,
        false,
    ))
    .await
    .expect("restores the branch under another name");

    for name in ["gone", "legacy"] {
        let (status, events) = resolve(handle, repository, name).await;

        assert_eq!(status, BranchNotFound::FFI_CODE, "{name}");
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, 0, 0)],
            "{name}"
        );
    }
    let (status, events) = resolve(handle, repository, "renamed").await;
    close_handle(handle).await;
    assert_eq!(status, 0);
    assert_eq!(resolved(&events), [(LoreBranchLocation::Local, restored)]);
    assert_eq!(branch_records(&events)[0].name.as_bytes(), b"renamed");
}

/// A failed load of the latest revision or the metadata key of the id derived from a name with no
/// mapping, or metadata of that id that isn't a metadata blob, fails the local store's location
/// with the read's error, rather than reading as no branch.
#[tokio::test]
async fn an_unreadable_derived_branch_fails_the_local_location() {
    let store = in_memory_for_tests("graph-branch-resolve-unreadable-derived").await;
    let repository = new_repository();
    let derived = branch::fallback_id("legacy");
    store_latest(&store, repository, derived, hash(2)).await;
    let salt = local_read_context(&store, repository).salt();

    for function in [branch::LATEST, branch::METADATA] {
        let failing = branch::mutable_key(salt, function, repository, derived).0;
        let handle = handle_failing(&store, move |call, key, _| {
            call == StoreCall::Load && key == failing
        });

        let (status, events) = resolve(handle, repository, "legacy").await;
        close_handle(handle).await;

        assert_eq!(
            status,
            Disconnected::FFI_CODE,
            "a failed load of {function}"
        );
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, 0, Disconnected::FFI_CODE)],
            "a failed load of {function}"
        );
    }
    in_setup_context(store_corrupt_metadata(
        &local_write_context(&store, repository),
        derived,
    ))
    .await;
    let handle = handle::register(store.clone());
    let (status, events) = resolve(handle, repository, "legacy").await;
    close_handle(handle).await;
    assert_eq!(status, Internal::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, Internal::FFI_CODE)]
    );
}

/// A branch whose metadata the local store doesn't hold has no record, so its name resolves to no
/// branch there: a name mapped to a branch without a metadata key, as a create whose claim landed
/// but reported a failure leaves it, or with one naming a blob the store doesn't hold, and a name
/// with no mapping whose derived id has a latest revision and the same.
#[tokio::test]
async fn a_branch_without_local_metadata_has_no_record() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    let (keyless, blobless) = (new_branch(), new_branch());
    in_setup_context(async {
        for (name, mapped) in [("keyless", keyless), ("blobless", blobless)] {
            branch::claim_name(write.clone(), name, Context::default(), mapped)
                .await
                .expect("maps the name");
        }
        branch::mutable_store_metadata(write.clone(), blobless, hash(9))
            .await
            .expect("stores the metadata key");
        branch::mutable_store_metadata(
            write.clone(),
            branch::fallback_id("derived-blobless"),
            hash(9),
        )
        .await
        .expect("stores the metadata key");
    })
    .await;
    for name in ["derived-keyless", "derived-blobless"] {
        store_latest(&store, repository, branch::fallback_id(name), hash(2)).await;
    }

    for name in ["keyless", "blobless", "derived-keyless", "derived-blobless"] {
        let (status, events) = resolve(handle, repository, name).await;

        assert_eq!(status, BranchNotFound::FFI_CODE, "{name}");
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, 0, 0)],
            "{name}"
        );
    }
    close_handle(handle).await;
}

/// A failed read in the local store fails its location with the read's error, rather than reading
/// as no branch, and sends no record: a load of the name's mapping, or of the branch's metadata
/// key, latest or latest status, or the list of the registered instances.
#[tokio::test]
async fn a_failed_local_read_fails_the_local_location() {
    let store = in_memory_for_tests("graph-branch-resolve-failed-read").await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let salt = local_read_context(&store, repository).salt();
    let key = |function| Some(branch::mutable_key(salt, function, repository, feature).0);

    for (read, failing_call, failing_type, failing_key) in [
        (
            "the name's mapping",
            StoreCall::Load,
            KeyType::BranchId,
            None,
        ),
        (
            "the metadata key",
            StoreCall::Load,
            KeyType::BranchMetadata,
            key(branch::METADATA),
        ),
        (
            "the latest",
            StoreCall::Load,
            KeyType::BranchLatestPointer,
            key(branch::LATEST),
        ),
        (
            "the latest status",
            StoreCall::Load,
            KeyType::Untyped,
            key(branch::LATEST_STATUS),
        ),
        ("the instances", StoreCall::List, KeyType::Instance, None),
    ] {
        let handle = handle_failing(&store, move |call, key, key_type| {
            call == failing_call
                && key_type == failing_type
                && failing_key.is_none_or(|failing| failing == key)
        });

        let (status, events) = resolve(handle, repository, "feature").await;
        close_handle(handle).await;

        assert_eq!(status, Disconnected::FFI_CODE, "a failed read of {read}");
        assert!(resolved(&events).is_empty(), "a failed read of {read}");
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [(LoreBranchLocation::Local, 0, Disconnected::FFI_CODE)],
            "a failed read of {read}"
        );
    }
}

/// The remote's answer that no live branch holds the name is a miss there: its location complete
/// event comes first, with no error and no record, and the local store's record follows. The
/// remote is asked by the name as given, alone.
#[tokio::test]
async fn a_branch_the_remote_lacks_still_resolves_locally() {
    let repository = new_repository();
    let service = NamingRevision::new(QueryAnswer::NotFound);
    let store = store_with_remote(repository, service.clone()).await;
    let feature = new_branch();
    create_in_store(&store, repository, feature, "Feature", "ana", hash(2)).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) = resolve(handle, repository, "feature").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(resolved(&events), [(LoreBranchLocation::Local, feature)]);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 1, 0)
        ]
    );
    assert_eq!(
        *service.queries.lock().unwrap(),
        [(None, Some("feature".to_owned()))]
    );
}

/// A failure at the remote, a refused query or a connect that fails, fails the call with the
/// remote's error though the local store's record follows: the remote's location complete event
/// carries the error and no record.
#[tokio::test]
async fn a_failure_at_the_remote_fails_the_call_after_the_local_record() {
    let repository = new_repository();
    let refusing = store_with_remote(repository, NamingRevision::new(QueryAnswer::SlowDown)).await;
    let unreachable = in_memory_store(
        Some(Arc::new(RemoteEndpoint::new(closed_port_url("grpc"), None))),
        BoundFlags::default(),
    )
    .await;

    for (store, code) in [
        (refusing, SlowDown::FFI_CODE),
        (unreachable, Disconnected::FFI_CODE),
    ] {
        let feature = new_branch();
        create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
        let handle = handle::register(Arc::new(store));

        let (status, events) = resolve(handle, repository, "feature").await;
        close_handle(handle).await;

        assert_eq!(status, code);
        assert_eq!(resolved(&events), [(LoreBranchLocation::Local, feature)]);
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [
                (LoreBranchLocation::Remote, 0, code),
                (LoreBranchLocation::Local, 1, 0)
            ]
        );
    }
}

/// Where both locations fail, the call fails with the remote's error, and each location's complete
/// event carries its own.
#[tokio::test]
async fn when_both_locations_fail_the_call_carries_the_remotes_error() {
    let repository = new_repository();
    let store = store_with_remote(repository, NamingRevision::new(QueryAnswer::SlowDown)).await;
    create_in_store(&store, repository, new_branch(), "feature", "ana", hash(2)).await;
    let handle = handle_failing(&store, |call, _, key_type| {
        call == StoreCall::Load && key_type == KeyType::BranchId
    });

    let (status, events) = resolve(handle, repository, "feature").await;
    close_handle(handle).await;

    assert_eq!(status, SlowDown::FFI_CODE);
    assert!(resolved(&events).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, SlowDown::FFI_CODE),
            (LoreBranchLocation::Local, 0, Disconnected::FFI_CODE)
        ]
    );
}

/// A name neither location holds a branch under fails the call with `BranchNotFound`, a miss at
/// each location.
#[tokio::test]
async fn a_branch_no_location_holds_fails_branch_not_found() {
    let repository = new_repository();
    let store = store_with_remote(repository, NamingRevision::new(QueryAnswer::NotFound)).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) = resolve(handle, repository, "feature").await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 0, 0)
        ]
    );
}

/// With the `remote` flag the call consults the remote alone, though the local store holds a
/// branch under the name.
#[tokio::test]
async fn the_remote_flag_consults_the_remote_alone() {
    let repository = new_repository();
    let store = store_with_remote(repository, NamingRevision::new(QueryAnswer::NotFound)).await;
    create_in_store(&store, repository, new_branch(), "feature", "ana", hash(2)).await;
    let handle = handle::register(Arc::new(store));
    let remote_only = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };

    let (status, events) = resolve_with(remote_only, handle, repository, "feature").await;
    close_handle(handle).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, 0)]
    );
}

/// A remote record whose metadata blob the remote can't serve fails the remote's location rather
/// than reporting the branch unprotected: the remote answers the query, and the blob read over a
/// connection with no storage fails `NotConnected`, which the complete event and the call carry.
#[tokio::test]
async fn a_remote_record_whose_metadata_cannot_be_read_fails_the_remote_location() {
    let repository = new_repository();
    let service = NamingRevision::new(QueryAnswer::Record(new_branch()));
    let store = store_with_remote(repository, service.clone()).await;
    let handle = handle::register(Arc::new(store));
    let remote_only = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };

    let (status, events) = resolve_with(remote_only, handle, repository, "feature").await;
    close_handle(handle).await;

    assert_eq!(
        *service.queries.lock().unwrap(),
        [(None, Some("feature".to_owned()))]
    );
    assert!(resolved(&events).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Remote, 0, NotConnected::FFI_CODE)]
    );
    assert_eq!(status, NotConnected::FFI_CODE);
}
