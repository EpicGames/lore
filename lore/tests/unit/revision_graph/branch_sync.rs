// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::RevisionForm;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncArgs;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncOptions;
use lore::revision_graph::branch_sync::SyncTarget;
use lore::revision_graph::branch_sync::TargetLocation;
use lore::revision_graph::branch_sync::branch_sync;
use lore::revision_graph::branch_sync::sync_target;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchNotFound;
use lore_base::error::Divergent;
use lore_base::error::InvalidArguments;
use lore_base::error::NoRemote;
use lore_base::error::Oversized;
use lore_base::error::RevisionNotFound;
use lore_base::error::SlowDown;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::FfiError;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::event::LoreEvent;
use lore_revision::metadata::METADATA_MAX_SIZE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::test_util::write_state_revision;
use lore_transport::BranchQueryResponse;
use lore_transport::Revision;
use lore_transport::test_util::MemoryPeer;

use crate::call::test_support::recording_callback;
use crate::call::test_support::take_without_log_lines;
use crate::revision_graph::support::NamingRevision;
use crate::revision_graph::support::QueryAnswer;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::counting_reads;
use crate::revision_graph::support::create;
use crate::revision_graph::support::create_in_store;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::metadata_naming;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::revision_graph::support::put_root_fragment;
use crate::revision_graph::support::register_live_instance;
use crate::revision_graph::support::store_bytes_as_metadata;
use crate::revision_graph::support::store_latest;
use crate::revision_graph::support::store_with_remote;
use crate::revision_graph::support::store_with_remote_storage;
use crate::revision_graph::support::with_stores;
use crate::revision_graph::support::write_line;

/// The `id` every sync in this file passes.
const CALL_ID: u64 = 7;

/// A context with the in-memory write token over a new in-memory store, for a new repository.
async fn write_context(label: &str) -> Arc<RepositoryContext> {
    local_write_context(&*in_memory_for_tests(label).await, new_repository())
}

/// The target revision [`sync_target`] resolves for a sync of `branch` in the local store `ctx`
/// reaches, whose latest of `branch` is `latest` and whose metadata key of `branch` holds
/// `metadata`, as its FFI code where it fails. Asserts that the target isn't known to be on the
/// remote's history line, as no target looked up in the local store is.
async fn local_target(
    ctx: &Arc<RepositoryContext>,
    latest: Hash,
    branch: BranchId,
    metadata: Option<Hash>,
    form: RevisionForm,
) -> Result<Option<Hash>, i32> {
    in_setup_context(sync_target(
        ctx,
        TargetLocation::Local { latest, metadata },
        branch,
        form,
    ))
    .await
    .map(|target| {
        target.map(|target| {
            assert!(!target.on_remote_line, "{target:?}");
            target.revision
        })
    })
    .map_err(|err: RevisionGraphError| err.ffi_code())
}

/// Creates `branch` with `stack` and no latest in the local store through `write`, and returns the
/// hash its metadata key holds.
async fn metadata_with_stack(
    write: &Arc<RepositoryContext>,
    branch: BranchId,
    stack: Vec<BranchPoint>,
) -> Hash {
    in_setup_context(async {
        create(
            write,
            branch,
            "feature",
            "ci",
            stack,
            Hash::default(),
            BranchLatestStatus::Divergent,
        )
        .await;
        branch::mutable_load(write.clone(), branch::METADATA, branch)
            .await
            .expect("loads the metadata key")
    })
    .await
}

/// A hash target whose revision records another branch fails with `InvalidArguments`, unless it is
/// the branch point the synced branch starts from.
#[tokio::test]
async fn a_hash_target_off_the_branch_fails_invalid_arguments() {
    let ctx = write_context("graph-sync-hash-off-branch").await;
    let (branch, other) = (new_branch(), new_branch());
    let off = in_setup_context(write_line(&ctx, other, 2)).await;
    let stack = vec![BranchPoint {
        branch: other,
        revision: off[0],
    }];
    let metadata = Some(metadata_with_stack(&ctx, branch, stack).await);

    assert_eq!(
        local_target(
            &ctx,
            Hash::default(),
            branch,
            metadata,
            RevisionForm::Hash(off[1])
        )
        .await,
        Err(InvalidArguments::FFI_CODE)
    );
    assert_eq!(
        local_target(
            &ctx,
            Hash::default(),
            branch,
            metadata,
            RevisionForm::Hash(off[0])
        )
        .await,
        Ok(Some(off[0]))
    );
}

/// A branch with an empty stack, as the default branch has, starts from no branch point, so a hash
/// target on another branch fails with `InvalidArguments`.
#[tokio::test]
async fn a_hash_target_off_the_default_branch_fails_invalid_arguments() {
    let ctx = write_context("graph-sync-hash-off-default").await;
    let (main, other) = (new_branch(), new_branch());
    let off = in_setup_context(write_line(&ctx, other, 1)).await;

    assert_eq!(
        local_target(
            &ctx,
            Hash::default(),
            main,
            None,
            RevisionForm::Hash(off[0])
        )
        .await,
        Err(InvalidArguments::FFI_CODE)
    );
}

/// A latest or a number target that names another branch fails with `InvalidArguments`.
#[tokio::test]
async fn a_branch_form_target_naming_another_branch_fails_invalid_arguments() {
    let ctx = write_context("graph-sync-other-branch").await;
    let (branch, other) = (new_branch(), new_branch());
    for form in [RevisionForm::Latest(other), RevisionForm::Number(other, 1)] {
        assert_eq!(
            local_target(&ctx, Hash::default(), branch, None, form).await,
            Err(InvalidArguments::FFI_CODE),
            "{form:?}"
        );
    }
}

/// In the local store, a hash on the branch names its revision, a number names the revision the
/// walk from the local latest finds, and a hash or number the store doesn't hold fails with
/// `RevisionNotFound`.
#[tokio::test]
async fn targets_on_the_branch_resolve_at_the_local_location() {
    let ctx = write_context("graph-sync-hash-on-branch").await;
    let branch = new_branch();
    let line = in_setup_context(write_line(&ctx, branch, 3)).await;

    for (form, expected) in [
        (RevisionForm::Hash(line[1]), Ok(Some(line[1]))),
        (RevisionForm::Number(branch, 1), Ok(Some(line[0]))),
        (
            RevisionForm::Hash(Hash::from([9; 32])),
            Err(RevisionNotFound::FFI_CODE),
        ),
        (
            RevisionForm::Number(branch, 4),
            Err(RevisionNotFound::FFI_CODE),
        ),
    ] {
        assert_eq!(
            local_target(&ctx, line[2], branch, None, form).await,
            expected,
            "{form:?}"
        );
    }
}

/// The latest of the branch names no target in the local store.
#[tokio::test]
async fn a_latest_target_at_the_local_location_is_no_target() {
    let ctx = write_context("graph-sync-local-latest").await;
    let branch = new_branch();
    let line = in_setup_context(write_line(&ctx, branch, 1)).await;

    assert_eq!(
        local_target(&ctx, line[0], branch, None, RevisionForm::Latest(branch)).await,
        Ok(None)
    );
}

/// At the remote, the latest of the branch is the latest the remote's record names, on the remote's
/// history line, and a zero one fails with `RevisionNotFound`.
#[tokio::test]
async fn a_latest_target_at_the_remote_is_the_remote_latest() {
    let ctx = write_context("graph-sync-remote-latest").await;
    let branch = new_branch();
    let service: Arc<dyn Revision> = NamingRevision::new(QueryAnswer::NotFound);
    for (latest, expected) in [
        (
            Hash::from([5; 32]),
            Ok(Some(SyncTarget {
                revision: Hash::from([5; 32]),
                on_remote_line: true,
            })),
        ),
        (Hash::default(), Err(RevisionNotFound::FFI_CODE)),
    ] {
        let target = in_setup_context(sync_target(
            &ctx,
            TargetLocation::Remote {
                service: &service,
                latest,
                stack: &[],
            },
            branch,
            RevisionForm::Latest(branch),
        ))
        .await
        .map_err(|err| err.ffi_code());
        assert_eq!(target, expected);
    }
}

/// Runs `branch_sync` of `branch` to `target` with `options` on `store` in `repository` under
/// `globals`, and returns its status and the events it sent other than log lines.
async fn sync_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    target: LoreRevisionIdentifier,
    options: LoreRevisionGraphBranchSyncOptions,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let args = LoreRevisionGraphBranchSyncArgs {
        id: CALL_ID,
        store,
        repository,
        branch,
        target,
        options,
    };
    let status = branch_sync(globals, args, callback).await;
    (status, take_without_log_lines(&events))
}

/// [`sync_with`] under default globals and with default options.
async fn sync(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    target: LoreRevisionIdentifier,
) -> (i32, Vec<LoreEvent>) {
    sync_with(
        LoreGlobalArgs::default(),
        store,
        repository,
        branch,
        target,
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await
}

/// The identifier of the revision with `hash`.
fn by_hash(hash: Hash) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        hash,
        ..Default::default()
    }
}

/// The options of a forced sync.
fn forced() -> LoreRevisionGraphBranchSyncOptions {
    LoreRevisionGraphBranchSyncOptions {
        force: 1,
        ..Default::default()
    }
}

/// The synced events among `events` as (id, branch, previous local latest, local latest).
fn synced(events: &[LoreEvent]) -> Vec<(u64, BranchId, Hash, Hash)> {
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranchSynced(data) => Some((
                data.id,
                data.branch,
                data.previous_local_latest,
                data.local_latest,
            )),
            _ => None,
        })
        .collect()
}

/// Asserts that `events` holds only the call's terminal events.
fn assert_terminal_only(events: &[LoreEvent]) {
    let [LoreEvent::Complete(_), LoreEvent::End(_)] = events else {
        panic!("expected Complete, then End");
    };
}

/// The local latest of `branch` in the local store of `store`, `None` where it holds none.
async fn latest_in(store: &StoreInternal, repository: Partition, branch: BranchId) -> Option<Hash> {
    in_setup_context(branch::mutable_value(
        local_read_context(store, repository),
        branch::LATEST,
        branch,
    ))
    .await
    .expect("loads the local latest")
}

/// A new branch in the local store of `store` with a line of `count` revisions, whose local latest
/// is the revision of the line numbered `at`, zero for none, and with an empty stack. Returns the
/// branch and the line, oldest first.
async fn branch_at(
    store: &StoreInternal,
    repository: Partition,
    count: u64,
    at: usize,
) -> (BranchId, Vec<Hash>) {
    let write = local_write_context(store, repository);
    let branch = new_branch();
    let line = in_setup_context(write_line(&write, branch, count)).await;
    let latest = at
        .checked_sub(1)
        .map_or(Hash::default(), |index| line[index]);
    in_setup_context(create(
        &write,
        branch,
        "feature",
        "ci",
        Vec::new(),
        latest,
        BranchLatestStatus::Divergent,
    ))
    .await;
    (branch, line)
}

/// A branch with no local latest moves to a hash target on it, and the synced event reports the
/// move from zero.
#[tokio::test]
async fn a_hash_target_on_the_branch_fast_forwards_from_zero() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 2, 0).await;

    let (status, events) = sync(handle, repository, branch, by_hash(line[1])).await;

    assert_eq!(status, 0);
    assert_eq!(
        synced(&events),
        [(CALL_ID, branch, Hash::default(), line[1])]
    );
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[1]));
    close_handle(handle).await;
}

/// A target the local latest descends from fails with `Divergent` and leaves the local latest,
/// and `force` moves the local latest back to it.
#[tokio::test]
async fn moving_back_fails_divergent_without_force_and_moves_with_it() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 3, 3).await;

    let (status, _) = sync(handle, repository, branch, by_hash(line[0])).await;
    assert_eq!(status, Divergent::FFI_CODE);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[2]));

    let (status, events) = sync_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        branch,
        by_hash(line[0]),
        forced(),
    )
    .await;
    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(CALL_ID, branch, line[2], line[0])]);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[0]));
    close_handle(handle).await;
}

/// An expectation other than the local latest fails with `BranchAdvanced` and writes nothing, and
/// one equal to it lets the sync move the local latest.
#[tokio::test]
async fn a_mismatched_expectation_fails_branch_advanced() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 2, 1).await;
    let expecting = |expected_local_latest| LoreRevisionGraphBranchSyncOptions {
        expected_local_latest,
        ..Default::default()
    };

    let (status, _) = sync_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        branch,
        by_hash(line[1]),
        expecting(hash(9)),
    )
    .await;
    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[0]));

    let (status, _) = sync_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        branch,
        by_hash(line[1]),
        expecting(line[0]),
    )
    .await;
    assert_eq!(status, 0);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[1]));
    close_handle(handle).await;
}

/// A target equal to the local latest succeeds without a write, and the synced event reports the
/// local latest as both.
#[tokio::test]
async fn a_target_equal_to_the_local_latest_writes_nothing() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 2, 2).await;

    let (status, events) = sync(handle, repository, branch, by_hash(line[1])).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(CALL_ID, branch, line[1], line[1])]);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[1]));
    close_handle(handle).await;
}

/// Without a remote, a walk whose target line reaches a revision the local store lacks before it
/// meets the local latest fails with `RevisionNotFound`, and the local latest stays.
#[tokio::test]
async fn a_walk_that_runs_out_of_revisions_fails_revision_not_found() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 1, 1).await;
    let write = local_write_context(&store, repository);
    let target = in_setup_context(async {
        let metadata = metadata_naming(&write, branch).await;
        write_state_revision(&write, hash(0xab), 3, metadata).await
    })
    .await;

    let (status, events) = sync(handle, repository, branch, by_hash(target)).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert_eq!(synced(&events), [(CALL_ID, branch, line[0], line[0])]);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[0]));
    close_handle(handle).await;
}

/// `globals.force` doesn't force a sync: a target off the local latest's line still fails with
/// `Divergent`.
#[tokio::test]
async fn globals_force_is_ignored() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 2, 2).await;
    let globals = LoreGlobalArgs {
        force: 1,
        ..Default::default()
    };

    let (status, _) = sync_with(
        globals,
        handle,
        repository,
        branch,
        by_hash(line[0]),
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await;

    assert_eq!(status, Divergent::FFI_CODE);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[1]));
    close_handle(handle).await;
}

/// A sync that fails after reading the local latest reports that latest as both the previous and
/// the current local latest.
#[tokio::test]
async fn a_failed_sync_reports_the_observed_latest() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 2, 1).await;
    let options = LoreRevisionGraphBranchSyncOptions {
        expected_local_latest: line[1],
        ..Default::default()
    };

    let (status, events) = sync_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        branch,
        by_hash(line[1]),
        options,
    )
    .await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert_eq!(synced(&events), [(CALL_ID, branch, line[0], line[0])]);
    close_handle(handle).await;
}

/// A sync of a branch the local store doesn't hold, on a handle without a remote, fails with
/// `BranchNotFound` before it reads a local latest, so it sends only the terminal events.
#[tokio::test]
async fn a_failure_before_the_read_emits_no_synced_event() {
    let (_store, handle) = open_in_memory_handle().await;
    let branch = new_branch();

    let (status, events) = sync(handle, new_repository(), branch, by_hash(hash(3))).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// Without a remote, a local latest without the branch's metadata key doesn't make the local store
/// hold the branch: the sync fails with `BranchNotFound` and sends only the terminal events.
#[tokio::test]
async fn a_latest_without_metadata_fails_branch_not_found_without_a_remote() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    let branch = new_branch();
    let line = in_setup_context(write_line(&write, branch, 2)).await;
    store_latest(&store, repository, branch, line[0]).await;

    let (status, events) = sync(handle, repository, branch, by_hash(line[1])).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_terminal_only(&events);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[0]));
    close_handle(handle).await;
}

/// A local sync to the branch point a branch starts from, on the parent branch, moves the local
/// latest there with `force`.
#[tokio::test]
async fn a_hash_target_at_the_branch_point_syncs_locally() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    let (main, branch) = (new_branch(), new_branch());
    let point = in_setup_context(write_line(&write, main, 1)).await[0];
    let commit = in_setup_context(async {
        let metadata = metadata_naming(&write, branch).await;
        write_state_revision(&write, point, 2, metadata).await
    })
    .await;
    let stack = vec![BranchPoint {
        branch: main,
        revision: point,
    }];
    in_setup_context(create(
        &write,
        branch,
        "x",
        "ci",
        stack,
        commit,
        BranchLatestStatus::Divergent,
    ))
    .await;
    let globals = LoreGlobalArgs {
        local: 1,
        ..Default::default()
    };

    let (status, _) = sync_with(
        globals,
        handle,
        repository,
        branch,
        by_hash(point),
        forced(),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(latest_in(&store, repository, branch).await, Some(point));
    close_handle(handle).await;
}

/// A local hash target whose revision records the synced branch as its own needs no branch point,
/// so the sync reads no branch metadata blob.
#[tokio::test]
async fn a_local_hash_target_on_the_branch_reads_no_branch_metadata() {
    let store = in_memory_for_tests("graph-sync-local-hash-reads").await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 2, 1).await;
    let blob = in_setup_context(branch::mutable_load(
        local_read_context(&store, repository),
        branch::METADATA,
        branch,
    ))
    .await
    .expect("loads the metadata key");
    let (counted, reads) = counting_reads(store.immutable.clone());
    let handle = handle::register(Arc::new(with_stores(
        &store,
        counted,
        store.mutable.clone(),
    )));
    let globals = LoreGlobalArgs {
        local: 1,
        ..Default::default()
    };

    let (status, _) = sync_with(
        globals,
        handle,
        repository,
        branch,
        by_hash(line[1]),
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[1]));
    assert_eq!(reads.of(blob), 0);
}

/// `remote` on a handle without a remote fails with `NoRemote`.
#[tokio::test]
async fn remote_flag_without_a_remote_fails_no_remote() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 1, 1).await;
    let globals = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };

    let (status, events) = sync_with(
        globals,
        handle,
        repository,
        branch,
        by_hash(line[0]),
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await;

    assert_eq!(status, NoRemote::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// `local` and `remote` together fail with `InvalidArguments`.
#[tokio::test]
async fn local_and_remote_flags_fail_invalid_arguments() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 1, 1).await;
    let globals = LoreGlobalArgs {
        local: 1,
        remote: 1,
        ..Default::default()
    };

    let (status, events) = sync_with(
        globals,
        handle,
        repository,
        branch,
        by_hash(line[0]),
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// A failed branch query at the remote fails the call. Of a branch the local store holds, the
/// synced event reports the local latest the call read; of one it doesn't hold, the call sends only
/// the terminal events.
#[tokio::test]
async fn a_failed_remote_request_reports_the_local_latest_the_call_read() {
    let repository = new_repository();
    let store =
        Arc::new(store_with_remote(repository, NamingRevision::new(QueryAnswer::SlowDown)).await);
    let handle = handle::register(store.clone());
    let (held, line) = branch_at(&store, repository, 1, 1).await;

    let (status, events) = sync(handle, repository, held, by_hash(line[0])).await;
    assert_eq!(status, SlowDown::FFI_CODE);
    assert_eq!(synced(&events), [(CALL_ID, held, line[0], line[0])]);

    let (status, events) = sync(handle, repository, new_branch(), by_hash(line[0])).await;
    assert_eq!(status, SlowDown::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// A local sync with an all-zero target sends the record of the instance on the branch, then the
/// synced event with the local latest as both, and changes nothing.
#[tokio::test]
async fn a_local_sync_with_no_target_reports_instances_and_changes_nothing() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 1, 1).await;
    let dir = TempDir::new("graph-sync-no-target-");
    let instance = register_live_instance(&store, repository, branch, line[0], &dir).await;

    let (status, events) = sync(
        handle,
        repository,
        branch,
        LoreRevisionIdentifier::default(),
    )
    .await;

    assert_eq!(status, 0);
    let [
        LoreEvent::RevisionGraphInstance(record),
        LoreEvent::RevisionGraphBranchSynced(synced),
        LoreEvent::Complete(_),
        LoreEvent::End(_),
    ] = &events[..]
    else {
        panic!("expected an instance record, the synced event, then the terminal events");
    };
    assert_eq!(
        (
            record.id,
            record.instance_id,
            record.branch,
            record.revision
        ),
        (CALL_ID, instance, branch, line[0])
    );
    assert_eq!(
        (synced.previous_local_latest, synced.local_latest),
        (line[0], line[0])
    );
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[0]));
    close_handle(handle).await;
}

/// A local sync with no target of a branch the local store doesn't hold fails with
/// `BranchNotFound` and sends only the terminal events.
#[tokio::test]
async fn a_local_sync_with_no_target_on_a_missing_branch_fails_branch_not_found() {
    let (_store, handle) = open_in_memory_handle().await;

    let (status, events) = sync(
        handle,
        new_repository(),
        new_branch(),
        LoreRevisionIdentifier::default(),
    )
    .await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// A target that reaches the local latest only through the second parent of a merge doesn't
/// descend from it along first parents, so the sync fails with `Divergent` and leaves the local
/// latest.
#[tokio::test]
async fn a_target_that_reaches_the_local_latest_only_through_a_second_parent_fails_divergent() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (branch, line) = branch_at(&store, repository, 2, 2).await;
    let write = local_write_context(&store, repository);
    let merge = in_setup_context(async {
        let state = State::new();
        state.set_parent_self(line[0]);
        state.set_parent_other(line[1]);
        state.set_revision_number(3);
        state.set_metadata_hash(metadata_naming(&write, branch).await);
        let token = write
            .try_write_token()
            .expect("the context carries a write token");
        state
            .serialize(write.clone(), token)
            .await
            .expect("writes the merge")
    })
    .await;

    let (status, events) = sync(handle, repository, branch, by_hash(merge)).await;

    assert_eq!(status, Divergent::FFI_CODE);
    assert_eq!(synced(&events), [(CALL_ID, branch, line[1], line[1])]);
    assert_eq!(latest_in(&store, repository, branch).await, Some(line[1]));
    close_handle(handle).await;
}

/// A remote branch whose metadata blob is larger than 1 MiB fails with `Oversized` before it is
/// brought in: the local store gets no metadata key for it, and the call reports no synced event.
#[tokio::test]
async fn a_remote_metadata_blob_over_one_mebibyte_fails_oversized_and_brings_nothing_in() {
    let repository = new_repository();
    let feature = new_branch();
    let scratch = in_memory_for_tests("graph-sync-oversized-metadata").await;
    let blob = in_setup_context(store_bytes_as_metadata(
        &local_write_context(&scratch, repository),
        feature,
        &vec![7; METADATA_MAX_SIZE + 1],
    ))
    .await;
    let peer = MemoryPeer::new();
    put_root_fragment(&peer, &scratch, repository, blob).await;
    let service = NamingRevision::new(QueryAnswer::Answer(BranchQueryResponse {
        id: feature,
        name: "feature".to_owned(),
        latest: hash(5),
        metadata: blob.hash,
        ..Default::default()
    }));
    let store = Arc::new(store_with_remote_storage(repository, service, vec![peer]).await);
    let handle = handle::register(store.clone());

    let (status, events) = sync(
        handle,
        repository,
        feature,
        LoreRevisionIdentifier::default(),
    )
    .await;
    close_handle(handle).await;

    assert_eq!(status, Oversized::FFI_CODE);
    assert_terminal_only(&events);
    assert_eq!(
        in_setup_context(branch::mutable_value(
            local_read_context(&store, repository),
            branch::METADATA,
            feature,
        ))
        .await
        .expect("loads the metadata key"),
        None
    );
}

/// A sync to a local latest that is the remote latest clears only the latest status recorded for
/// that latest: a status another writer stored for another revision, as a commit that lands after
/// the sync's read stores it, stays.
#[tokio::test]
async fn an_in_sync_sync_keeps_a_latest_status_another_writer_stored() {
    let repository = new_repository();
    let feature = new_branch();
    let latest = hash(5);
    let service = NamingRevision::new(QueryAnswer::Answer(BranchQueryResponse {
        id: feature,
        name: "feature".to_owned(),
        latest,
        metadata: hash(7),
        ..Default::default()
    }));
    let store = Arc::new(store_with_remote(repository, service).await);
    let handle = handle::register(store.clone());
    create_in_store(&store, repository, feature, "feature", "ci", latest).await;
    let committed = hash(6);
    in_setup_context(branch::mutable_try_store(
        local_write_context(&store, repository),
        branch::LATEST_STATUS,
        feature,
        Hash::default(),
        committed,
    ))
    .await
    .expect("stores another writer's status");

    let (status, events) = sync(
        handle,
        repository,
        feature,
        LoreRevisionIdentifier::default(),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(CALL_ID, feature, latest, latest)]);
    let value = |function| {
        in_setup_context(branch::mutable_value(
            local_read_context(&store, repository),
            function,
            feature,
        ))
    };
    assert_eq!(
        value(branch::LATEST_STATUS)
            .await
            .expect("loads the status"),
        Some(committed)
    );
    assert_eq!(
        value(branch::LAST_SYNC).await.expect("loads the last sync"),
        Some(latest)
    );
    close_handle(handle).await;
}
