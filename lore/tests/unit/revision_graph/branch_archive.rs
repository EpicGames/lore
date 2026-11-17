// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::branch_archive::LoreRevisionGraphBranchArchiveArgs;
use lore::revision_graph::branch_archive::branch_archive;
use lore::revision_graph::branch_list::LoreRevisionGraphBranchListArgs;
use lore::revision_graph::branch_list::branch_list;
use lore::revision_graph::branch_resolve::LoreRevisionGraphBranchResolveArgs;
use lore::revision_graph::branch_resolve::branch_resolve;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::revision_graph::revision_resolve::LoreRevisionGraphRevisionResolveArgs;
use lore::revision_graph::revision_resolve::revision_resolve;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::StoreInternal;
use lore_base::error::BranchNotFound;
use lore_base::error::DeleteCurrent;
use lore_base::error::DeleteDefault;
use lore_base::error::DeleteProtected;
use lore_base::error::InvalidArguments;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Partition;
use lore_error_set::Internal;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_storage::StoreError;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::ImmutableCall;

use crate::call::test_support::recording_callback;
use crate::call::test_support::take_without_log_lines;
use crate::revision_graph::support::archive;
use crate::revision_graph::support::branch_records;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::create;
use crate::revision_graph::support::create_in_store;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::local_metadata;
use crate::revision_graph::support::location_outcomes;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::revision_graph::support::register_live_instance;
use crate::revision_graph::support::store_metadata;
use crate::revision_graph::support::store_repository_metadata;
use crate::revision_graph::support::with_stores;

/// The `id` every call in this file passes.
const CALL_ID: u64 = 37;

/// Runs `branch_archive` of `branch` on `store` in `repository` under `globals`, and returns its
/// status and the events it sent other than log lines.
async fn archive_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_archive(
        globals,
        LoreRevisionGraphBranchArchiveArgs {
            id: CALL_ID,
            store,
            repository,
            branch,
        },
        callback,
    )
    .await;
    (status, take_without_log_lines(&events))
}

/// [`archive_with`] under default globals.
async fn archive_branch(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
) -> (i32, Vec<LoreEvent>) {
    archive_with(LoreGlobalArgs::default(), store, repository, branch).await
}

/// The branch the local store maps `name` to, read without writing.
async fn mapped(store: &StoreInternal, repository: Partition, name: &str) -> Option<BranchId> {
    in_setup_context(branch::lookup_name_local_unwritten(
        local_read_context(store, repository),
        name,
    ))
    .await
    .expect("looks up the name")
    .map(|named| named.branch())
}

/// The status of the call `call` makes with the callback it is passed.
async fn status_of<F: Future<Output = i32>>(
    call: impl FnOnce(lore::interface::LoreEventCallback) -> F,
) -> i32 {
    let (_, callback) = recording_callback();
    call(callback).await
}

/// Asserts that `events` holds the archived local record of `branch` alone, followed by the local
/// store's location complete event with one record.
fn assert_archived_locally(events: &[LoreEvent], branch: BranchId) {
    let [record] = branch_records(events)[..] else {
        panic!("expected one branch record");
    };
    assert_eq!(
        (record.id, record.location, record.branch, record.archived),
        (CALL_ID, LoreBranchLocation::Local, branch, 1)
    );
    assert_eq!(
        location_outcomes(events, CALL_ID),
        [(LoreBranchLocation::Local, 1, 0)]
    );
}

/// Asserts that `events` holds only the call's terminal events.
fn assert_terminal_only(events: &[LoreEvent]) {
    let [LoreEvent::Complete(_), LoreEvent::End(_)] = events else {
        panic!("expected Complete, then End");
    };
}

/// An archive releases the branch's name and keeps its metadata and latest: the name resolves no
/// branch, the branch's latest still resolves by its id, and `include_archived` lists it.
#[tokio::test]
async fn archiving_a_local_branch_removes_the_name_and_keeps_metadata() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, 0);
    assert_archived_locally(&events, feature);
    assert_eq!(mapped(&store, repository, "feature").await, None);
    let resolved = status_of(|callback| {
        branch_resolve(
            LoreGlobalArgs::default(),
            LoreRevisionGraphBranchResolveArgs {
                id: CALL_ID,
                store: handle,
                repository,
                name: LoreString::from("feature"),
            },
            callback,
        )
    })
    .await;
    assert_eq!(resolved, BranchNotFound::FFI_CODE);
    let latest = in_setup_context(branch::mutable_value(
        local_read_context(&store, repository),
        branch::LATEST,
        feature,
    ))
    .await
    .expect("loads the latest");
    assert_eq!(latest, Some(hash(2)));
    let resolved_by_id = status_of(|callback| {
        revision_resolve(
            LoreGlobalArgs::default(),
            LoreRevisionGraphRevisionResolveArgs {
                id: CALL_ID,
                store: handle,
                repository,
                revision: LoreRevisionIdentifier {
                    branch: feature,
                    ..Default::default()
                },
            },
            callback,
        )
    })
    .await;
    assert_ne!(
        resolved_by_id,
        BranchNotFound::FFI_CODE,
        "the branch is still found by id"
    );
    let (events, callback) = recording_callback();
    let listed = branch_list(
        LoreGlobalArgs::default(),
        LoreRevisionGraphBranchListArgs {
            id: CALL_ID,
            store: handle,
            repository,
            include_archived: 1,
            ..Default::default()
        },
        callback,
    )
    .await;
    assert_eq!(listed, 0);
    let listed = take_without_log_lines(&events);
    assert!(
        branch_records(&listed)
            .iter()
            .any(|record| record.branch == feature && record.archived == 1)
    );
    close_handle(handle).await;
}

/// A zero branch id fails with `InvalidArguments` before any read.
#[tokio::test]
async fn a_zero_branch_fails_invalid_arguments() {
    let (_store, handle) = open_in_memory_handle().await;

    let (status, events) = archive_branch(handle, new_repository(), BranchId::default()).await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// The repository's default branch fails with `DeleteDefault` in the local store, whose complete
/// event carries the code, and keeps its name.
#[tokio::test]
async fn the_default_branch_fails_delete_default() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let main = new_branch();
    create_in_store(&store, repository, main, "main", "ana", hash(2)).await;
    in_setup_context(store_repository_metadata(
        &local_write_context(&store, repository),
        main,
    ))
    .await;

    let (status, events) = archive_branch(handle, repository, main).await;

    assert_eq!(status, DeleteDefault::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, DeleteDefault::FFI_CODE)]
    );
    assert_eq!(mapped(&store, repository, "main").await, Some(main));
    close_handle(handle).await;
}

/// A protected branch fails with `DeleteProtected` in the local store, and keeps its name.
#[tokio::test]
async fn a_protected_branch_fails_delete_protected() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    in_setup_context(branch::protect(
        local_write_context(&store, repository),
        feature,
    ))
    .await
    .expect("protects the branch");

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, DeleteProtected::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, DeleteProtected::FFI_CODE)]
    );
    assert_eq!(mapped(&store, repository, "feature").await, Some(feature));
    close_handle(handle).await;
}

/// A live instance on the branch refuses the archive with `DeleteCurrent`: the call sends the live
/// instance's record alone, not the stale one's, and no branch record or location complete event,
/// and the name still maps.
#[tokio::test]
async fn a_live_instance_on_the_branch_fails_delete_current_and_reports_only_live_instances() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let live_dir = TempDir::new("graph-archive-live-");
    let live = in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(2),
        &live_dir,
    ))
    .await;
    let gone_dir = TempDir::new("graph-archive-gone-");
    in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(2),
        &gone_dir,
    ))
    .await;
    drop(gone_dir);

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, DeleteCurrent::FFI_CODE);
    let instances: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphInstance(data) => Some((data.id, data.instance_id, data.stale)),
            _ => None,
        })
        .collect();
    assert_eq!(instances, [(CALL_ID, live, 0)]);
    assert!(branch_records(&events).is_empty());
    assert!(location_outcomes(&events, CALL_ID).is_empty());
    assert_eq!(mapped(&store, repository, "feature").await, Some(feature));
    close_handle(handle).await;
}

/// An instance whose path is gone doesn't refuse the archive.
#[tokio::test]
async fn a_stale_only_instance_does_not_block_the_archive() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let gone_dir = TempDir::new("graph-archive-stale-");
    in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(2),
        &gone_dir,
    ))
    .await;
    drop(gone_dir);

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, 0);
    assert_archived_locally(&events, feature);
    close_handle(handle).await;
}

/// A branch whose id derives from its name, with no mapping and a live instance on it, refuses
/// the archive with `DeleteCurrent`: the name names it by its derived id. The branch stays live.
#[tokio::test]
async fn a_live_instance_on_an_unmapped_derived_branch_fails_delete_current() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let derived = branch::fallback_id("legacy");
    create_in_store(&store, repository, derived, "legacy", "ana", hash(2)).await;
    in_setup_context(branch::release_name(
        local_write_context(&store, repository),
        "legacy",
        derived,
    ))
    .await
    .expect("releases the mapping");
    let dir = TempDir::new("graph-archive-derived-");
    in_setup_context(register_live_instance(
        &store,
        repository,
        derived,
        hash(2),
        &dir,
    ))
    .await;

    let (status, events) = archive_branch(handle, repository, derived).await;

    assert_eq!(status, DeleteCurrent::FFI_CODE);
    assert!(location_outcomes(&events, CALL_ID).is_empty());
    let metadata = local_metadata(&store, repository, derived).await;
    assert!(
        metadata.get_u64(branch::ARCHIVED).is_err(),
        "the branch stays live"
    );
    assert_eq!(mapped(&store, repository, "legacy").await, Some(derived));
    close_handle(handle).await;
}

/// A branch archived already is reported archived, and nothing is written: its metadata key keeps
/// the value the first archive left.
#[tokio::test]
async fn an_already_archived_branch_is_reported_archived_without_a_write() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    archive(&store, repository, feature, "feature").await;
    let key = || {
        in_setup_context(branch::mutable_value(
            local_read_context(&store, repository),
            branch::METADATA,
            feature,
        ))
    };
    let before = key().await.expect("loads the metadata key");

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, 0);
    assert_archived_locally(&events, feature);
    assert_eq!(key().await.expect("loads the metadata key"), before);
    close_handle(handle).await;
}

/// A live instance on a branch id the local store lacks runs no instance check: the archive fails
/// with `BranchNotFound`, the local store holding no branch.
#[tokio::test]
async fn an_instance_on_a_branch_absent_locally_does_not_block_the_archive() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let absent = new_branch();
    let dir = TempDir::new("graph-archive-absent-");
    in_setup_context(register_live_instance(
        &store,
        repository,
        absent,
        hash(2),
        &dir,
    ))
    .await;

    let (status, events) = archive_branch(handle, repository, absent).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, 0)]
    );
    close_handle(handle).await;
}

/// A live instance on a branch archived in the local store doesn't refuse: the archive reports
/// the branch archived, and sends no instance record.
#[tokio::test]
async fn a_live_instance_on_a_locally_archived_branch_does_not_refuse() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    in_setup_context(branch::delete(
        local_write_context(&store, repository),
        feature,
    ))
    .await
    .expect("archives the branch");
    let dir = TempDir::new("graph-archive-archived-");
    in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(2),
        &dir,
    ))
    .await;

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, 0);
    assert_archived_locally(&events, feature);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LoreEvent::RevisionGraphInstance(_)))
    );
    close_handle(handle).await;
}

/// A failed read of an instance registration fails the archive in the local store: its complete
/// event carries the read's code, which isn't `DeleteCurrent`, and the name still maps.
#[tokio::test]
async fn a_failed_registration_read_fails_the_local_archive() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let dir = TempDir::new("graph-archive-unreadable-");
    in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(2),
        &dir,
    ))
    .await;
    let metadata = in_setup_context(branch::mutable_value(
        local_read_context(&store, repository),
        branch::METADATA,
        feature,
    ))
    .await
    .expect("loads the metadata key")
    .expect("the branch has metadata");
    let failing = HookedImmutableStore::wrap(store.immutable.clone(), move |call, _, address| {
        if call == ImmutableCall::Get && address.hash != metadata {
            return Err(StoreError::internal("the test fails this read"));
        }
        Ok(())
    });
    let failing = handle::register(Arc::new(with_stores(
        &store,
        failing,
        store.mutable.clone(),
    )));

    let (status, events) = archive_branch(failing, repository, feature).await;

    assert_eq!(status, Internal::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, Internal::FFI_CODE)]
    );
    assert_eq!(mapped(&store, repository, "feature").await, Some(feature));
    close_handle(failing).await;
    close_handle(handle).await;
}

/// `globals.dry_run` doesn't stop the archive.
#[tokio::test]
async fn globals_dry_run_is_ignored() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let dry_run = LoreGlobalArgs {
        dry_run: 1,
        ..Default::default()
    };

    let (status, events) = archive_with(dry_run, handle, repository, feature).await;

    assert_eq!(status, 0);
    assert_archived_locally(&events, feature);
    assert_eq!(mapped(&store, repository, "feature").await, None);
    close_handle(handle).await;
}

/// An archive of a branch whose metadata records a stopped restore under another name, which maps
/// to the branch, settles it before it decides: the name is released, the metadata marks it
/// released, and the branch is reported archived.
#[tokio::test]
async fn an_archive_settles_a_stopped_restore_before_it_decides() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(create(
        &write,
        feature,
        "feature",
        "ana",
        vec![BranchPoint {
            branch: new_branch(),
            revision: hash(1),
        }],
        hash(2),
        BranchLatestStatus::Convergent,
    ))
    .await;
    archive(&store, repository, feature, "feature").await;
    let mut metadata = local_metadata(&store, repository, feature).await;
    metadata
        .set_string(branch::RESTORING, "bee")
        .expect("records the restore");
    in_setup_context(async {
        store_metadata(&write, feature, metadata).await;
        branch::claim_name(write.clone(), "bee", Context::default(), feature)
            .await
            .expect("the stopped restore claimed its name");
    })
    .await;

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, 0);
    assert_archived_locally(&events, feature);
    assert_eq!(mapped(&store, repository, "bee").await, None);
    let settled = local_metadata(&store, repository, feature).await;
    assert_eq!(settled.get_string(branch::RELEASED).ok(), Some("bee"));
    assert!(settled.get_string(branch::RESTORING).is_err());
    close_handle(handle).await;
}

/// A live instance on an archived branch whose stopped restore claimed another name, which still
/// maps to the branch, refuses the archive with `DeleteCurrent`: the archive would release that
/// name. The name still maps and the restore stays unsettled.
#[tokio::test]
async fn a_live_instance_refuses_an_archive_that_would_settle_a_stopped_restore() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    archive(&store, repository, feature, "feature").await;
    let write = local_write_context(&store, repository);
    let mut metadata = local_metadata(&store, repository, feature).await;
    metadata
        .set_string(branch::RESTORING, "bee")
        .expect("records the restore");
    in_setup_context(async {
        store_metadata(&write, feature, metadata).await;
        branch::claim_name(write.clone(), "bee", Context::default(), feature)
            .await
            .expect("the stopped restore claimed its name");
    })
    .await;
    let dir = TempDir::new("graph-archive-stopped-restore-");
    in_setup_context(register_live_instance(
        &store,
        repository,
        feature,
        hash(2),
        &dir,
    ))
    .await;

    let (status, events) = archive_branch(handle, repository, feature).await;

    assert_eq!(status, DeleteCurrent::FFI_CODE);
    assert!(location_outcomes(&events, CALL_ID).is_empty());
    assert_eq!(mapped(&store, repository, "bee").await, Some(feature));
    let unsettled = local_metadata(&store, repository, feature).await;
    assert_eq!(unsettled.get_string(branch::RESTORING).ok(), Some("bee"));
    close_handle(handle).await;
}
