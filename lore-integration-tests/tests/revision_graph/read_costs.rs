// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What each read verb reads in the local store in one call with no location flag: its loads and
//! lists of each key type and its payload reads, counted on a handle over the fixture handle's
//! local stores. Each count is exact, a number walk's included: on a line without merges, the walk
//! reads one state per revision from the latest down to the number.

use lore::storage::handle::LoreStore;
use lore_base::error::BranchNotFound;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::KeyType;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreBranchLocation;
use lore_storage::test_util::ImmutableCall;
use lore_storage::test_util::StoreCall;

use super::counting::CountingImmutable;
use super::counting::CountingMutable;
use super::counting::open_counted_handle;
use super::support::*;
use crate::common::net_common::close_handle;

/// Mutable store calls of one kind, by the key type they name.
#[derive(Debug, Default, PartialEq, Eq)]
struct ByKeyType {
    /// On name mappings.
    branch_id: usize,
    /// On branch metadata keys.
    branch_metadata: usize,
    /// On the latest keys of branches.
    branch_latest_pointer: usize,
    /// On untyped keys: a branch's latest status and an instance's anchors.
    untyped: usize,
    /// On instance registrations.
    instance: usize,
}

impl ByKeyType {
    /// The calls of kind `call` that `mutable` counted.
    fn of(mutable: &CountingMutable, call: StoreCall) -> Self {
        let count = |key_type| mutable.count_for(call, key_type);
        Self {
            branch_id: count(KeyType::BranchId),
            branch_metadata: count(KeyType::BranchMetadata),
            branch_latest_pointer: count(KeyType::BranchLatestPointer),
            untyped: count(KeyType::Untyped),
            instance: count(KeyType::Instance),
        }
    }

    /// The calls of every key type.
    fn total(&self) -> usize {
        self.branch_id
            + self.branch_metadata
            + self.branch_latest_pointer
            + self.untyped
            + self.instance
    }
}

/// The calls one verb call made on the local stores.
#[derive(Debug, Default, PartialEq, Eq)]
struct LocalCalls {
    /// Its loads.
    loads: ByKeyType,
    /// Its lists.
    lists: ByKeyType,
    /// Its payload reads.
    gets: usize,
    /// Its other calls on either store: stores, compare-and-swaps, loads and lists of other key
    /// types, and content calls other than payload reads.
    others: usize,
}

impl LocalCalls {
    /// The calls `mutable` and `immutable` counted.
    fn of(mutable: &CountingMutable, immutable: &CountingImmutable) -> Self {
        let loads = ByKeyType::of(mutable, StoreCall::Load);
        let lists = ByKeyType::of(mutable, StoreCall::List);
        let gets = immutable.count(ImmutableCall::Get);
        let others = mutable.total() - loads.total() - lists.total() + immutable.total() - gets;
        Self {
            loads,
            lists,
            gets,
            others,
        }
    }
}

/// The status and events of the call `call` makes on a counted handle over the local stores of
/// `f`'s handle, with the fixture's remote, and the calls it made on those stores.
async fn counted<F: Future<Output = (i32, Vec<LoreEvent>)>>(
    f: &Fixture,
    call: impl FnOnce(LoreStore) -> F,
) -> (i32, Vec<LoreEvent>, LocalCalls) {
    let (store, mutable, immutable) = open_counted_handle(f, None);
    let (status, events) = call(store).await;
    let calls = LocalCalls::of(&mutable, &immutable);
    close_handle(store).await;
    (status, events, calls)
}

/// Creates a branch named `name` in the local store of `f`'s handle from the default branch, as
/// [`create_local_branch`] does, under the id derived from the name, then releases the name, so
/// that no name maps to the branch, as an older version's archive leaves one. Returns the id.
async fn create_unmapped_derived(f: &Fixture, name: &str) -> BranchId {
    let derived = branch::fallback_id(name);
    create_local_branch_as(
        f,
        derived,
        name,
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    in_client(branch::release_name(local_writer(f), name, derived))
        .await
        .expect("releases the derived branch's name");
    derived
}

/// Maps `name` to a new branch id in the local store of `f`'s handle, with no metadata or latest
/// under that id.
async fn map_name_without_metadata(f: &Fixture, name: &str) {
    in_client(branch::claim_name(
        local_writer(f),
        name,
        Context::default(),
        BranchId::from(uuid::Uuid::now_v7()),
    ))
    .await
    .expect("maps the name");
}

/// A `branch_resolve` of a name that maps to a branch, with no instance registered, loads the
/// name's mapping, the branch's metadata key, latest and latest status, reads the metadata blob,
/// and lists the registrations.
#[tokio::test]
async fn branch_resolve_local_costs_four_loads_and_one_blob() {
    let f = fixture().await;
    seed_main(&f).await;
    create_local_branch(&f, "feature", f.default_branch, CREATOR).await;

    let (status, events, calls) = counted(&f, |store| {
        resolve_branch(&f, store, graph_globals(), "feature")
    })
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    assert_eq!(
        calls,
        LocalCalls {
            loads: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                branch_latest_pointer: 1,
                untyped: 1,
                ..Default::default()
            },
            lists: ByKeyType {
                instance: 1,
                ..Default::default()
            },
            gets: 1,
            others: 0,
        }
    );
    f.close().await;
}

/// A `branch_resolve` of a name nothing maps to loads the mapping and the latest of the id derived
/// from the name, and nothing more where that id has no latest. Where it has one, the call also
/// loads the id's metadata key and reads its blob, which tell whether the branch left the name,
/// then reads the instances and the record as for a mapped name: 6 loads and 2 blobs in all. Of a
/// name mapped to a branch without local metadata, it reads the instances, then loads the metadata
/// key alone, and sends no record.
#[tokio::test]
async fn branch_resolve_pays_for_a_derived_id_and_for_a_mapping_without_metadata() {
    let f = fixture().await;
    seed_main(&f).await;
    let derived = create_unmapped_derived(&f, "legacy").await;
    map_name_without_metadata(&f, "stale").await;

    let (missing_status, missing_events, missing_calls) = counted(&f, |store| {
        resolve_branch(&f, store, graph_globals(), "missing")
    })
    .await;
    let (derived_status, derived_events, derived_calls) = counted(&f, |store| {
        resolve_branch(&f, store, graph_globals(), "legacy")
    })
    .await;
    let (stale_status, stale_events, stale_calls) = counted(&f, |store| {
        resolve_branch(&f, store, graph_globals(), "stale")
    })
    .await;

    for events in [&missing_events, &stale_events] {
        assert_eq!(
            location_outcomes(events),
            [
                (LoreBranchLocation::Remote, 0, 0),
                (LoreBranchLocation::Local, 0, 0),
            ]
        );
    }
    let derived_records: Vec<_> = branch_records(&derived_events)
        .into_iter()
        .map(|record| (record.location, record.branch))
        .collect();
    assert_eq!(derived_records, [(LoreBranchLocation::Local, derived)]);
    assert_eq!(
        [missing_status, derived_status, stale_status],
        [BranchNotFound::FFI_CODE, 0, BranchNotFound::FFI_CODE]
    );
    assert_eq!(
        missing_calls,
        LocalCalls {
            loads: ByKeyType {
                branch_id: 1,
                branch_latest_pointer: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    );
    assert_eq!(
        derived_calls,
        LocalCalls {
            loads: ByKeyType {
                branch_id: 1,
                branch_metadata: 1 + 1,
                branch_latest_pointer: 1 + 1,
                untyped: 1,
                ..Default::default()
            },
            lists: ByKeyType {
                instance: 1,
                ..Default::default()
            },
            gets: 1 + 1,
            others: 0,
        }
    );
    assert_eq!(
        stale_calls,
        LocalCalls {
            loads: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                ..Default::default()
            },
            lists: ByKeyType {
                instance: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    );
    f.close().await;
}

/// A `branch_resolve` reads the blob of every registration and the current branch of its instance,
/// and the current revision only of the instance on the branch it resolves, the one instance it
/// reports: with one instance on that branch and one on another, two registration blobs, two
/// current branches and one current revision.
#[tokio::test]
async fn branch_resolve_reads_the_current_revision_of_instances_on_its_branch_alone() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_local_branch(&f, "feature", f.default_branch, CREATOR).await;
    let other = create_local_branch(&f, "other", f.default_branch, CREATOR).await;
    let dirs = [(); 2].map(|()| TempDir::new("revision-graph-read-costs-resolve-instances-"));
    let on_feature = register_instance_at(&f, feature, seeded, &dirs[0]).await;
    register_instance_at(&f, other, seeded, &dirs[1]).await;

    let (status, events, calls) = counted(&f, |store| {
        resolve_branch(&f, store, graph_globals(), "feature")
    })
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 0, 0),
            (LoreBranchLocation::Local, 2, 0),
        ]
    );
    let instances: Vec<_> = instance_records(&events)
        .into_iter()
        .map(|record| record.instance_id)
        .collect();
    assert_eq!(instances, [on_feature]);
    assert_eq!(
        calls,
        LocalCalls {
            loads: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                branch_latest_pointer: 1,
                untyped: 1 + 2 + 1,
                ..Default::default()
            },
            lists: ByKeyType {
                instance: 1,
                ..Default::default()
            },
            gets: 1 + 2,
            others: 0,
        }
    );
    f.close().await;
}

/// With `include_archived`, a `branch_list` reads the metadata blob of each live branch once: the
/// live pass loads each live branch's metadata key, latest and latest status and reads its blob,
/// and the archived pass skips the live branches' metadata keys and reads the blob, latest and
/// latest status of the archived branch alone. It lists the name mappings, the branch metadata keys
/// and the registrations once each.
#[tokio::test]
async fn branch_list_with_include_archived_reads_live_blobs_once() {
    let f = fixture().await;
    seed_main(&f).await;
    for name in ["first", "second", "third"] {
        create_local_branch(&f, name, f.default_branch, CREATOR).await;
    }
    let archived = create_local_branch(&f, "archived", f.default_branch, CREATOR).await;
    archive_locally(&f, archived).await;

    let (status, events, calls) = counted(&f, |store| {
        list_branches(&f, store, graph_globals(), true, "")
    })
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 4, 0),
        ]
    );
    assert_eq!(
        calls,
        LocalCalls {
            loads: ByKeyType {
                branch_metadata: 3,
                branch_latest_pointer: 3 + 1,
                untyped: 3 + 1,
                ..Default::default()
            },
            lists: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                instance: 1,
                ..Default::default()
            },
            gets: 3 + 1,
            others: 0,
        }
    );
    f.close().await;
}

/// A `branch_list` reads the registered instances once for all the branches it lists: with an
/// instance on each of three branches, it lists the registrations once, reads each registration's
/// blob once, and loads each instance's current branch and current revision once, beside the
/// metadata key, blob, latest and latest status of each branch.
#[tokio::test]
async fn branch_list_lists_registered_instances_once() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let dirs = [(); 3].map(|()| TempDir::new("revision-graph-read-costs-list-instances-"));
    for (name, dir) in ["first", "second", "third"].into_iter().zip(&dirs) {
        let branch = create_local_branch(&f, name, f.default_branch, CREATOR).await;
        register_instance_at(&f, branch, seeded, dir).await;
    }

    let (status, events, calls) = counted(&f, |store| {
        list_branches(&f, store, graph_globals(), false, "")
    })
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 3 + 3, 0),
        ]
    );
    assert_eq!(
        calls,
        LocalCalls {
            loads: ByKeyType {
                branch_metadata: 3,
                branch_latest_pointer: 3,
                untyped: 3 + 3 + 3,
                ..Default::default()
            },
            lists: ByKeyType {
                branch_id: 1,
                instance: 1,
                ..Default::default()
            },
            gets: 3 + 3,
            others: 0,
        }
    );
    f.close().await;
}

/// A `revision_resolve` of a number loads the branch's latest once and reads one state per revision
/// it walks, from the latest down to the number, which on a line of three revisions is exactly the
/// latest's number less the number plus one, then the found revision's metadata.
#[tokio::test]
async fn revision_resolve_number_walks_one_state_per_step() {
    let f = fixture().await;
    let first = seed_main(&f).await;
    let second = commit_on(f.store, f.repository, f.default_branch, first).await;
    let line = [
        first,
        second,
        commit_on(f.store, f.repository, f.default_branch, second).await,
    ];

    for (index, &revision) in line.iter().enumerate() {
        let number = index as u64 + 1;
        let (status, events, calls) = counted(&f, |store| {
            resolve_revision(
                &f,
                store,
                graph_globals(),
                numbered(f.default_branch, number),
            )
        })
        .await;

        assert_eq!(status, 0, "number {number} resolves");
        let local: Vec<_> = revision_records(&events)
            .into_iter()
            .filter(|record| record.location == LoreBranchLocation::Local)
            .map(|record| (record.revision_hash, record.revision_number))
            .collect();
        assert_eq!(local, [(revision, number)]);
        let walked = line.len() - index;
        assert_eq!(
            calls,
            LocalCalls {
                loads: ByKeyType {
                    branch_latest_pointer: 1,
                    ..Default::default()
                },
                gets: walked + 1,
                ..Default::default()
            },
            "number {number}"
        );
    }
    f.close().await;
}

/// A `revision_resolve` of a branch's latest loads the latest and reads its state and metadata;
/// where the store holds no latest of the branch, it also loads the branch's metadata key, which
/// tells a branch without a latest from one the store doesn't hold, and reads nothing; and of a
/// hash it reads the state and metadata alone.
#[tokio::test]
async fn revision_resolve_latest_and_hash_read_a_state_and_its_metadata() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let unknown = BranchId::from(uuid::Uuid::now_v7());

    let (latest_status, latest_events, latest_calls) = counted(&f, |store| {
        resolve_revision(&f, store, graph_globals(), latest(f.default_branch))
    })
    .await;
    let (absent_status, _, absent_calls) = counted(&f, |store| {
        resolve_revision(&f, store, graph_globals(), latest(unknown))
    })
    .await;
    let (hash_status, hash_events, hash_calls) = counted(&f, |store| {
        resolve_revision(&f, store, graph_globals(), by_hash(seeded))
    })
    .await;

    for events in [&latest_events, &hash_events] {
        assert_eq!(
            location_outcomes(events),
            [
                (LoreBranchLocation::Remote, 1, 0),
                (LoreBranchLocation::Local, 1, 0),
            ]
        );
    }
    assert_eq!(
        [latest_status, absent_status, hash_status],
        [0, BranchNotFound::FFI_CODE, 0]
    );
    assert_eq!(
        latest_calls,
        LocalCalls {
            loads: ByKeyType {
                branch_latest_pointer: 1,
                ..Default::default()
            },
            gets: 2,
            ..Default::default()
        }
    );
    assert_eq!(
        absent_calls,
        LocalCalls {
            loads: ByKeyType {
                branch_metadata: 1,
                branch_latest_pointer: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    );
    assert_eq!(
        hash_calls,
        LocalCalls {
            gets: 2,
            ..Default::default()
        }
    );
    f.close().await;
}
