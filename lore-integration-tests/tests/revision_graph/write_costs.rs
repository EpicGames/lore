// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What `branch_create` and `branch_sync` read and write in the local store in one call with no
//! location flag, counted on a handle over the fixture handle's local stores: the create's
//! pre-checks' loads and reads, then exactly the record it creates, and the keys a sync writes; and
//! that a malformed revision identifier touches no store.

use std::sync::atomic::Ordering;

use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncOptions;
use lore_base::error::InvalidArguments;
use lore_base::test_util::TempDir;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::repository::SALT_LORE;

use super::counting::ByKeyType;
use super::counting::LocalCalls;
use super::counting::counted;
use super::counting::open_counted_handle;
use super::support::*;
use crate::common::net_common::close_handle;

/// A create from a branch the local store holds with metadata, at both locations, loads the
/// name's mapping, the latest of the id the name derives, the parent's metadata key and latest, and
/// reads the parent's metadata blob. It then writes exactly its record: its metadata blob, swapped
/// into the metadata key from zero, the name's mapping, the latest and the latest's status, cleared
/// as convergent, each with a compare-and-swap, and the latest's history: a load of the history
/// key, and a stored history blob the key then names. Each blob write queries the store before it puts. The parent's latest is a
/// revision committed on it, so no history blob of another branch holds the new branch's entry.
#[tokio::test]
async fn branch_create_local_writes_exactly_its_record() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let parent = create_branch_at_both_locations(
        &f,
        "parent",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let committed = commit_on(f.store, f.repository, parent, seeded).await;
    publish(&f, parent, committed).await;

    let (status, _, calls) = counted(&f, async |store| {
        create_branch(&f, store, graph_globals(), "feature", latest(parent)).await
    })
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        calls,
        LocalCalls {
            loads: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                branch_latest_pointer: 2,
                untyped: 1,
                ..Default::default()
            },
            swaps: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                branch_latest_pointer: 1,
                untyped: 1,
                ..Default::default()
            },
            gets: 1,
            queries: 2,
            puts: 2,
            others: 1,
            ..Default::default()
        }
    );
    f.close().await;
}

/// A create with an invalid name fails before any read: no call on either local store, no read on
/// the server's store, and no location complete event.
#[tokio::test]
async fn branch_create_with_an_invalid_name_touches_no_store() {
    let f = fixture().await;
    let (store, mutable, immutable) = open_counted_handle(&f, None, None);
    let server_gets = f.server_gets.load(Ordering::SeqCst);

    let (status, events) =
        create_branch(&f, store, graph_globals(), "", latest(f.default_branch)).await;
    close_handle(store).await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_eq!((mutable.total(), immutable.total()), (0, 0));
    assert_eq!(f.server_gets.load(Ordering::SeqCst), server_gets);
    assert!(location_outcomes(&events).is_empty());
    f.close().await;
}

/// A sync that fast-forwards a branch the local store holds, with an instance registered on it,
/// writes exactly the branch's latest, with a compare-and-swap, then its latest status, its latest
/// history and its last synchronized revision, and no instance key.
#[tokio::test]
async fn branch_sync_writes_only_the_branchs_keys() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let dir = TempDir::new("graph-sync-writes-");
    register_instance_at(&f, feature, seeded, &dir).await;
    let published = publish_from_another_handle(&f, feature, seeded).await;
    let (store, mutable, _) = open_counted_handle(&f, None, None);

    let (status, _) = sync_branch(
        &f,
        store,
        graph_globals(),
        feature,
        LoreRevisionIdentifier::default(),
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await;
    close_handle(store).await;

    assert_eq!(status, 0);
    assert_eq!(
        local_value(&f, branch::LATEST, feature).await,
        Some(published)
    );
    let key = |function| branch::mutable_key(SALT_LORE, function, f.repository, feature);
    assert_eq!(
        mutable.written(),
        [
            key(branch::LATEST),
            key(branch::LATEST_STATUS),
            key(branch::LATEST_HISTORY),
            key(branch::LAST_SYNC),
        ]
    );
    f.close().await;
}

/// A sync at the remote to a number target at the local latest, which the remote's revision list
/// numbers so, of a branch the local store holds, writes nothing where the latest status is
/// convergent, and only the latest status, cleared with a compare-and-swap, where it is divergent.
/// A first sync brings the branch in, which the counts leave out.
#[tokio::test]
async fn branch_sync_to_the_local_latest_writes_only_a_divergent_status() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    let sync_to_local_latest = async |store| {
        let (status, _) = sync_branch(
            &f,
            store,
            graph_globals(),
            f.default_branch,
            numbered(f.default_branch, 1),
            LoreRevisionGraphBranchSyncOptions::default(),
        )
        .await;
        assert_eq!(status, 0);
    };
    let counted_sync = async || {
        let (store, mutable, _) = open_counted_handle(&f, None, None);
        sync_to_local_latest(store).await;
        close_handle(store).await;
        mutable.written()
    };
    sync_to_local_latest(f.store).await;

    let convergent = counted_sync().await;
    record_divergent(&f, f.default_branch, seeded).await;
    let divergent = counted_sync().await;

    let key = |function| branch::mutable_key(SALT_LORE, function, f.repository, f.default_branch);
    assert_eq!(convergent, []);
    assert_eq!(divergent, [key(branch::LATEST_STATUS)]);
    f.close().await;
}

/// A create whose `from` names both a hash and a branch, and a sync whose target names both a hash
/// and a revision number, fail with `InvalidArguments` before any store access, the connect to the
/// remote included.
#[tokio::test]
async fn malformed_identifiers_touch_no_store_for_create_and_sync() {
    let f = fixture().await;
    let (store, mutable, immutable) = open_counted_handle(&f, Some(closed_port_remote(&f)), None);
    let malformed_from = LoreRevisionIdentifier {
        hash: Hash::from([3; 32]),
        branch: f.default_branch,
        revision_number: 0,
    };
    let malformed_target = LoreRevisionIdentifier {
        hash: Hash::from([3; 32]),
        branch: Default::default(),
        revision_number: 2,
    };

    let (created, create_events) =
        create_branch(&f, store, graph_globals(), "feature", malformed_from).await;
    let (sync_status, sync_events) = sync_branch(
        &f,
        store,
        graph_globals(),
        f.default_branch,
        malformed_target,
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await;
    close_handle(store).await;

    assert_eq!(
        (created, sync_status),
        (InvalidArguments::FFI_CODE, InvalidArguments::FFI_CODE)
    );
    assert_eq!((mutable.total(), immutable.total()), (0, 0));
    assert!(location_outcomes(&create_events).is_empty());
    assert!(synced(&sync_events).is_empty());
    f.close().await;
}
