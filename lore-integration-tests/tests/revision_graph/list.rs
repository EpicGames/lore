// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `branch_list` at both locations: the records each location reports, their order and the location
//! complete events that end them, a branch two names map to, archived branches, the creator filter,
//! and the instance records that follow each local branch record.

use lore::revision_graph::branch_list::branch_list;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::instance::InstanceId;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreBranchPoint;

use super::support::*;

/// The events of a `branch_list` call on `f`'s storage handle with no location flag and the
/// arguments [`branch_list_args`] gives. Panics unless the call succeeds.
async fn list(f: &Fixture, include_archived: bool, creator: &str) -> Vec<LoreEvent> {
    events_of("listing the branches", |callback| {
        branch_list(
            graph_globals(),
            branch_list_args(f, f.store, include_archived, creator),
            callback,
        )
    })
    .await
}

/// `items`, sorted.
fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort_unstable();
    items
}

/// The branch, latest and archived flag of each branch record among `events` at `location`,
/// sorted.
fn records_at(events: &[LoreEvent], location: LoreBranchLocation) -> Vec<(BranchId, Hash, u8)> {
    sorted(
        branch_records(events)
            .into_iter()
            .filter(|record| record.location == location)
            .map(|record| (record.branch, record.latest, record.archived))
            .collect(),
    )
}

/// A branch record's location and branch, and the instance ids of the instance records that follow
/// it.
type InstanceGroup = (LoreBranchLocation, BranchId, Vec<InstanceId>);

/// The order of [`instance_groups`]: the remote's groups first, each location's by branch.
fn group_order((location, branch, _): &InstanceGroup) -> (bool, BranchId) {
    (*location == LoreBranchLocation::Local, *branch)
}

/// Each branch record among `events` as its location, its branch, and the sorted instance ids of
/// the instance records that follow it before the next branch record or location complete event,
/// in [`group_order`]. Panics on an instance record that follows no branch record, or names another
/// branch than the record it follows.
fn instance_groups(events: &[LoreEvent]) -> Vec<InstanceGroup> {
    let mut groups: Vec<InstanceGroup> = Vec::new();
    let mut open = false;
    for event in events {
        match event {
            LoreEvent::RevisionGraphBranch(data) => {
                groups.push((data.location, data.branch, Vec::new()));
                open = true;
            }
            LoreEvent::RevisionGraphInstance(data) => {
                let (_, branch, instances) = groups
                    .last_mut()
                    .filter(|_| open)
                    .expect("an instance record follows a branch record");
                assert_eq!(data.branch, *branch, "an instance record names its branch");
                instances.push(data.instance_id);
            }
            LoreEvent::RevisionGraphLocationComplete(_) => open = false,
            _ => {}
        }
    }
    for (_, _, instances) in &mut groups {
        instances.sort_unstable();
    }
    groups.sort_unstable_by_key(group_order);
    groups
}

/// Each location reports one record per branch it holds, the remote first, and a branch held at
/// both reports one record at each, with that location's latest and stack. Each location's complete
/// event follows its records and counts them.
#[tokio::test]
async fn lists_one_record_per_branch_per_location_remote_first() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    let both = create_branch_at_both_locations(
        &f,
        "both",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let remote_only = create_remote_branch(&f, "remote-only", f.default_branch, CREATOR).await;
    let local_only = create_local_branch(&f, "local-only", f.default_branch, CREATOR).await;
    let ahead = commit_on(f.store, f.repository, both, committed).await;
    let published = publish_from_another_handle(&f, remote_only, seeded).await;

    let events = list(&f, false, "").await;

    assert_eq!(
        records_at(&events, LoreBranchLocation::Remote),
        sorted(vec![
            (f.default_branch, seeded, 0),
            (both, seeded, 0),
            (remote_only, published, 0),
        ])
    );
    assert_eq!(
        records_at(&events, LoreBranchLocation::Local),
        sorted(vec![(both, ahead, 0), (local_only, committed, 0)])
    );
    let record_of_both = |location| {
        branch_records(&events)
            .into_iter()
            .find(|record| record.branch == both && record.location == location)
            .expect("the branch has a record at each location")
    };
    let main_at = |revision| {
        [LoreBranchPoint {
            branch: f.default_branch,
            revision,
        }]
    };
    let remote_both = record_of_both(LoreBranchLocation::Remote);
    assert_eq!(
        (
            remote_both.name.as_str(),
            remote_both.stack.as_slice(),
            remote_both.divergent
        ),
        ("both", &main_at(seeded)[..], 0)
    );
    assert_eq!(
        record_of_both(LoreBranchLocation::Local).stack.as_slice(),
        main_at(committed)
    );
    let order = |location, records| {
        std::iter::repeat_n((location, false), records).chain([(location, true)])
    };
    assert_eq!(
        event_order(&events),
        order(LoreBranchLocation::Remote, 3)
            .chain(order(LoreBranchLocation::Local, 2))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 3, 0),
            (LoreBranchLocation::Local, 2, 0),
        ]
    );
    f.close().await;
}

/// A branch two names map to on the server, as a store an older version wrote can hold it, gets one
/// `Remote` record, which the remote's complete event counts once.
#[tokio::test]
async fn a_remote_branch_two_names_map_to_is_listed_once() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let twice = create_remote_branch(&f, "twice", f.default_branch, CREATOR).await;
    on_server(&f, async |context| {
        branch::claim_name(context, "twice-again", Context::default(), twice).await
    })
    .await
    .expect("maps a second name to the branch");

    let events = list(&f, false, "").await;

    assert_eq!(
        records_at(&events, LoreBranchLocation::Remote),
        sorted(vec![(f.default_branch, seeded, 0), (twice, seeded, 0)])
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 2, 0),
            (LoreBranchLocation::Local, 0, 0),
        ]
    );
    f.close().await;
}

/// `include_archived` adds the branches archived at each location, archived and at the latest each
/// kept: the remote's as Lore Server reports them deleted, the local store's from the branch
/// metadata keys no live branch holds.
#[tokio::test]
async fn include_archived_adds_archived_branches_at_both_locations() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let remote_live = create_remote_branch(&f, "remote-live", f.default_branch, CREATOR).await;
    let remote_archived =
        create_remote_branch(&f, "remote-archived", f.default_branch, CREATOR).await;
    let local_live = create_local_branch(&f, "local-live", f.default_branch, CREATOR).await;
    let local_archived = create_local_branch(&f, "local-archived", f.default_branch, CREATOR).await;
    let kept = publish_from_another_handle(&f, remote_archived, seeded).await;
    archive_on_server(&f, remote_archived).await;
    archive_locally(&f, local_archived).await;

    let live = list(&f, false, "").await;
    let with_archived = list(&f, true, "").await;

    assert_eq!(
        records_at(&live, LoreBranchLocation::Remote),
        sorted(vec![
            (f.default_branch, seeded, 0),
            (remote_live, seeded, 0)
        ])
    );
    assert_eq!(
        records_at(&live, LoreBranchLocation::Local),
        [(local_live, seeded, 0)]
    );
    assert_eq!(
        records_at(&with_archived, LoreBranchLocation::Remote),
        sorted(vec![
            (f.default_branch, seeded, 0),
            (remote_live, seeded, 0),
            (remote_archived, kept, 1),
        ])
    );
    assert_eq!(
        records_at(&with_archived, LoreBranchLocation::Local),
        sorted(vec![(local_live, seeded, 0), (local_archived, seeded, 1)])
    );
    f.close().await;
}

/// A creator filter keeps, at each location, the branches whose creator equals it exactly: of a
/// branch created by `a` and one created by `A` at each location, filter `a` keeps one record at
/// each.
#[tokio::test]
async fn creator_filter_applies_at_both_locations() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let remote_lower = create_remote_branch(&f, "remote-lower", f.default_branch, "a").await;
    create_remote_branch(&f, "remote-upper", f.default_branch, "A").await;
    let local_lower = create_local_branch(&f, "local-lower", f.default_branch, "a").await;
    create_local_branch(&f, "local-upper", f.default_branch, "A").await;

    let events = list(&f, false, "a").await;

    assert_eq!(
        records_at(&events, LoreBranchLocation::Remote),
        [(remote_lower, seeded, 0)]
    );
    assert_eq!(
        records_at(&events, LoreBranchLocation::Local),
        [(local_lower, seeded, 0)]
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

/// Each local branch record is followed by exactly the instance records of the instances on its
/// branch, and each of the remote's records by none, that of a branch both locations hold
/// included. The local store's complete event counts the instance records with the branch records.
#[tokio::test]
async fn each_local_record_is_followed_by_its_instance_records() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let first = create_local_branch(&f, "first", f.default_branch, CREATOR).await;
    let second = create_local_branch(&f, "second", f.default_branch, CREATOR).await;
    let shared = create_branch_at_both_locations(
        &f,
        "shared",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let dirs = [(); 4].map(|()| TempDir::new("revision-graph-list-instance-"));
    let on_first = vec![
        register_instance_at(&f, first, seeded, &dirs[0]).await,
        register_instance_at(&f, first, seeded, &dirs[1]).await,
    ];
    let on_second = register_instance_at(&f, second, seeded, &dirs[2]).await;
    let on_shared = register_instance_at(&f, shared, seeded, &dirs[3]).await;

    let events = list(&f, false, "").await;

    let mut expected = vec![
        (LoreBranchLocation::Remote, f.default_branch, Vec::new()),
        (LoreBranchLocation::Remote, shared, Vec::new()),
        (LoreBranchLocation::Local, first, sorted(on_first)),
        (LoreBranchLocation::Local, second, vec![on_second]),
        (LoreBranchLocation::Local, shared, vec![on_shared]),
    ];
    expected.sort_unstable_by_key(group_order);
    assert_eq!(instance_groups(&events), expected);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 2, 0),
            (LoreBranchLocation::Local, 7, 0),
        ]
    );
    f.close().await;
}
