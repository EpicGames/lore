// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Concurrent `branch_create` calls of one name, at the remote and on one local store, a create
//! the remote's `branch_create` refuses after the pre-checks passed, a `branch_sync` and a
//! `branch_push` each racing a tree commit on its branch, and two pushes racing each other. The
//! concurrent creates reach the race window only by chance; the claim's deterministic guards are
//! the create core's rollback test and `classify_create_refusal_maps_each_row`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncOptions;
use lore::storage::handle;
use lore::storage::store::StoreInternal;
use lore::storage::store::disk_backed_for_tests;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchAlreadyExists;
use lore_base::error::BranchNotFound;
use lore_base::error::NotAuthorized;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::repository::SALT_LORE;
use lore_storage::local::store_lock::StoreHold;

use super::counting::open_counted_handle;
use super::support::*;
use crate::common::net_common::close_handle;
use crate::common::net_common::open_handle;

/// The statuses of two calls, lowest first.
fn sorted(mut statuses: [i32; 2]) -> [i32; 2] {
    statuses.sort_unstable();
    statuses
}

/// Two creates of one name with `remote`, through two storage handles, leave one branch of that
/// name at the remote: one succeeds and the other fails with `BranchAlreadyExists`.
#[tokio::test]
async fn two_concurrent_creates_of_one_name_leave_one_winner_at_the_remote() {
    let f = fixture().await;
    seed_main(&f).await;
    let other = open_handle(Some(&f.url)).await;
    let remote = graph_globals_at(LoreBranchLocation::Remote);

    let ((first, _), (second, _)) = tokio::join!(
        create_branch(
            &f,
            f.store,
            remote.clone(),
            "race",
            latest(f.default_branch)
        ),
        create_branch(&f, other, remote.clone(), "race", latest(f.default_branch)),
    );
    close_handle(other).await;

    assert_eq!(sorted([first, second]), [0, BranchAlreadyExists::FFI_CODE]);
    let (status, events) = list_branches(&f, f.store, remote, false, "").await;
    assert_eq!(status, 0);
    let named: Vec<BranchId> = branch_records(&events)
        .into_iter()
        .filter(|record| record.name.as_str() == "race")
        .map(|record| record.branch)
        .collect();
    assert_eq!(named.len(), 1, "one live branch holds the name");
    f.close().await;
}

/// Two local creates of one name, through two storage handles over one disk-backed store, leave one
/// branch of that name, archived ones included, and no metadata key of the loser: the store holds
/// the metadata of the parent and of the winner alone.
#[tokio::test]
async fn two_concurrent_local_creates_leave_one_winner_on_a_shared_store() {
    let dir = TempDir::new("graph-concurrent-local-creates-");
    let disk = disk_backed_for_tests("graph-concurrent-local-creates", dir.path()).await;
    let f = fixture_without_remote().await;
    let parent = BranchId::from(uuid::Uuid::now_v7());
    in_client(branch::create_record(
        lore::revision_graph::locations::local_write_context(&disk, f.repository),
        parent,
        "parent",
        branch::default_category(),
        CREATOR,
        lore_revision::util::time::timestamp(),
        Vec::new(),
        Hash::from([7; 32]),
        BranchLatestStatus::Divergent,
        Context::default(),
    ))
    .await
    .expect("creates the parent");
    let [first_handle, second_handle] = [(), ()].map(|()| over_stores_of(&disk));

    let ((first, _), (second, _)) = tokio::join!(
        create_branch(&f, first_handle, graph_globals(), "race", latest(parent)),
        create_branch(&f, second_handle, graph_globals(), "race", latest(parent)),
    );

    assert_eq!(sorted([first, second]), [0, BranchAlreadyExists::FFI_CODE]);
    let (status, events) = list_branches(&f, first_handle, graph_globals(), true, "").await;
    assert_eq!(status, 0);
    let named = branch_records(&events)
        .into_iter()
        .filter(|record| record.name.as_str() == "race")
        .count();
    assert_eq!(named, 1, "one branch of the name, archived ones included");
    let held = in_client(async {
        let mut listed = disk
            .mutable
            .clone()
            .list(f.repository, KeyType::BranchMetadata)
            .await
            .expect("lists the metadata keys")
            .channel();
        let mut held = 0;
        while let Some((_, value)) = listed.recv().await {
            held += usize::from(!value.is_zero());
        }
        held
    })
    .await;
    assert_eq!(held, 2, "the parent's metadata key and the winner's");
    close_handle(first_handle).await;
    close_handle(second_handle).await;
    f.close().await;
}

/// A storage handle without a remote over the stores of `store`, holding no claim on them.
fn over_stores_of(store: &StoreInternal) -> lore::storage::handle::LoreStore {
    handle::register(Arc::new(StoreInternal::new(
        store.identity.clone(),
        store.immutable.clone(),
        store.mutable.clone(),
        None,
        store.bound_flags,
        store.trust_remote,
        StoreHold::default(),
    )))
}

/// A create whose pre-checks the remote answers, and whose `branch_create` it refuses for want of
/// write access, fails with `NotAuthorized`: the remote's complete event carries the code with no
/// record, no local complete event follows, and the local store holds no branch of the name.
#[tokio::test]
async fn a_create_refused_by_the_remote_rpc_emits_its_error_and_no_local_event() {
    let armed = Arc::new(AtomicBool::new(false));
    let f = fixture_with_authorizer(Arc::new(ReadOnly(armed.clone()))).await;
    seed_main(&f).await;
    armed.store(true, Ordering::SeqCst);

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, NotAuthorized::FFI_CODE);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, 0, NotAuthorized::FFI_CODE)]
    );
    assert!(branch_records(&events).is_empty());
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_eq!(status, BranchNotFound::FFI_CODE);
    f.close().await;
}

/// The options of a forced sync that proceeds only while the local latest is `expected`.
fn forced_from(expected: Hash) -> LoreRevisionGraphBranchSyncOptions {
    LoreRevisionGraphBranchSyncOptions {
        force: 1,
        expected_local_latest: expected,
    }
}

/// A forced sync expecting the local latest a tree commit on the branch also starts from, run
/// together with that commit, leaves exactly one of them failed: a commit that lands first fails
/// the sync with `BranchAdvanced`, and a sync that lands first fails the commit with a tip
/// collision. The local latest is the winner's revision.
#[tokio::test]
async fn a_sync_racing_a_tree_commit_leaves_one_failed() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let published = publish_from_another_handle(&f, f.default_branch, seeded).await;

    let ((committed, revision), (synced_status, _)) = tokio::join!(
        try_commit_on(f.store, f.repository, f.default_branch, seeded),
        sync_branch(
            &f,
            f.store,
            graph_globals(),
            f.default_branch,
            LoreRevisionIdentifier::default(),
            forced_from(seeded),
        ),
    );

    let failed = [committed != 0, synced_status != 0];
    assert_eq!(
        failed.iter().filter(|failed| **failed).count(),
        1,
        "exactly one fails: commit {committed}, sync {synced_status}"
    );
    let winner = if synced_status == 0 {
        published
    } else {
        assert_eq!(synced_status, BranchAdvanced::FFI_CODE);
        revision
    };
    assert_eq!(
        local_latest(f.store, f.repository, f.default_branch).await,
        winner
    );
    f.close().await;
}

/// A forced sync whose expected local latest a commit has moved past fails with `BranchAdvanced`,
/// and the commit stays the local latest.
#[tokio::test]
async fn expected_local_latest_keeps_a_forced_sync_off_a_moved_latest() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let (status, _) = sync_branch(
        &f,
        f.store,
        graph_globals(),
        f.default_branch,
        LoreRevisionIdentifier::default(),
        forced_from(seeded),
    )
    .await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert_eq!(
        local_latest(f.store, f.repository, f.default_branch).await,
        committed
    );
    f.close().await;
}

/// A sync whose compare-and-swap of the local latest finds another writer's value, stored after
/// the sync read the local latest, fails with `BranchAdvanced`, and its synced event reports the
/// value that won as both latests.
#[tokio::test]
async fn a_sync_that_loses_its_swap_reports_the_latest_that_won() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    let won = Hash::from([5; 32]);
    let latest_key =
        branch::mutable_key(SALT_LORE, branch::LATEST, f.repository, f.default_branch).0;
    let overtake = Overtake::default();
    *overtake.lock().unwrap() = Some((latest_key, won));
    let (store, _, _) = open_counted_handle(&f, None, overtaking_load(overtake));

    let (status, events) = sync_branch(
        &f,
        store,
        graph_globals(),
        f.default_branch,
        LoreRevisionIdentifier::default(),
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await;
    close_handle(store).await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert_eq!(synced(&events), [(won, won)]);
    assert_eq!(
        local_latest(f.store, f.repository, f.default_branch).await,
        won
    );
    f.close().await;
}

/// A tree commit that runs with a push of its branch's local latest is not overwritten: whichever
/// lands first, both succeed, the local latest afterwards is the commit's revision, and the remote
/// latest is the revision the push published. An integration counterpart of the push core's
/// `a_local_write_that_misses_leaves_the_commit_and_succeeds`.
#[tokio::test]
async fn a_tree_commit_during_a_push_is_not_overwritten() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let pushed_revision = commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let ((committed, revision), (pushed_status, events)) = tokio::join!(
        try_commit_on(f.store, f.repository, f.default_branch, pushed_revision),
        push_branch_call(
            &f,
            f.store,
            graph_globals(),
            f.default_branch,
            Default::default()
        ),
    );

    assert_eq!((committed, pushed_status), (0, 0));
    let [event] = pushed(&events)[..] else {
        panic!("one pushed event");
    };
    assert_eq!(
        (
            local_latest(f.store, f.repository, f.default_branch).await,
            server_latest(&f, f.default_branch).await
        ),
        (revision, event.revision_hash)
    );
    f.close().await;
}

/// Two pushes of one revision each onto the same remote latest, through two storage handles, leave
/// one published and the other failed with `BranchAdvanced`.
#[tokio::test]
async fn concurrent_single_revision_pushes_leave_one_branch_advanced() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let other = open_handle(Some(&f.url)).await;
    commit_on(f.store, f.repository, f.default_branch, seeded).await;
    commit_on(other, f.repository, f.default_branch, seeded).await;

    let ((first, _), (second, _)) = tokio::join!(
        push_branch_call(
            &f,
            f.store,
            graph_globals(),
            f.default_branch,
            Default::default()
        ),
        push_branch_call(
            &f,
            other,
            graph_globals(),
            f.default_branch,
            Default::default()
        ),
    );
    close_handle(other).await;

    assert_eq!(sorted([first, second]), [0, BranchAdvanced::FFI_CODE]);
    f.close().await;
}
