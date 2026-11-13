// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What each read verb reads and writes in the local store, by the location it consults, and how a
//! failed local read ends that location: work at the remote touches no local store, a read verb
//! writes nothing to the local store, a block only the remote holds is absent from the local store's
//! answer, a failed local read fails the local location alone, the remote's error is the call's
//! where both locations fail, and invalid arguments end a call before it reaches any store.

use std::sync::atomic::Ordering;

use lore::revision_graph::LoreRevisionIdentifier;
use lore_base::error::InvalidArguments;
use lore_base::error::RevisionNotFound;
use lore_base::test_util::TempDir;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreBranchLocation;
use lore_storage::test_util::ImmutableCall;

use super::counting::open_counted_handle;
use super::support::*;
use crate::common::net_common::close_handle;

/// Asserts that a call, given by its status and events, succeeded with records at `location`
/// alone, which its one location complete event counts without an error.
fn assert_answered_at(location: LoreBranchLocation, (status, events): &(i32, Vec<LoreEvent>)) {
    let outcomes = location_outcomes(events);
    assert_eq!(*status, 0, "the call succeeds, with outcomes {outcomes:?}");
    assert!(
        matches!(outcomes[..], [(answered, records, 0)] if answered == location && records > 0),
        "expected records at {location:?} alone, got {outcomes:?}"
    );
}

/// Every read verb with the `remote` flag makes no call on the local stores: it loads, lists and
/// reads nothing there, and stores, swaps and puts nothing, while it reads the remote's blobs of a
/// branch the local store doesn't hold.
#[tokio::test]
async fn work_at_the_remote_reads_no_local_key_and_writes_nothing() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let remote_only = create_remote_branch(&f, "remote-only", f.default_branch, CREATOR).await;
    let published = publish_from_another_handle(&f, remote_only, seeded).await;
    let (counted, mutable, immutable) = open_counted_handle(&f, None);
    let gets = f.server_gets.load(Ordering::SeqCst);

    let remote = || graph_globals_at(LoreBranchLocation::Remote);
    let calls = [
        resolve_revision(&f, counted, remote(), latest(remote_only)).await,
        resolve_revision(&f, counted, remote(), numbered(remote_only, 2)).await,
        resolve_revision(&f, counted, remote(), by_hash(published)).await,
        resolve_branch(&f, counted, remote(), "remote-only").await,
        list_branches(&f, counted, remote(), true, "").await,
    ];
    close_handle(counted).await;

    for answered in &calls {
        assert_answered_at(LoreBranchLocation::Remote, answered);
    }
    assert!(f.server_gets.load(Ordering::SeqCst) > gets);
    assert_eq!((mutable.total(), immutable.total()), (0, 0));
    f.close().await;
}

/// Every read verb with the `local` flag writes nothing to the local store: it stores and swaps no
/// key, and puts, copies, obliterates and heals no content, while it reads branches, archived ones
/// and instances included, and revisions there.
#[tokio::test]
async fn a_read_verb_writes_nothing_locally() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let local = create_local_branch(&f, "local", f.default_branch, CREATOR).await;
    let archived = create_local_branch(&f, "archived", f.default_branch, CREATOR).await;
    archive_locally(&f, archived).await;
    let dir = TempDir::new("revision-graph-isolation-instance-");
    register_instance_at(&f, local, seeded, &dir).await;
    let (counted, mutable, immutable) = open_counted_handle(&f, None);

    let in_local_store = || graph_globals_at(LoreBranchLocation::Local);
    let calls = [
        resolve_revision(&f, counted, in_local_store(), latest(local)).await,
        resolve_revision(&f, counted, in_local_store(), numbered(local, 1)).await,
        resolve_revision(&f, counted, in_local_store(), by_hash(seeded)).await,
        resolve_branch(&f, counted, in_local_store(), "local").await,
        list_branches(&f, counted, in_local_store(), true, "").await,
    ];
    close_handle(counted).await;

    for answered in &calls {
        assert_answered_at(LoreBranchLocation::Local, answered);
    }
    let written = mutable.written();
    assert!(written.is_empty(), "the calls wrote {written:?}");
    assert_eq!(
        [
            ImmutableCall::Put,
            ImmutableCall::Copy,
            ImmutableCall::Obliterate,
            ImmutableCall::Heal
        ]
        .map(|call| immutable.count(call)),
        [0; 4]
    );
    f.close().await;
}

/// A revision only the remote holds is absent from the local store's answer: a `local` resolve of
/// its hash asks the local store, fetches nothing from the remote, and fails `RevisionNotFound`
/// with a local location complete event that reports no record and no error.
#[tokio::test]
async fn a_lazy_fetch_miss_counts_as_absent_locally() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let remote_only = publish_from_another_handle(&f, f.default_branch, seeded).await;
    let (counted, _, immutable) = open_counted_handle(&f, None);
    let gets = f.server_gets.load(Ordering::SeqCst);

    let (status, events) = resolve_revision(
        &f,
        counted,
        graph_globals_at(LoreBranchLocation::Local),
        by_hash(remote_only),
    )
    .await;
    close_handle(counted).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Local, 0, 0)]
    );
    assert!(immutable.count(ImmutableCall::Get) > 0);
    assert_eq!(f.server_gets.load(Ordering::SeqCst), gets);
    f.close().await;
}

/// A local payload read that fails fails the local location of a `branch_resolve` with no flag:
/// the remote's record and location complete event come first, the local location complete event
/// carries an error and no record, and the call's status is that error's code.
#[tokio::test]
async fn a_failed_local_read_fails_the_local_location() {
    let f = fixture().await;
    seed_main(&f).await;
    let both = create_branch_at_both_locations(
        &f,
        "both",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let (counted, _, immutable) = open_counted_handle(&f, None);
    immutable.fail_gets();

    let (status, events) = resolve_branch(&f, counted, graph_globals(), "both").await;
    close_handle(counted).await;

    let records: Vec<_> = branch_records(&events)
        .into_iter()
        .map(|record| (record.location, record.branch))
        .collect();
    assert_eq!(records, [(LoreBranchLocation::Remote, both)]);
    let outcomes = location_outcomes(&events);
    assert!(
        matches!(
            outcomes[..],
            [
                (LoreBranchLocation::Remote, 1, 0),
                (LoreBranchLocation::Local, 0, code),
            ] if code != 0 && code == status
        ),
        "expected the local location to fail the call, got status {status} and {outcomes:?}"
    );
    f.close().await;
}

/// Where the remote refuses connections and a local payload read fails, a `revision_resolve` of a
/// latest fails at both locations, and the call's status is the remote's code, not the local one.
#[tokio::test]
async fn when_both_locations_fail_complete_carries_the_remotes_code() {
    let f = fixture().await;
    seed_main(&f).await;
    let remote = closed_port_remote(&f);
    let (counted, _, immutable) = open_counted_handle(&f, Some(remote));
    immutable.fail_gets();

    let (status, events) =
        resolve_revision(&f, counted, graph_globals(), latest(f.default_branch)).await;
    close_handle(counted).await;

    let outcomes = location_outcomes(&events);
    assert!(
        matches!(
            outcomes[..],
            [
                (LoreBranchLocation::Remote, 0, remote_code),
                (LoreBranchLocation::Local, 0, local_code),
            ] if remote_code != 0
                && local_code != 0
                && remote_code != local_code
                && status == remote_code
        ),
        "expected the remote's code as the status, got status {status} and {outcomes:?}"
    );
    f.close().await;
}

/// An identifier naming both a hash and a branch, and an empty branch name, fail `InvalidArguments`
/// before the call reaches any store: no call on the local stores, no read at the remote and no
/// location complete event.
#[tokio::test]
async fn malformed_identifiers_and_invalid_names_touch_no_store() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let (counted, mutable, immutable) = open_counted_handle(&f, None);
    let gets = f.server_gets.load(Ordering::SeqCst);

    let hash_and_branch = LoreRevisionIdentifier {
        hash: seeded,
        branch: f.default_branch,
        revision_number: 0,
    };
    let calls = [
        resolve_revision(&f, counted, graph_globals(), hash_and_branch).await,
        resolve_branch(&f, counted, graph_globals(), "").await,
    ];
    close_handle(counted).await;

    for (status, events) in &calls {
        assert_eq!(*status, InvalidArguments::FFI_CODE);
        assert!(location_completes(events).is_empty());
    }
    assert_eq!((mutable.total(), immutable.total()), (0, 0));
    assert_eq!(f.server_gets.load(Ordering::SeqCst), gets);
    f.close().await;
}
