// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `revision_resolve` and `branch_resolve` against Lore Server: the record each location reports,
//! the remote first, and the location complete events that end them; revision numbers each location
//! resolves on its own line; the location flags; a remote that refuses connections; and of a
//! branch, the protect flag the remote's blob holds, an archive at the remote, names compared
//! regardless of case, and the instance records that follow the local record.

use lore_base::error::BranchNotFound;
use lore_base::error::Disconnected;
use lore_base::error::InvalidArguments;
use lore_base::error::NoRemote;
use lore_base::error::NotConnected;
use lore_base::error::RevisionNotFound;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreBranchPoint;
use lore_revision::interface::LoreGlobalArgs;

use super::support::*;
use crate::common::net_common::close_handle;

/// The location, hash, number and branch of each revision record among `events`, in order.
fn revisions(events: &[LoreEvent]) -> Vec<(LoreBranchLocation, Hash, u64, BranchId)> {
    revision_records(events)
        .into_iter()
        .map(|record| {
            (
                record.location,
                record.revision_hash,
                record.revision_number,
                record.branch,
            )
        })
        .collect()
}

/// A branch's latest resolves at the remote, then in the local store, each location's record
/// followed by its complete event: the revision, its number and the branch its metadata names.
#[tokio::test]
async fn latest_resolves_at_both_locations_remote_first() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;

    let (status, events) =
        resolve_revision(&f, f.store, graph_globals(), latest(f.default_branch)).await;

    assert_eq!(status, 0);
    assert_eq!(
        event_order(&events),
        [
            (LoreBranchLocation::Remote, false),
            (LoreBranchLocation::Remote, true),
            (LoreBranchLocation::Local, false),
            (LoreBranchLocation::Local, true),
        ]
    );
    assert_eq!(
        revisions(&events),
        [
            (LoreBranchLocation::Remote, seeded, 1, f.default_branch),
            (LoreBranchLocation::Local, seeded, 1, f.default_branch),
        ]
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    f.close().await;
}

/// Where the branch's lines part after revision 1, number 2 names a different revision at each
/// location, and each location reports its own: the revision committed in the local store, and the
/// one another handle published at the remote.
#[tokio::test]
async fn a_number_resolves_differently_per_location_when_the_lines_diverge() {
    let f = fixture().await;
    let published = seed_main(&f).await;
    let in_local_store = commit_on(f.store, f.repository, f.default_branch, published).await;
    let at_remote = publish_from_another_handle(&f, f.default_branch, published).await;

    let (status, events) =
        resolve_revision(&f, f.store, graph_globals(), numbered(f.default_branch, 2)).await;

    assert_eq!(status, 0);
    assert_eq!(
        revisions(&events),
        [
            (LoreBranchLocation::Remote, at_remote, 2, f.default_branch),
            (
                LoreBranchLocation::Local,
                in_local_store,
                2,
                f.default_branch
            ),
        ]
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    f.close().await;
}

/// With the `remote` flag, each number on a branch of three published revisions resolves at the
/// remote to the revision with that number.
#[tokio::test]
async fn every_number_on_a_three_revision_branch_resolves_on_the_remote() {
    let f = fixture().await;
    let first = seed_main(&f).await;
    let second = commit_on(f.store, f.repository, f.default_branch, first).await;
    let third = commit_on(f.store, f.repository, f.default_branch, second).await;
    publish(&f, f.default_branch, third).await;

    let mut resolved = Vec::new();
    for number in 1..=3 {
        let (status, events) = resolve_revision(
            &f,
            f.store,
            graph_globals_at(LoreBranchLocation::Remote),
            numbered(f.default_branch, number),
        )
        .await;
        resolved.push((status, revisions(&events)));
    }

    let at_remote = |revision, number| {
        (
            0,
            vec![(
                LoreBranchLocation::Remote,
                revision,
                number,
                f.default_branch,
            )],
        )
    };
    assert_eq!(
        resolved,
        [
            at_remote(first, 1),
            at_remote(second, 2),
            at_remote(third, 3)
        ]
    );
    f.close().await;
}

/// A number on a branch neither location holds fails `BranchNotFound`, and each location's
/// complete event reports no record and no error.
#[tokio::test]
async fn a_number_on_a_branch_no_location_holds_fails_branch_not_found() {
    let f = fixture().await;
    let missing = BranchId::from(uuid::Uuid::now_v7());

    let (status, events) =
        resolve_revision(&f, f.store, graph_globals(), numbered(missing, 1)).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 0, 0),
        ]
    );
    f.close().await;
}

/// A number past the latest at both locations fails `RevisionNotFound`. Each location holds the
/// branch without the revision, so its complete event reports no record and no error.
#[tokio::test]
async fn a_number_past_both_latests_fails_revision_not_found() {
    let f = fixture().await;
    seed_main(&f).await;

    let (status, events) =
        resolve_revision(&f, f.store, graph_globals(), numbered(f.default_branch, 2)).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 0, 0),
        ]
    );
    f.close().await;
}

/// A hash whose revision the local store alone holds, as committing through a handle without a
/// remote leaves it, resolves in the local store alone.
#[tokio::test]
async fn a_hash_held_only_locally_yields_one_local_record() {
    let f = fixture().await;
    let without_remote = handle_over_local_stores(&f, None);
    let revision = commit_on(
        without_remote,
        f.repository,
        f.default_branch,
        Hash::default(),
    )
    .await;
    close_handle(without_remote).await;

    let (status, events) = resolve_revision(&f, f.store, graph_globals(), by_hash(revision)).await;

    assert_eq!(status, 0);
    assert_eq!(
        revisions(&events),
        [(LoreBranchLocation::Local, revision, 1, f.default_branch)]
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    f.close().await;
}

/// The `remote` flag on a handle without a remote fails `NoRemote` before the call consults a
/// location, so it sends no record and no location complete event.
#[tokio::test]
async fn remote_flag_on_a_handle_without_a_remote_fails_no_remote() {
    let f = fixture_without_remote().await;

    let (status, events) = resolve_revision(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        latest(f.default_branch),
    )
    .await;

    let order = event_order(&events);
    assert_eq!(status, NoRemote::FFI_CODE);
    assert!(order.is_empty(), "expected no graph event, got {order:?}");
    f.close().await;
}

/// The `local` and `remote` flags together fail `InvalidArguments` before the call consults a
/// location, so it sends no record and no location complete event.
#[tokio::test]
async fn local_and_remote_flags_together_fail_invalid_arguments() {
    let f = fixture().await;

    let (status, events) = resolve_revision(
        &f,
        f.store,
        LoreGlobalArgs {
            local: 1,
            ..graph_globals_at(LoreBranchLocation::Remote)
        },
        latest(f.default_branch),
    )
    .await;

    let order = event_order(&events);
    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert!(order.is_empty(), "expected no graph event, got {order:?}");
    f.close().await;
}

/// Through a handle whose remote refuses connections, the remote's complete event carries the
/// failure with no record, the local store still resolves the latest, and the call fails with the
/// remote's code.
#[tokio::test]
async fn a_failure_at_the_remote_still_emits_the_local_record_and_fails_the_call() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let over_closed_port = handle_over_closed_port(&f);

    let (status, events) = resolve_revision(
        &f,
        over_closed_port,
        graph_globals(),
        latest(f.default_branch),
    )
    .await;
    close_handle(over_closed_port).await;

    let outcomes = location_outcomes(&events);
    let remote_code = outcomes.first().map_or(0, |&(_, _, code)| code);
    assert_ne!(remote_code, 0, "the remote fails, in {outcomes:?}");
    assert_eq!(status, remote_code);
    assert_eq!(
        event_order(&events),
        [
            (LoreBranchLocation::Remote, true),
            (LoreBranchLocation::Local, false),
            (LoreBranchLocation::Local, true),
        ]
    );
    assert_eq!(
        revisions(&events),
        [(LoreBranchLocation::Local, seeded, 1, f.default_branch)]
    );
    assert_eq!(
        outcomes,
        [
            (LoreBranchLocation::Remote, 0, remote_code),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    f.close().await;
}

/// The `local` flag consults the local store alone: one record and the local store's complete
/// event, and no complete event for the remote the handle has.
#[tokio::test]
async fn a_local_only_call_emits_one_location_complete_event() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;

    let (status, events) = resolve_revision(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        latest(f.default_branch),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        revisions(&events),
        [(LoreBranchLocation::Local, seeded, 1, f.default_branch)]
    );
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Local, 1, 0)]
    );
    f.close().await;
}

/// A remote that refuses connections fails each resolve verb at the remote with a connectivity
/// code, `Disconnected` or `NotConnected`, which the call completes with, not `Internal`.
#[tokio::test]
async fn an_unreachable_remote_reports_a_connectivity_code() {
    let f = fixture().await;
    let over_closed_port = handle_over_closed_port(&f);

    let revision = resolve_revision(
        &f,
        over_closed_port,
        graph_globals_at(LoreBranchLocation::Remote),
        latest(f.default_branch),
    )
    .await;
    let branch = resolve_branch(
        &f,
        over_closed_port,
        graph_globals_at(LoreBranchLocation::Remote),
        DEFAULT_BRANCH_NAME,
    )
    .await;
    close_handle(over_closed_port).await;

    for (verb, (status, events)) in [("revision_resolve", revision), ("branch_resolve", branch)] {
        assert!(
            [Disconnected::FFI_CODE, NotConnected::FFI_CODE].contains(&status),
            "{verb} completes with a connectivity code, not {status}"
        );
        assert_eq!(
            location_outcomes(&events),
            [(LoreBranchLocation::Remote, 0, status)],
            "{verb}"
        );
    }
    f.close().await;
}

/// A branch both locations hold resolves to the remote's record, then the local store's, which the
/// record of the instance on the branch follows. Each record carries the latest, stack, creator and
/// category its location holds, and the local one is divergent, as its create records it. Each
/// location's complete event follows its records and counts them.
#[tokio::test]
async fn branch_resolve_emits_remote_then_local_records_then_instances() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::personal_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let dir = TempDir::new("revision-graph-resolve-instance-");
    let instance = register_instance_at(&f, feature, seeded, &dir).await;

    let (status, events) = resolve_branch(&f, f.store, graph_globals(), "feature").await;

    let records: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranch(data) => Some((data.location, data.branch, None)),
            LoreEvent::RevisionGraphInstance(data) => {
                Some((data.location, data.branch, Some(data.instance_id)))
            }
            _ => None,
        })
        .collect();
    let fields: Vec<_> = branch_records(&events)
        .into_iter()
        .map(|record| {
            (
                record.location,
                record.latest,
                record.stack.as_slice(),
                record.creator.as_str(),
                record.category.as_str(),
                record.divergent,
            )
        })
        .collect();
    let stack = [LoreBranchPoint {
        branch: f.default_branch,
        revision: seeded,
    }];
    let category = branch::personal_category();
    assert_eq!(status, 0);
    assert_eq!(
        records,
        [
            (LoreBranchLocation::Remote, feature, None),
            (LoreBranchLocation::Local, feature, None),
            (LoreBranchLocation::Local, feature, Some(instance)),
        ]
    );
    assert_eq!(
        fields,
        [
            (
                LoreBranchLocation::Remote,
                seeded,
                &stack[..],
                CREATOR,
                category,
                0
            ),
            (
                LoreBranchLocation::Local,
                seeded,
                &stack[..],
                CREATOR,
                category,
                1
            ),
        ]
    );
    assert_eq!(
        event_order(&events),
        [
            (LoreBranchLocation::Remote, false),
            (LoreBranchLocation::Remote, true),
            (LoreBranchLocation::Local, false),
            (LoreBranchLocation::Local, false),
            (LoreBranchLocation::Local, true),
        ]
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 2, 0),
        ]
    );
    f.close().await;
}

/// Each location's record of a branch reports `protect` from that location's blob: the server's
/// protect sets it on the remote's record and leaves the local store's record clear.
#[tokio::test]
async fn branch_resolve_reports_protect_from_the_remote_blob() {
    let f = fixture().await;
    seed_main(&f).await;
    let guarded = create_branch_at_both_locations(
        &f,
        "guarded",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    on_server(&f, async |context| branch::protect(context, guarded).await)
        .await
        .expect("protects the branch on the server");

    let (status, events) = resolve_branch(&f, f.store, graph_globals(), "guarded").await;

    let protect: Vec<_> = branch_records(&events)
        .into_iter()
        .map(|record| (record.location, record.branch, record.protect))
        .collect();
    assert_eq!(status, 0);
    assert_eq!(
        protect,
        [
            (LoreBranchLocation::Remote, guarded, 1),
            (LoreBranchLocation::Local, guarded, 0),
        ]
    );
    f.close().await;
}

/// A branch the remote resolves stops resolving there once the server archives it: with the
/// `remote` flag the call fails `BranchNotFound`, and the remote's complete event reports no record
/// and no error.
#[tokio::test]
async fn branch_resolve_never_returns_an_archived_remote_branch() {
    let f = fixture().await;
    seed_main(&f).await;
    let retired = create_remote_branch(&f, "retired", f.default_branch, CREATOR).await;
    let (live_status, live) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        "retired",
    )
    .await;
    archive_on_server(&f, retired).await;

    let (status, events) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        "retired",
    )
    .await;

    let live_records: Vec<_> = branch_records(&live)
        .into_iter()
        .map(|record| record.branch)
        .collect();
    assert_eq!((live_status, live_records), (0, vec![retired]));
    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(event_order(&events), [(LoreBranchLocation::Remote, true)]);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, 0, 0)]
    );
    f.close().await;
}

/// A name finds its branch regardless of case at each location: `FEATURE` resolves the branch
/// created as `Feature` at the remote and in the local store, and each record carries the name as
/// created.
#[tokio::test]
async fn branch_resolve_compares_names_regardless_of_case_at_both_locations() {
    let f = fixture().await;
    seed_main(&f).await;
    let branch = create_branch_at_both_locations(
        &f,
        "Feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;

    let (status, events) = resolve_branch(&f, f.store, graph_globals(), "FEATURE").await;

    let named: Vec<_> = branch_records(&events)
        .into_iter()
        .map(|record| (record.location, record.branch, record.name.as_str()))
        .collect();
    assert_eq!(status, 0);
    assert_eq!(
        named,
        [
            (LoreBranchLocation::Remote, branch, "Feature"),
            (LoreBranchLocation::Local, branch, "Feature"),
        ]
    );
    f.close().await;
}
