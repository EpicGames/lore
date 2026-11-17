// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `branch_create` against Lore Server: one record per location created, the remote first, each
//! followed by its location complete event, from a branch's latest, a hash and a revision number;
//! the parent's record taken from the remote where the
//! local store lacks it; the location flags and the latest status they leave; branch points that
//! differ or don't resolve at a location; taken, archived and unused names and parents; a remote
//! that refuses connections; the default-branch check at the remote; and a commit on the new
//! branch through the revision tree API.

use lore_base::error::BranchAlreadyExists;
use lore_base::error::BranchNotFound;
use lore_base::error::Divergent;
use lore_base::error::InvalidArguments;
use lore_base::error::RevisionNotFound;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchEventData;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreBranchPoint;

use super::support::*;
use crate::common::net_common::close_handle;

/// `branch` at `revision` as a record's stack carries it.
fn point(branch: BranchId, revision: Hash) -> LoreBranchPoint {
    LoreBranchPoint::from(&BranchPoint { branch, revision })
}

/// The branch records of `events`, after asserting that the call created the branch at the remote,
/// then in the local store, each record followed by its location complete event with one record.
fn created_at_both(
    events: &[lore_revision::event::LoreEvent],
) -> (
    &LoreRevisionGraphBranchEventData,
    &LoreRevisionGraphBranchEventData,
) {
    assert_eq!(
        event_order(events),
        [
            (LoreBranchLocation::Remote, false),
            (LoreBranchLocation::Remote, true),
            (LoreBranchLocation::Local, false),
            (LoreBranchLocation::Local, true),
        ]
    );
    assert_eq!(
        location_outcomes(events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    let [remote, local] = branch_records(events)[..] else {
        panic!("expected two branch records");
    };
    (remote, local)
}

/// A create with no flag writes one record at the remote and the same one in the local store: the
/// same id, name, category, creator and stack, and both latests at the branch point, with the
/// local latest in sync with the remote.
#[tokio::test]
async fn a_both_location_create_writes_the_same_record_at_both_and_starts_in_sync() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, 0);
    let (remote, local) = created_at_both(&events);
    assert_eq!(
        (
            remote.branch,
            remote.name.as_str(),
            remote.category.as_str()
        ),
        (local.branch, local.name.as_str(), local.category.as_str())
    );
    assert_eq!(remote.name.as_str(), "feature");
    assert_eq!(remote.category.as_str(), branch::default_category());
    assert_eq!(remote.creator.as_str(), local.creator.as_str());
    assert_eq!(remote.stack.as_slice(), local.stack.as_slice());
    assert_eq!(remote.stack.as_slice(), [point(f.default_branch, seeded)]);
    assert_eq!((remote.divergent, local.divergent), (0, 0));
    assert_eq!(server_latest(&f, remote.branch).await, seeded);
    assert_eq!(
        local_latest(f.store, f.repository, local.branch).await,
        seeded
    );
    let (status, events) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_eq!(status, 0);
    assert_eq!(branch_records(&events)[0].divergent, 0);
    f.close().await;
}

/// A create from a hash with no flag starts at that revision at both locations, on the branch its
/// metadata names.
#[tokio::test]
async fn a_both_location_create_from_a_hash_takes_the_revisions_own_branch() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let latest_main = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    publish(&f, f.default_branch, latest_main).await;

    let (status, events) =
        create_branch(&f, f.store, graph_globals(), "feature", by_hash(seeded)).await;

    assert_eq!(status, 0);
    let (remote, local) = created_at_both(&events);
    for record in [remote, local] {
        assert_eq!(record.latest, seeded);
        assert_eq!(record.stack.as_slice(), [point(f.default_branch, seeded)]);
    }
    f.close().await;
}

/// A create from a revision number with no flag starts at the revision with that number at both
/// locations, not at the branch's latest.
#[tokio::test]
async fn a_both_location_create_from_a_number_branches_at_that_revision() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let latest_main = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    publish(&f, f.default_branch, latest_main).await;

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        numbered(f.default_branch, 1),
    )
    .await;

    assert_eq!(status, 0);
    let (remote, local) = created_at_both(&events);
    for record in [remote, local] {
        assert_eq!(record.latest, seeded);
        assert_eq!(record.stack.as_slice(), [point(f.default_branch, seeded)]);
    }
    f.close().await;
}

/// Where the local store holds the parent's latest without its metadata, as a handle that only
/// committed on the parent holds it, the local record takes its stack from the remote's record of
/// the parent.
#[tokio::test]
async fn a_both_location_create_uses_the_remote_parent_record_when_the_local_store_lacks_it() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let main_metadata = in_client(branch::local_metadata(local_writer(&f), f.default_branch))
        .await
        .expect("reads the local store");
    assert!(
        main_metadata.is_none(),
        "the local store lacks main's metadata"
    );

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, 0);
    let (remote, local) = created_at_both(&events);
    assert_eq!(local.stack.as_slice(), remote.stack.as_slice());
    let stored = local_branch_metadata(&f, local.branch).await;
    assert_eq!(
        branch::stack(&stored),
        [BranchPoint {
            branch: f.default_branch,
            revision: seeded,
        }]
    );
    f.close().await;
}

/// A create with `local` writes the local store alone, and the branch starts divergent there.
#[tokio::test]
async fn a_local_only_create_starts_divergent() {
    let f = fixture().await;
    seed_main(&f).await;
    let parent = create_local_branch(&f, "parent", f.default_branch, CREATOR).await;
    let branch_point = local_latest(f.store, f.repository, parent).await;

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
        latest(parent),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Local, 1, 0)]
    );
    let [record] = branch_records(&events)[..] else {
        panic!("expected one branch record");
    };
    assert_eq!(
        (record.location, record.divergent),
        (LoreBranchLocation::Local, 1)
    );
    assert_eq!(record.stack.as_slice()[0], point(parent, branch_point));
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        "feature",
    )
    .await;
    assert_eq!(status, BranchNotFound::FFI_CODE);
    f.close().await;
}

/// A create with `remote` writes the remote alone: the local store holds no record of the branch.
#[tokio::test]
async fn a_remote_only_create_leaves_no_local_record() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        "feature",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, 1, 0)]
    );
    let created = branch_records(&events)[0].branch;
    assert_eq!(server_latest(&f, created).await, seeded);
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(
        local_latest(f.store, f.repository, created).await,
        Hash::default()
    );
    f.close().await;
}

/// A parent whose latest differs between the locations, after a local commit not yet published,
/// fails with `Divergent` before either location is written.
#[tokio::test]
async fn a_branch_point_that_differs_between_locations_fails_divergent() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, Divergent::FFI_CODE);
    assert!(location_outcomes(&events).is_empty());
    assert!(branch_records(&events).is_empty());
    for location in [LoreBranchLocation::Remote, LoreBranchLocation::Local] {
        let (status, _) = resolve_branch(&f, f.store, graph_globals_at(location), "feature").await;
        assert_eq!(status, BranchNotFound::FFI_CODE, "{location}");
    }
    f.close().await;
}

/// A revision the local store holds and the remote lacks fails with `RevisionNotFound`: the branch
/// point must resolve at every location the call creates on.
#[tokio::test]
async fn a_branch_point_missing_at_the_remote_fails_revision_not_found() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let offline = handle_over_local_stores(&f, None);
    let local_only = commit_on(offline, f.repository, f.default_branch, seeded).await;
    close_handle(offline).await;

    let (status, events) =
        create_branch(&f, f.store, graph_globals(), "feature", by_hash(local_only)).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert!(location_outcomes(&events).is_empty());
    f.close().await;
}

/// A name a live branch holds at the remote fails with `BranchAlreadyExists` before either
/// location is written: no location complete event, and no local record of the name.
#[tokio::test]
async fn a_name_a_live_remote_branch_holds_fails_branch_already_exists_before_any_write() {
    let f = fixture().await;
    seed_main(&f).await;
    create_remote_branch(&f, "taken", f.default_branch, CREATOR).await;

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "Taken",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, BranchAlreadyExists::FFI_CODE);
    assert!(location_outcomes(&events).is_empty());
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "taken",
    )
    .await;
    assert_eq!(status, BranchNotFound::FFI_CODE);
    f.close().await;
}

/// A name an archived branch held gets a new branch with a new id at both locations, and the
/// archived branch stays listed with `include_archived`.
#[tokio::test]
async fn an_archived_remote_name_gets_a_new_id() {
    let f = fixture().await;
    seed_main(&f).await;
    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;
    assert_eq!(status, 0);
    let first = branch_records(&events)[0].branch;
    archive_on_server(&f, first).await;
    archive_locally(&f, first).await;

    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, 0);
    let (remote, local) = created_at_both(&events);
    assert_eq!(remote.branch, local.branch);
    assert_ne!(remote.branch, first);
    let (status, events) = list_branches(&f, f.store, graph_globals(), true, "").await;
    assert_eq!(status, 0);
    let archived: Vec<_> = branch_records(&events)
        .into_iter()
        .filter(|record| record.branch == first)
        .map(|record| (record.location, record.archived))
        .collect();
    assert_eq!(
        archived,
        [
            (LoreBranchLocation::Remote, 1),
            (LoreBranchLocation::Local, 1)
        ]
    );
    f.close().await;
}

/// A parent no location holds fails with `BranchNotFound`, with no flag and with `remote`.
#[tokio::test]
async fn a_parent_no_location_holds_fails_branch_not_found() {
    let f = fixture().await;
    seed_main(&f).await;
    let unused = BranchId::from(uuid::Uuid::now_v7());

    for globals in [
        graph_globals(),
        graph_globals_at(LoreBranchLocation::Remote),
    ] {
        let (status, events) = create_branch(&f, f.store, globals, "feature", latest(unused)).await;
        assert_eq!(status, BranchNotFound::FFI_CODE);
        assert!(location_outcomes(&events).is_empty());
    }
    f.close().await;
}

/// A remote that refuses connections fails the remote's pre-check: its complete event carries the
/// error, and the call stops before the local store.
#[tokio::test]
async fn a_remote_pre_check_read_failure_reports_the_remote_error() {
    let f = fixture().await;
    let unreachable = handle_over_closed_port(&f);

    let (status, events) = create_branch(
        &f,
        unreachable,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;
    close_handle(unreachable).await;

    assert_ne!(status, 0);
    let [(LoreBranchLocation::Remote, 0, code)] = location_outcomes(&events)[..] else {
        panic!("expected the remote's complete event alone");
    };
    assert_eq!(code, status);
    assert!(branch_records(&events).is_empty());
    f.close().await;
}

/// At the remote, the default branch without a revision starts a branch at a zero branch point,
/// and another branch without a latest revision fails with `InvalidArguments`.
#[tokio::test]
async fn a_zero_branch_point_is_checked_against_the_remotes_default_branch() {
    let f = fixture().await;
    let remote = graph_globals_at(LoreBranchLocation::Remote);

    let (status, events) = create_branch(
        &f,
        f.store,
        remote.clone(),
        "empty",
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, 0);
    let [record] = branch_records(&events)[..] else {
        panic!("expected one branch record");
    };
    assert_eq!(
        record.stack.as_slice(),
        [point(f.default_branch, Hash::default())]
    );
    let empty = record.branch;
    let (status, events) = create_branch(&f, f.store, remote, "child", latest(empty)).await;
    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert!(location_outcomes(&events).is_empty());
    f.close().await;
}

/// The new branch takes a commit through the revision tree API: the commit moves its local latest,
/// which `revision_resolve` with `local` then resolves.
#[tokio::test]
async fn the_new_branch_takes_a_commit_through_the_revision_tree_api() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;
    assert_eq!(status, 0);
    let created = branch_records(&events)[0].branch;

    let committed = commit_on(f.store, f.repository, created, seeded).await;

    assert_eq!(
        local_latest(f.store, f.repository, created).await,
        committed
    );
    let (status, events) = resolve_revision(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        latest(created),
    )
    .await;
    assert_eq!(status, 0);
    let [record] = revision_records(&events)[..] else {
        panic!("expected one revision record");
    };
    assert_eq!((record.revision_hash, record.branch), (committed, created));
    f.close().await;
}
