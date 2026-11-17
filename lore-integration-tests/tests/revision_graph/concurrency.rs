// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Concurrent `branch_create` calls of one name, at the remote and on one local store, and a
//! create the remote's `branch_create` refuses after the pre-checks passed. The concurrent creates
//! reach the race window only by chance; the claim's deterministic guards are the create core's
//! rollback test and `classify_create_refusal_maps_each_row`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore::storage::handle;
use lore::storage::store::StoreInternal;
use lore::storage::store::disk_backed_for_tests;
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
use lore_storage::local::store_lock::StoreHold;

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
