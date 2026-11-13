// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::stream;
use lore::interface::LoreGlobalArgs;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::branch_list::LoreRevisionGraphBranchListArgs;
use lore::revision_graph::branch_list::RECORDS_IN_FLIGHT;
use lore::revision_graph::branch_list::branch_list;
use lore::revision_graph::branch_list::build_records;
use lore::revision_graph::branch_list::one_record_per_branch;
use lore::revision_graph::branch_list::remote_record_builder;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::Disconnected;
use lore_base::lore_spawn;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_error_set::Internal;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::event::LoreCompleteEventData;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_graph::LoreRevisionGraphInstanceEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphLocationCompleteEventData;
use lore_revision::instance::InstanceId;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_revision::test_util::BranchStep;
use lore_revision::test_util::step_branch_writes;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::StoreCall;
use lore_transport::BranchRecord;
use tokio::sync::Notify;
use tokio::sync::Semaphore;

use crate::call::test_support::recording_callback;
use crate::call::test_support::take_without_log_lines;
use crate::revision_graph::support::CATEGORY;
use crate::revision_graph::support::CREATED;
use crate::revision_graph::support::NamingRevision;
use crate::revision_graph::support::QueryAnswer;
use crate::revision_graph::support::archive;
use crate::revision_graph::support::branch_records;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::counting_reads;
use crate::revision_graph::support::create_in_store;
use crate::revision_graph::support::handle_failing;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::location_completes;
use crate::revision_graph::support::location_outcomes;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::revision_graph::support::partial_metadata;
use crate::revision_graph::support::register_live_instance;
use crate::revision_graph::support::store_corrupt_metadata;
use crate::revision_graph::support::store_metadata;
use crate::revision_graph::support::store_with_remote;
use crate::revision_graph::support::with_mutable_store;
use crate::revision_graph::support::with_stores;

/// The `id` every call in this file passes.
const CALL_ID: u64 = 26;

/// Maps the name `nameless` to a new branch in the local store of `store` whose metadata blob holds
/// its id and a category, and no name or creator.
async fn store_nameless_branch(store: &StoreInternal, repository: Partition) {
    let nameless = new_branch();
    let write = local_write_context(store, repository);
    in_setup_context(async {
        store_metadata(
            &write,
            nameless,
            partial_metadata(Some(nameless), &[(branch::CATEGORY, CATEGORY)]),
        )
        .await;
        branch::claim_name(write.clone(), "nameless", Context::default(), nameless)
            .await
            .expect("maps a name to the branch");
    })
    .await;
}

/// Runs `branch_list` on `store` for `repository`, and returns its status and the events it sent
/// other than log lines.
async fn list(
    store: LoreStore,
    repository: Partition,
    include_archived: bool,
    creator: &str,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_list(
        LoreGlobalArgs::default(),
        LoreRevisionGraphBranchListArgs {
            id: CALL_ID,
            store,
            repository,
            include_archived: u8::from(include_archived),
            creator: LoreString::from(creator),
        },
        callback,
    )
    .await;
    (status, take_without_log_lines(&events))
}

/// The branches of the branch records [`list`] reports, for a call that succeeds.
async fn listed_branches(
    store: LoreStore,
    repository: Partition,
    include_archived: bool,
    creator: &str,
) -> HashSet<BranchId> {
    let (status, events) = list(store, repository, include_archived, creator).await;
    assert_eq!(status, 0);
    branch_records(&events)
        .into_iter()
        .map(|record| record.branch)
        .collect()
}

/// The branch records' branches, each with the instance records that follow it before the next
/// branch record, in order. Fails where an instance record names another branch than the record
/// it follows, or follows none.
fn groups(events: &[LoreEvent]) -> Vec<(BranchId, Vec<&LoreRevisionGraphInstanceEventData>)> {
    let mut groups: Vec<(BranchId, Vec<&LoreRevisionGraphInstanceEventData>)> = Vec::new();
    for event in events {
        match event {
            LoreEvent::RevisionGraphBranch(data) => groups.push((data.branch, Vec::new())),
            LoreEvent::RevisionGraphInstance(data) => {
                let (branch, instances) = groups
                    .last_mut()
                    .expect("an instance record follows a branch record");
                assert_eq!(data.branch, *branch, "an instance record names its branch");
                instances.push(data);
            }
            _ => {}
        }
    }
    groups
}

/// Each group of `events` as its branch and the ids of its instances, keyed by branch.
fn instance_ids(events: &[LoreEvent]) -> HashMap<BranchId, Vec<InstanceId>> {
    let groups = groups(events);
    let by_branch: HashMap<BranchId, Vec<InstanceId>> = groups
        .iter()
        .map(|(branch, instances)| {
            (
                *branch,
                instances.iter().map(|data| data.instance_id).collect(),
            )
        })
        .collect();
    assert_eq!(by_branch.len(), groups.len(), "one record per branch");
    by_branch
}

/// Asserts that `events` end with the local store's location complete event, after every record,
/// then `Complete` and `End`, and returns that event.
fn local_complete(events: &[LoreEvent]) -> &LoreRevisionGraphLocationCompleteEventData {
    let [
        ..,
        LoreEvent::RevisionGraphLocationComplete(data),
        LoreEvent::Complete(_),
        LoreEvent::End(_),
    ] = events
    else {
        panic!("expected a location complete event, then Complete and End");
    };
    assert_eq!(
        location_completes(events).len(),
        1,
        "one location consulted"
    );
    assert_eq!(data.id, CALL_ID);
    assert_eq!(data.location, LoreBranchLocation::Local);
    data
}

/// The `Complete` event that ends `events`, before `End`.
fn call_complete(events: &[LoreEvent]) -> &LoreCompleteEventData {
    let [.., LoreEvent::Complete(data), LoreEvent::End(_)] = events else {
        panic!("expected Complete, then End");
    };
    data
}

/// Each live branch is listed once with its record, and the instance record of the instance on
/// it follows that record.
#[tokio::test]
async fn lists_live_local_branches_with_their_instances() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (alpha, beta) = (new_branch(), new_branch());
    let dir = TempDir::new("graph-branch-list-instances-");
    create_in_store(&store, repository, alpha, "alpha", "ana", hash(2)).await;
    create_in_store(&store, repository, beta, "beta", "ana", hash(3)).await;
    let instance = in_setup_context(register_live_instance(
        &store,
        repository,
        alpha,
        hash(2),
        &dir,
    ))
    .await;

    let (status, events) = list(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        instance_ids(&events),
        HashMap::from([(alpha, vec![instance]), (beta, Vec::new())])
    );
    for record in branch_records(&events) {
        assert_eq!(record.id, CALL_ID);
        assert_eq!(record.location, LoreBranchLocation::Local);
        assert_eq!(record.archived, 0);
        let (name, latest) = if record.branch == alpha {
            ("alpha", hash(2))
        } else {
            ("beta", hash(3))
        };
        assert_eq!(record.name.as_bytes(), name.as_bytes());
        assert_eq!(record.latest, latest);
        assert_eq!(record.creator.as_bytes(), b"ana");
    }
    let groups = groups(&events);
    let (_, on_alpha) = groups
        .iter()
        .find(|(branch, _)| *branch == alpha)
        .expect("alpha is listed");
    let [data] = on_alpha.as_slice() else {
        panic!("expected one instance record on alpha");
    };
    assert_eq!(data.id, CALL_ID);
    assert_eq!(data.location, LoreBranchLocation::Local);
    assert_eq!(data.revision, hash(2));
    assert_eq!(data.stale, 0);
}

/// A branch that two names map to, as a store an older client wrote can hold, is listed once,
/// with its instance records.
#[tokio::test]
async fn a_branch_two_names_map_to_is_listed_once() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let dir = TempDir::new("graph-branch-list-two-names-");
    create_in_store(&store, repository, branch, "first", "ana", hash(2)).await;
    let write = local_write_context(&store, repository);
    let instance = in_setup_context(async {
        branch::claim_name(write.clone(), "second", Context::default(), branch)
            .await
            .expect("maps a second name to the branch");
        register_live_instance(&store, repository, branch, hash(2), &dir).await
    })
    .await;

    let (status, events) = list(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        instance_ids(&events),
        HashMap::from([(branch, vec![instance])])
    );
    assert_eq!(local_complete(&events).records, 2);
}

/// The creator filter keeps the branches whose creator equals it, byte for byte, at both the live
/// and the archived pass, and an empty filter keeps every branch.
#[tokio::test]
async fn creator_filter_is_exact_and_case_sensitive() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let [live_ana, live_upper, archived_ana, archived_upper] = [(); 4].map(|()| new_branch());
    for (branch, name, creator) in [
        (live_ana, "live-ana", "ana"),
        (live_upper, "live-upper", "Ana"),
        (archived_ana, "archived-ana", "ana"),
        (archived_upper, "archived-upper", "Ana"),
    ] {
        create_in_store(&store, repository, branch, name, creator, hash(2)).await;
    }
    archive(&store, repository, archived_ana, "archived-ana").await;
    archive(&store, repository, archived_upper, "archived-upper").await;

    let live = listed_branches(handle, repository, false, "ana").await;
    let with_archived = listed_branches(handle, repository, true, "ana").await;
    let unfiltered = listed_branches(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(live, HashSet::from([live_ana]));
    assert_eq!(with_archived, HashSet::from([live_ana, archived_ana]));
    assert_eq!(unfiltered, HashSet::from([live_ana, live_upper]));
}

/// A branch the creator filter leaves out costs no load of its latest or its latest status, in the
/// live pass or the archived pass, while a branch it keeps costs one of each.
#[tokio::test]
async fn a_filtered_out_branch_costs_no_latest_load() {
    let store = in_memory_for_tests("graph-branch-list-filtered-loads").await;
    let repository = new_repository();
    let [kept, live_left_out, archived_left_out] = [(); 3].map(|()| new_branch());
    create_in_store(&store, repository, kept, "kept", "ana", hash(2)).await;
    create_in_store(&store, repository, live_left_out, "live", "bob", hash(3)).await;
    create_in_store(
        &store,
        repository,
        archived_left_out,
        "archived",
        "bob",
        hash(4),
    )
    .await;
    archive(&store, repository, archived_left_out, "archived").await;
    let loaded = Arc::new(Mutex::new(Vec::new()));
    let observed = HookedStore::wrap(
        store.mutable.clone(),
        {
            let loaded = loaded.clone();
            move |call, key, _| {
                if call == StoreCall::Load {
                    loaded.lock().unwrap().push(key);
                }
                Ok(())
            }
        },
        None,
    );
    let handle = handle::register(Arc::new(with_mutable_store(&store, observed)));

    let listed = listed_branches(handle, repository, true, "ana").await;
    close_handle(handle).await;

    assert_eq!(listed, HashSet::from([kept]));
    let salt = local_read_context(&store, repository).salt();
    let latest_loads = |branch| {
        let keys = [branch::LATEST, branch::LATEST_STATUS]
            .map(|function| branch::mutable_key(salt, function, repository, branch).0);
        let loaded = loaded.lock().unwrap();
        keys.map(|key| loaded.iter().filter(|loaded| **loaded == key).count())
    };
    assert_eq!(latest_loads(kept), [1, 1]);
    assert_eq!(latest_loads(live_left_out), [0, 0]);
    assert_eq!(latest_loads(archived_left_out), [0, 0]);
}

/// With `include_archived` and a creator filter, each metadata blob is read once: the archived
/// pass skips the blob of a live branch the filter leaves out, as it skips a listed one's.
#[tokio::test]
async fn the_archived_pass_reads_no_blob_the_live_pass_read() {
    let store = in_memory_for_tests("graph-branch-list-filtered-reads").await;
    let repository = new_repository();
    let [kept, left_out, archived] = [(); 3].map(|()| new_branch());
    create_in_store(&store, repository, kept, "kept", "ana", hash(2)).await;
    create_in_store(&store, repository, left_out, "left-out", "bob", hash(3)).await;
    create_in_store(&store, repository, archived, "archived", "ana", hash(4)).await;
    archive(&store, repository, archived, "archived").await;
    let read = local_read_context(&store, repository);
    let mut blobs = Vec::new();
    for branch in [kept, left_out, archived] {
        let blob = in_setup_context(branch::mutable_load(read.clone(), branch::METADATA, branch))
            .await
            .expect("loads the metadata key");
        blobs.push(blob);
    }
    let (counted, reads) = counting_reads(store.immutable.clone());
    let handle = handle::register(Arc::new(with_stores(
        &store,
        counted,
        store.mutable.clone(),
    )));

    let listed = listed_branches(handle, repository, true, "ana").await;
    close_handle(handle).await;

    assert_eq!(listed, HashSet::from([kept, archived]));
    let reads: Vec<usize> = blobs.into_iter().map(|blob| reads.of(blob)).collect();
    assert_eq!(reads, [1, 1, 1]);
}

/// A branch the creator filter leaves out costs no record build, so a blob without a name or a
/// creator, whose record fails the location unfiltered, adds no record and the location succeeds.
#[tokio::test]
async fn a_filtered_out_branch_without_a_name_is_left_out() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    store_nameless_branch(&store, repository).await;

    let (status, events) = list(handle, repository, false, "ana").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert!(branch_records(&events).is_empty());
    let complete = local_complete(&events);
    assert_eq!((complete.records, complete.error.error_code), (0, 0));
}

/// The call starts a builder only for a remote record whose creator equals the filter, so a remote
/// that ignores the filter yields the records one that honors it does. No builder is polled.
#[tokio::test]
async fn remote_records_are_filtered_when_the_remote_ignores_creator() {
    let store = in_memory_for_tests("graph-branch-list-remote-filter").await;
    let ctx = local_read_context(&store, new_repository());
    let record = |creator: &str| BranchRecord {
        id: new_branch(),
        creator: creator.to_owned(),
        ..Default::default()
    };

    assert!(remote_record_builder(Some("a"), &ctx, record("A")).is_none());
    assert!(remote_record_builder(Some("a"), &ctx, record("a")).is_some());
    assert!(remote_record_builder(None, &ctx, record("A")).is_some());
}

/// The remote's records keep one record per branch, though the copies of a branch's record are
/// apart in the list, as the remote sends its records in the order its reads finish.
#[test]
fn the_remote_records_keep_one_per_branch_wherever_its_copies_fall() {
    let (a, b) = (new_branch(), new_branch());
    let record = |id| BranchRecord {
        id,
        ..Default::default()
    };

    let mut kept: Vec<BranchId> = one_record_per_branch(vec![record(a), record(b), record(a)])
        .into_iter()
        .map(|record| record.id)
        .collect();

    kept.sort_unstable();
    let mut expected = vec![a, b];
    expected.sort_unstable();
    assert_eq!(kept, expected);
}

/// With `include_archived`, an archived branch is listed once, archived and at the latest it
/// kept, beside the live branch, whose metadata key the archived pass lists too.
#[tokio::test]
async fn include_archived_adds_archived_branches_once() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (live, archived) = (new_branch(), new_branch());
    create_in_store(&store, repository, live, "live", "ana", hash(4)).await;
    create_in_store(&store, repository, archived, "archived", "ana", hash(5)).await;
    archive(&store, repository, archived, "archived").await;

    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let records = branch_records(&events);
    assert_eq!(records.len(), 2, "one record per branch");
    for record in records {
        if record.branch == archived {
            assert_eq!((record.archived, record.latest), (1, hash(5)));
            assert_eq!(record.name.as_bytes(), b"archived");
        } else {
            assert_eq!(record.branch, live);
            assert_eq!((record.archived, record.latest), (0, hash(4)));
        }
    }
    assert_eq!(local_complete(&events).records, 2);
}

/// Without `include_archived`, an archived branch, whose name no longer maps to it, has no record.
#[tokio::test]
async fn an_archived_name_costs_no_record() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (live, archived) = (new_branch(), new_branch());
    create_in_store(&store, repository, live, "live", "ana", hash(4)).await;
    create_in_store(&store, repository, archived, "archived", "ana", hash(5)).await;
    archive(&store, repository, archived, "archived").await;

    let (status, events) = list(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let branches: Vec<BranchId> = branch_records(&events)
        .into_iter()
        .map(|record| record.branch)
        .collect();
    assert_eq!(branches, [live]);
    assert_eq!(local_complete(&events).records, 1);
}

/// A metadata key whose value no live record holds, but whose blob names a live branch, as the
/// branch's key holds once its metadata changes after its live record is read, adds no archived
/// record of that branch.
#[tokio::test]
async fn an_archived_entry_naming_a_live_branch_is_left_out() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (live, elsewhere) = (new_branch(), new_branch());
    create_in_store(&store, repository, live, "live", "ana", hash(4)).await;
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        let created = branch::mutable_load(write.clone(), branch::METADATA, live)
            .await
            .expect("loads the metadata key");
        branch::protect(write.clone(), live)
            .await
            .expect("protects the branch");
        branch::mutable_store_metadata(write.clone(), elsewhere, created)
            .await
            .expect("stores the branch's earlier metadata under another key");
    })
    .await;

    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let [record] = branch_records(&events)[..] else {
        panic!("expected one branch record");
    };
    assert_eq!(
        (record.branch, record.archived, record.protect),
        (live, 0, 1)
    );
}

/// A metadata key whose blob names a live branch whose own blob the live pass missed adds no
/// archived record of that branch: a name maps to it, so it isn't archived.
#[tokio::test]
async fn an_archived_entry_naming_a_live_branch_without_a_record_is_left_out() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (live, elsewhere) = (new_branch(), new_branch());
    create_in_store(&store, repository, live, "live", "ana", hash(4)).await;
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        let created = branch::mutable_load(write.clone(), branch::METADATA, live)
            .await
            .expect("loads the metadata key");
        branch::mutable_store_metadata(write.clone(), elsewhere, created)
            .await
            .expect("stores the branch's metadata under another key");
        branch::mutable_store_metadata(write.clone(), live, hash(9))
            .await
            .expect("points the branch's metadata key at a blob the store doesn't hold");
    })
    .await;

    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert!(branch_records(&events).is_empty());
    assert_eq!(local_complete(&events).records, 0);
}

/// With `include_archived`, a metadata key that no name maps to and that names a blob the local
/// store doesn't hold is a miss: no record, and the location succeeds.
#[tokio::test]
async fn an_archived_entry_whose_blob_is_missing_is_skipped() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    in_setup_context(branch::mutable_store_metadata(
        local_write_context(&store, repository),
        new_branch(),
        hash(9),
    ))
    .await
    .expect("stores the metadata key");

    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert!(branch_records(&events).is_empty());
    let complete = local_complete(&events);
    assert_eq!((complete.records, complete.error.error_code), (0, 0));
}

/// With `include_archived`, a metadata key that no name maps to and whose blob holds a name and no
/// branch id names no branch: no record, and the location succeeds.
#[tokio::test]
async fn an_archived_entry_whose_blob_names_no_branch_is_skipped() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    in_setup_context(store_metadata(
        &write,
        new_branch(),
        partial_metadata(None, &[(branch::NAME, "idless")]),
    ))
    .await;

    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert!(branch_records(&events).is_empty());
    let complete = local_complete(&events);
    assert_eq!((complete.records, complete.error.error_code), (0, 0));
}

/// With `include_archived`, a metadata key that no name maps to and that names bytes that aren't a
/// metadata blob fails the local store's location and the call with `Internal`, rather than
/// reading as a miss.
#[tokio::test]
async fn a_corrupt_archived_blob_fails_the_local_location() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    in_setup_context(store_corrupt_metadata(&write, new_branch())).await;

    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(status, Internal::FFI_CODE);
    assert_eq!(local_complete(&events).error.error_code, Internal::FFI_CODE);
    let complete = call_complete(&events);
    assert_eq!(
        (complete.status, complete.error.error_code),
        (Internal::FFI_CODE, Internal::FFI_CODE)
    );
}

/// A create that loses its name claim swaps its metadata key back to zero, so the archived pass
/// finds no branch metadata for it.
#[tokio::test]
async fn a_lost_create_leaves_no_archived_record() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (winner, loser) = (new_branch(), new_branch());
    create_in_store(&store, repository, winner, "shared", "ana", hash(2)).await;
    let lost = in_setup_context(branch::create_record(
        local_write_context(&store, repository),
        loser,
        "shared",
        CATEGORY,
        "ana",
        CREATED,
        Vec::new(),
        hash(3),
        BranchLatestStatus::Convergent,
        Context::default(),
    ))
    .await;
    assert!(
        lost.as_ref()
            .is_err_and(|err| err.is_branch_already_exists()),
        "the second create loses its claim: {lost:?}"
    );

    let listed = listed_branches(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(listed, HashSet::from([winner]));
}

/// A create stores its metadata key before it claims its name, so with `include_archived` a branch
/// whose create stopped between the two lists as archived, the known limit the proposal's "Branch
/// list" records, and lists live once the create has claimed its name.
#[tokio::test]
async fn a_create_that_has_not_claimed_its_name_lists_as_archived() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let (reached_tx, mut reached) = tokio::sync::mpsc::unbounded_channel();
    let resume = Arc::new(Notify::new());
    let create = lore_spawn!(in_setup_context(step_branch_writes(
        reached_tx,
        resume.clone(),
        branch::create_record(
            local_write_context(&store, repository),
            branch,
            "pending",
            CATEGORY,
            "ana",
            CREATED,
            Vec::new(),
            hash(3),
            BranchLatestStatus::Convergent,
            Context::default(),
        ),
    )));
    assert_eq!(reached.recv().await, Some(BranchStep::CreateMetadataStored));

    let (status, events) = list(handle, repository, true, "").await;

    assert_eq!(status, 0);
    let listed: Vec<(BranchId, u8)> = branch_records(&events)
        .into_iter()
        .map(|record| (record.branch, record.archived))
        .collect();
    assert_eq!(listed, [(branch, 1)]);
    resume.notify_one();
    assert_eq!(reached.recv().await, Some(BranchStep::CreateNameClaimed));
    resume.notify_one();
    create
        .await
        .expect("the create task ends")
        .expect("the create claims its name");
    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;
    assert_eq!(status, 0);
    let listed: Vec<(BranchId, u8)> = branch_records(&events)
        .into_iter()
        .map(|record| (record.branch, record.archived))
        .collect();
    assert_eq!(listed, [(branch, 0)]);
}

/// With `include_archived`, the instance record of an instance on an archived branch follows that
/// branch's record.
#[tokio::test]
async fn an_archived_local_record_is_followed_by_its_instance_records() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (live, archived) = (new_branch(), new_branch());
    let dir = TempDir::new("graph-branch-list-archived-instance-");
    create_in_store(&store, repository, live, "live", "ana", hash(4)).await;
    create_in_store(&store, repository, archived, "archived", "ana", hash(5)).await;
    let instance = in_setup_context(register_live_instance(
        &store,
        repository,
        archived,
        hash(5),
        &dir,
    ))
    .await;
    archive(&store, repository, archived, "archived").await;

    let (status, events) = list(handle, repository, true, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        instance_ids(&events),
        HashMap::from([(live, Vec::new()), (archived, vec![instance])])
    );
    let archived_record = branch_records(&events)
        .into_iter()
        .find(|record| record.branch == archived)
        .expect("the archived branch is listed");
    assert_eq!(archived_record.archived, 1);
    assert_eq!(local_complete(&events).records, 3);
}

/// The local store's location complete event follows its records and counts the branch and the
/// instance records alike.
#[tokio::test]
async fn the_location_complete_event_counts_branch_and_instance_records() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (first, second) = (new_branch(), new_branch());
    let dir = TempDir::new("graph-branch-list-count-");
    create_in_store(&store, repository, first, "first", "ana", hash(2)).await;
    create_in_store(&store, repository, second, "second", "ana", hash(3)).await;
    in_setup_context(register_live_instance(
        &store,
        repository,
        first,
        hash(2),
        &dir,
    ))
    .await;

    let (status, events) = list(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let complete = local_complete(&events);
    assert_eq!(complete.records, 3);
    assert_eq!(complete.error.error_code, 0);
}

/// A branch whose record can't be built, here one whose metadata blob holds no name, fails the
/// call at the local store with `Internal`: its location complete event carries that error and
/// counts the records sent before it.
#[tokio::test]
async fn a_record_that_fails_fails_the_local_location() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    create_in_store(&store, repository, new_branch(), "named", "ana", hash(2)).await;
    store_nameless_branch(&store, repository).await;

    let (status, events) = list(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(status, Internal::FFI_CODE);
    let complete = local_complete(&events);
    assert_eq!(complete.error.error_code, Internal::FFI_CODE);
    assert_eq!(complete.records, branch_records(&events).len() as u64);
}

/// A failure at the remote, here a list the remote refuses, fails the call with the remote's error
/// though the local store's record follows: the remote's location complete event carries the error
/// and no record.
#[tokio::test]
async fn a_failure_at_the_remote_fails_the_call_after_the_local_record() {
    let repository = new_repository();
    let store = store_with_remote(repository, NamingRevision::new(QueryAnswer::NotFound)).await;
    let feature = new_branch();
    create_in_store(&store, repository, feature, "feature", "ana", hash(2)).await;
    let handle = handle::register(Arc::new(store));

    let (status, events) = list(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(status, Internal::FFI_CODE);
    let [
        LoreEvent::RevisionGraphLocationComplete(_),
        LoreEvent::RevisionGraphBranch(record),
        LoreEvent::RevisionGraphLocationComplete(_),
        LoreEvent::Complete(_),
        LoreEvent::End(_),
    ] = &events[..]
    else {
        panic!("expected the remote's location complete event, then the local record and its own");
    };
    assert_eq!(
        (record.location, record.branch),
        (LoreBranchLocation::Local, feature)
    );
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, Internal::FFI_CODE),
            (LoreBranchLocation::Local, 1, 0)
        ]
    );
}

/// Where both locations fail, the call fails with the remote's error, and each location's complete
/// event carries its own.
#[tokio::test]
async fn when_both_locations_fail_the_call_carries_the_remotes_error() {
    let repository = new_repository();
    let store = store_with_remote(repository, NamingRevision::new(QueryAnswer::NotFound)).await;
    create_in_store(&store, repository, new_branch(), "feature", "ana", hash(2)).await;
    let handle = handle_failing(&store, |call, _, key_type| {
        call == StoreCall::List && key_type == KeyType::Instance
    });

    let (status, events) = list(handle, repository, false, "").await;
    close_handle(handle).await;

    assert_eq!(status, Internal::FFI_CODE);
    assert!(branch_records(&events).is_empty());
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, Internal::FFI_CODE),
            (LoreBranchLocation::Local, 0, Disconnected::FFI_CODE)
        ]
    );
}

/// The record builders run at most [`RECORDS_IN_FLIGHT`] at a time, however many items there
/// are, and every record is passed on. Each builder waits for a permit once it runs, and the
/// permits come once [`RECORDS_IN_FLIGHT`] builders run, so no builder finishes before the cap is
/// reached.
#[tokio::test]
async fn record_builders_run_at_most_sixteen_at_once() {
    let items = 3 * RECORDS_IN_FLIGHT;
    let running = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let permits = Arc::new(Semaphore::new(0));
    let mut records = 0;
    let mut emitted = Vec::new();

    let (built, ()) = tokio::join!(
        build_records(
            &mut records,
            stream::iter(0..items),
            |item| {
                let (running, most, permits) = (running.clone(), most.clone(), permits.clone());
                Some(async move {
                    most.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    let _permit = permits.acquire().await.expect("the semaphore stays open");
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok::<_, RevisionGraphError>(item)
                })
            },
            |item| {
                emitted.push(item);
                1
            },
        ),
        async {
            tokio::time::timeout(Duration::from_secs(60), async {
                while running.load(Ordering::SeqCst) < RECORDS_IN_FLIGHT {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the first builders run");
            permits.add_permits(items);
        },
    );

    assert!(built.is_ok(), "{built:?}");
    assert_eq!(most.load(Ordering::SeqCst), RECORDS_IN_FLIGHT);
    assert_eq!(records, items as u64);
    emitted.sort_unstable();
    assert_eq!(emitted, (0..items).collect::<Vec<_>>());
}

/// Sets its flag when dropped.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A builder that fails ends the run only once every other builder has stopped, so no read a
/// builder makes outlives the call. On the test's current-thread runtime, a builder aborted
/// without waiting for it is dropped only once the test's task yields, so a run that ends sooner
/// fails the check.
#[tokio::test]
async fn a_failing_builder_stops_the_others_before_the_run_ends() {
    let stopped = Arc::new(AtomicBool::new(false));
    let mut records = 0;

    let built = build_records(
        &mut records,
        stream::iter([false, true]),
        |fails| {
            let pending = (!fails).then(|| SetOnDrop(stopped.clone()));
            Some(async move {
                if fails {
                    return Err(RevisionGraphError::internal("the builder fails"));
                }
                let _pending = pending;
                std::future::pending::<()>().await;
                Ok(())
            })
        },
        |()| 1,
    )
    .await;

    assert!(matches!(&built, Err(err) if err.is_internal()), "{built:?}");
    assert!(
        stopped.load(Ordering::SeqCst),
        "the pending builder stopped before the run ended"
    );
    assert_eq!(records, 0);
}
