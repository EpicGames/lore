// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The storage sessions the read verbs start, and their wall times and allocations.
//!
//! The session tests count the `session_start` fan-outs the connection of the fixture's storage
//! handle begins. A remote read joins a read of the same content that another read in the process
//! has in flight, under another partition too, and then starts no session, so each test reads only
//! blobs that hold ids of its own fixture. A test that asserts that calls start no session then
//! makes a call that reads a blob at the remote through the same handle, and asserts that it starts
//! one: the counter of a connection the handle doesn't use stays at zero.
//!
//! The benchmarks and the allocation report are ignored, and run on demand, one at a time:
//!
//! ```text
//! cargo test --release -p lore-integration-tests --features integration_tests --test integration \
//!     revision_graph::perf -- --ignored --nocapture --test-threads=1
//! ```
//!
//! A benchmark prints the fastest and the median time of a call, in microseconds, for each side of
//! a comparison; its doc says what each side reads. The allocation report counts with the
//! `allocount.c` interposer preloaded and the system allocator selected, as
//! `lore-revision/tests/path_allocations.rs` does, and prints no figure without them:
//!
//! ```text
//! gcc -shared -fPIC -O2 -o /tmp/libcount.so scripts/allocations/allocount.c
//! LORE_ALLOCATOR=system LD_PRELOAD=/tmp/libcount.so cargo test --release \
//!     -p lore-integration-tests --features integration_tests --test integration \
//!     revision_graph::perf::reports_allocations_per_branch_resolve -- --ignored --nocapture
//! ```

use std::time::Duration;
use std::time::Instant;

use lore::branch::LoreBranchInfoArgs;
use lore::repository::LoreRepositoryCloneArgs;
use lore::revision_graph::branch_list::branch_list;
use lore::revision_graph::branch_resolve::branch_resolve;
use lore::revision_graph::revision_resolve::revision_resolve;
use lore::storage::mutable_load::mutable_load;
use lore::storage::open::LoreStorageOpenArgs;
use lore::storage::open::LoreStorageRemoteConfig;
use lore_base::error::BranchNotFound;
use lore_base::test_util::TempDir;
use lore_base::test_util::allocations;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::repository::LoreSharedStoreMode;

use super::support::*;
use crate::common::net_common::close_handle;
use crate::common::net_common::open_handle_with_args;

/// Three `branch_resolve` calls under one correlation id start one session fan-out between them,
/// and a call under a fresh id starts one of its own. Each call reads the metadata blob of the
/// fixture's default branch at the remote.
#[tokio::test]
async fn branch_resolve_starts_one_session_per_correlation_id() {
    let f = fixture().await;
    let connection = connection_for(&f.url, f.repository).await;
    let before = connection.storage_session_starts();
    let starts = || connection.storage_session_starts() - before;

    let mut calls = Vec::new();
    for correlation_id in ["stable", "stable", "stable", ""] {
        let call = resolve_branch(
            &f,
            f.store,
            graph_globals_under(correlation_id),
            DEFAULT_BRANCH_NAME,
        )
        .await;
        calls.push((records_by_location(&call), starts()));
    }
    f.close().await;

    let at_remote = |starts| ((0, vec![LoreBranchLocation::Remote]), starts);
    assert_eq!(
        calls,
        [at_remote(1), at_remote(1), at_remote(1), at_remote(2)]
    );
}

/// Calls with the `local` flag start no session, though the remote holds what they read:
/// `branch_resolve` and `branch_list` of a branch both locations hold, and `revision_resolve` of a
/// revision number and of a latest. The same `branch_resolve` without the flag then reads the
/// branch's metadata blob at the remote, and starts one.
#[tokio::test]
async fn local_only_calls_start_no_session() {
    let f = fixture().await;
    seed_main(&f).await;
    create_branch_at_both_locations(
        &f,
        "both",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let connection = connection_for(&f.url, f.repository).await;
    let before = connection.storage_session_starts();
    let starts = || connection.storage_session_starts() - before;
    let local = || graph_globals_at(LoreBranchLocation::Local);

    let mut after = Vec::with_capacity(5);
    let branch = resolve_branch(&f, f.store, local(), "both").await;
    after.push(starts());
    let list = list_branches(&f, f.store, local(), false, "").await;
    after.push(starts());
    let number = resolve_revision(&f, f.store, local(), numbered(f.default_branch, 1)).await;
    after.push(starts());
    let at_latest = resolve_revision(&f, f.store, local(), latest(f.default_branch)).await;
    after.push(starts());
    let both = resolve_branch(&f, f.store, graph_globals(), "both").await;
    after.push(starts());
    f.close().await;

    assert_eq!(after, [0, 0, 0, 0, 1]);
    let local_only = || (0, vec![LoreBranchLocation::Local]);
    assert_eq!(
        [&branch, &list, &number, &at_latest].map(records_by_location),
        [local_only(), local_only(), local_only(), local_only()]
    );
    assert_eq!(
        records_by_location(&both),
        (
            0,
            vec![LoreBranchLocation::Remote, LoreBranchLocation::Local]
        )
    );
}

/// `revision_resolve` of the latest of a branch the remote lacks, with the `remote` flag and a
/// fresh correlation id, reads no blob at the remote and starts no session. The same call for the
/// default branch reads its latest's state and metadata there, and starts one.
#[tokio::test]
async fn a_remote_call_that_reads_no_blob_starts_no_session() {
    let f = fixture().await;
    seed_main(&f).await;
    let connection = connection_for(&f.url, f.repository).await;
    let before = connection.storage_session_starts();
    let starts = || connection.storage_session_starts() - before;
    let remote_fresh = || LoreGlobalArgs {
        remote: 1,
        ..graph_globals_under("")
    };

    let missing = latest(BranchId::from(uuid::Uuid::now_v7()));
    let (missing_status, missing_events) =
        resolve_revision(&f, f.store, remote_fresh(), missing).await;
    let after_missing = starts();
    let held = resolve_revision(&f, f.store, remote_fresh(), latest(f.default_branch)).await;
    let after_held = starts();
    f.close().await;

    assert_eq!([after_missing, after_held], [0, 1]);
    assert_eq!(missing_status, BranchNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&missing_events),
        [(LoreBranchLocation::Remote, 0, 0)]
    );
    assert_eq!(
        records_by_location(&held),
        (0, vec![LoreBranchLocation::Remote])
    );
}

/// The status of a call and the location of each of its records, in order.
fn records_by_location((status, events): &(i32, Vec<LoreEvent>)) -> (i32, Vec<LoreBranchLocation>) {
    let records = event_order(events)
        .into_iter()
        .filter(|&(_, complete)| !complete)
        .map(|(location, _)| location)
        .collect();
    (*status, records)
}

/// Graph `revision_resolve` of the default branch's latest, by its id, with the `remote` flag,
/// against the workaround: a `mutable_load` of the branch's latest key at the remote. The graph
/// call asks the remote's revision service for the branch (`branch_query`), then reads two blobs
/// there, the latest's state and its metadata; the workaround makes one key load over the storage
/// handle's session. Both answer the latest the fixture publishes.
#[ignore = "benchmark: run on demand with --ignored --nocapture"]
#[tokio::test]
async fn bench_revision_resolve_latest_remote_vs_mutable_load_workaround() {
    let f = fixture().await;
    let published = seed_main(&f).await;
    let remote = || graph_globals_at(LoreBranchLocation::Remote);
    let identifier = latest(f.default_branch);

    let resolved = named_revisions(&resolve_revision(&f, f.store, remote(), identifier).await);
    let loaded = latest_by_key(f.store, remote(), f.repository, f.default_branch).await;
    bench("graph revision_resolve of the latest, remote", || {
        revision_resolve(
            remote(),
            revision_resolve_args(&f, f.store, identifier),
            quiet(),
        )
    })
    .await;
    bench("mutable_load of the latest key, remote", || {
        mutable_load(
            remote(),
            latest_key_load(f.store, f.repository, f.default_branch),
            quiet(),
        )
    })
    .await;
    f.close().await;

    assert_eq!(resolved, [(LoreBranchLocation::Remote, published)]);
    assert_eq!(loaded, published);
}

/// Graph `revision_resolve` of the default branch's latest, by its id, with the `local` flag,
/// against the workaround: a `mutable_load` of the branch's latest key in the local store. The
/// graph call loads the latest key, then reads two blobs, the latest's state and its metadata; the
/// workaround makes the one key load. Both answer the latest the fixture commits.
#[ignore = "benchmark: run on demand with --ignored --nocapture"]
#[tokio::test]
async fn bench_revision_resolve_latest_local_vs_workaround() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let local = || graph_globals_at(LoreBranchLocation::Local);
    let identifier = latest(f.default_branch);

    let resolved = named_revisions(&resolve_revision(&f, f.store, local(), identifier).await);
    let loaded = latest_by_key(f.store, local(), f.repository, f.default_branch).await;
    bench("graph revision_resolve of the latest, local", || {
        revision_resolve(
            local(),
            revision_resolve_args(&f, f.store, identifier),
            quiet(),
        )
    })
    .await;
    bench("mutable_load of the latest key, local", || {
        mutable_load(
            local(),
            latest_key_load(f.store, f.repository, f.default_branch),
            quiet(),
        )
    })
    .await;
    f.close().await;

    assert_eq!(resolved, [(LoreBranchLocation::Local, seeded)]);
    assert_eq!(loaded, seeded);
}

/// Graph `branch_resolve` of the default branch, by its name, with the `remote` flag, against the
/// workaround: a `mutable_load` of the branch's latest key, by its id, at the remote. The graph
/// call asks the remote's revision service for the branch by name (`branch_query`), then reads the
/// branch's metadata blob there for `protect`, and answers the branch's whole record; the
/// workaround makes one key load over the storage handle's session. Both read the latest the
/// fixture publishes.
#[ignore = "benchmark: run on demand with --ignored --nocapture"]
#[tokio::test]
async fn bench_branch_resolve_remote_vs_mutable_load_workaround() {
    let f = fixture().await;
    let published = seed_main(&f).await;
    let remote = || graph_globals_at(LoreBranchLocation::Remote);

    let resolved =
        named_revisions(&resolve_branch(&f, f.store, remote(), DEFAULT_BRANCH_NAME).await);
    let loaded = latest_by_key(f.store, remote(), f.repository, f.default_branch).await;
    bench("graph branch_resolve, remote", || {
        branch_resolve(
            remote(),
            branch_resolve_args(&f, f.store, DEFAULT_BRANCH_NAME),
            quiet(),
        )
    })
    .await;
    bench("mutable_load of the latest key, remote", || {
        mutable_load(
            remote(),
            latest_key_load(f.store, f.repository, f.default_branch),
            quiet(),
        )
    })
    .await;
    f.close().await;

    assert_eq!(resolved, [(LoreBranchLocation::Remote, published)]);
    assert_eq!(loaded, published);
}

/// Graph `branch_resolve` of a branch, by its name, with the `local` flag, against the workaround:
/// a `mutable_load` of the branch's latest key, by its id, in the local store. The graph call loads
/// the name key, lists the registered instances (none here; the local store's list starts a
/// producer per bucket group, 256), then loads the branch's metadata key and its latest and latest
/// status keys, reads its metadata blob, and answers the branch's whole record; the workaround makes
/// one key load. Both read the latest the branch is created at.
#[ignore = "benchmark: run on demand with --ignored --nocapture"]
#[tokio::test]
async fn bench_branch_resolve_local_vs_workaround() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_local_branch(&f, "feature", f.default_branch, CREATOR).await;
    let local = || graph_globals_at(LoreBranchLocation::Local);

    let resolved = named_revisions(&resolve_branch(&f, f.store, local(), "feature").await);
    let loaded = latest_by_key(f.store, local(), f.repository, feature).await;
    bench("graph branch_resolve, local", || {
        branch_resolve(
            local(),
            branch_resolve_args(&f, f.store, "feature"),
            quiet(),
        )
    })
    .await;
    bench("mutable_load of the latest key, local", || {
        mutable_load(
            local(),
            latest_key_load(f.store, f.repository, feature),
            quiet(),
        )
    })
    .await;
    f.close().await;

    assert_eq!(resolved, [(LoreBranchLocation::Local, seeded)]);
    assert_eq!(loaded, seeded);
}

/// Graph `branch_resolve` with no flag, through a storage handle over the stores of a clone of the
/// fixture repository, against `lore::branch::info` of the default branch in that clone. The graph
/// call reads at the remote as with the `remote` flag, then in the clone's store as with the
/// `local` flag, where it also reads the clone's instance: its registration blob, its current
/// branch and revision keys, and its path on disk for staleness. `lore::branch::info` loads the
/// instance at the clone's path, resolves the name in its local store, asks the remote for the
/// branch (`branch_query`), and reads the local and the remote metadata blobs, the remote one from
/// the local store once the first call has stored it there. Both read the latest the fixture
/// publishes.
#[ignore = "benchmark: run on demand with --ignored --nocapture"]
#[tokio::test]
async fn bench_branch_resolve_vs_branch_info_on_a_clone() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let dir = TempDir::new("revision-graph-bench-clone-");
    events_of("cloning the repository", |callback| {
        lore::repository::clone(
            LoreGlobalArgs {
                repository_path: dir.path().into(),
                ..graph_globals()
            },
            LoreRepositoryCloneArgs {
                repository_url: LoreString::from(format!("{}/{}", f.url, f.repository).as_str()),
                bare: 1,
                use_shared_store: LoreSharedStoreMode::Disabled,
                ..Default::default()
            },
            callback,
        )
    })
    .await;
    let over_clone = open_handle_with_args(
        LoreGlobalArgs::default(),
        LoreStorageOpenArgs {
            repository_path: dir.path().into(),
            remote_config: LoreStorageRemoteConfig {
                remote_url: LoreString::from(f.url.as_str()),
            },
            has_remote_config: 1,
            ..Default::default()
        },
    )
    .await;
    let in_clone = || LoreGlobalArgs {
        repository_path: dir.path().into(),
        ..graph_globals()
    };
    let info = || LoreBranchInfoArgs {
        branch: LoreString::from(DEFAULT_BRANCH_NAME),
        link: LoreString::default(),
    };

    let resolved = named_revisions(
        &resolve_branch(&f, over_clone, graph_globals(), DEFAULT_BRANCH_NAME).await,
    );
    let info_latest = events_of("reading the branch info", |callback| {
        lore::branch::info(in_clone(), info(), callback)
    })
    .await
    .iter()
    .find_map(|event| match event {
        LoreEvent::BranchInfo(data) => Some(data.latest),
        _ => None,
    });
    bench("graph branch_resolve over the clone's stores", || {
        branch_resolve(
            graph_globals(),
            branch_resolve_args(&f, over_clone, DEFAULT_BRANCH_NAME),
            quiet(),
        )
    })
    .await;
    bench("lore::branch::info in the clone", || {
        lore::branch::info(in_clone(), info(), quiet())
    })
    .await;
    close_handle(over_clone).await;
    f.close().await;

    assert_eq!(
        resolved,
        [
            (LoreBranchLocation::Remote, seeded),
            (LoreBranchLocation::Local, seeded)
        ]
    );
    assert_eq!(info_latest, Some(seeded));
}

/// Graph `branch_list` with the `local` flag over 100 branches in the local store. The call lists
/// the registered instances once (none here) and the branches' ids, then, at most 16 at a time,
/// loads each branch's metadata key and its latest and latest status keys, and reads its metadata
/// blob.
#[ignore = "benchmark: run on demand with --ignored --nocapture"]
#[tokio::test]
async fn bench_branch_list_local_100_branches() {
    let f = fixture().await;
    seed_main(&f).await;
    let mut created = Vec::with_capacity(100);
    for index in 0..100 {
        let name = format!("branch-{index}");
        created.push(create_local_branch(&f, &name, f.default_branch, CREATOR).await);
    }
    let local = || graph_globals_at(LoreBranchLocation::Local);

    let (status, events) = list_branches(&f, f.store, local(), false, "").await;
    bench("graph branch_list, local, 100 branches", || {
        branch_list(local(), branch_list_args(&f, f.store, false, ""), quiet())
    })
    .await;
    f.close().await;

    let mut listed: Vec<_> = branch_records(&events)
        .into_iter()
        .map(|record| record.branch)
        .collect();
    listed.sort_unstable();
    created.sort_unstable();
    assert_eq!((status, listed), (0, created));
}

/// The calls a benchmark times, after its [`WARM_UP_CALLS`].
const TIMED_CALLS: usize = 200;

/// Calls a benchmark makes before it times any.
const WARM_UP_CALLS: usize = 20;

/// Times [`TIMED_CALLS`] calls `call` makes after [`WARM_UP_CALLS`] untimed ones, and prints the
/// fastest and the median time of a call under `label`, in microseconds. Panics unless every call
/// returns 0.
///
/// The calls take their events with [`quiet`] rather than [`status_and_events_of`], whose copy of
/// each event would add to the time of the call.
async fn bench<F: Future<Output = i32>>(label: &str, mut call: impl FnMut() -> F) {
    for _ in 0..WARM_UP_CALLS {
        assert_eq!(call().await, 0, "{label} must succeed");
    }
    let mut times = Vec::with_capacity(TIMED_CALLS);
    for _ in 0..TIMED_CALLS {
        let start = Instant::now();
        let status = call().await;
        times.push(start.elapsed());
        assert_eq!(status, 0, "{label} must succeed");
    }
    times.sort_unstable();
    let median = (times[(TIMED_CALLS - 1) / 2] + times[TIMED_CALLS / 2]) / 2;
    println!(
        "{label}: min {:.1} µs, median {:.1} µs over {TIMED_CALLS} calls",
        micros(times[0]),
        micros(median)
    );
}

/// `duration` in microseconds.
fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

/// A callback that ignores every event. It captures nothing, so boxing it allocates nothing.
fn quiet() -> LoreEventCallback {
    Some(Box::new(|_: &LoreEvent| {}))
}

/// The location of each branch and revision record of a call that succeeds, in order, and the
/// revision it names: a branch record's latest, or a revision record's revision.
fn named_revisions((status, events): &(i32, Vec<LoreEvent>)) -> Vec<(LoreBranchLocation, Hash)> {
    assert_eq!(*status, 0, "the call must succeed");
    events
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionGraphBranch(data) => Some((data.location, data.latest)),
            LoreEvent::RevisionGraphRevision(data) => Some((data.location, data.revision_hash)),
            _ => None,
        })
        .collect()
}

/// Calls the allocation report counts.
const COUNTED_CALLS: usize = 100;

/// The allocations of `branch_resolve` with the `local` flag on an in-memory storage handle, per
/// call, over [`COUNTED_CALLS`] calls after one that warms the process up. The branch has no
/// instance. The call arguments are built before the count starts, and the calls take their events
/// with [`quiet`], so the count holds what the verb allocates.
#[ignore = "reports allocation counts; run on demand with the interposer preloaded"]
#[tokio::test]
async fn reports_allocations_per_branch_resolve() {
    let f = fixture_without_remote().await;
    commit_on(f.store, f.repository, f.default_branch, Hash::default()).await;
    create_local_branch(&f, "feature", f.default_branch, CREATOR).await;
    let local = || graph_globals_at(LoreBranchLocation::Local);
    let warm_up = resolve_branch(&f, f.store, local(), "feature").await;
    let calls: Vec<_> = (0..COUNTED_CALLS)
        .map(|_| (local(), branch_resolve_args(&f, f.store, "feature")))
        .collect();
    let mut statuses = Vec::with_capacity(COUNTED_CALLS);

    let before = allocations();
    for (globals, args) in calls {
        statuses.push(branch_resolve(globals, args, quiet()).await);
    }
    let after = allocations();
    f.close().await;

    assert_eq!(
        records_by_location(&warm_up),
        (0, vec![LoreBranchLocation::Local])
    );
    assert_eq!(statuses, [0; COUNTED_CALLS]);
    match before.zip(after) {
        Some((before, after)) => println!(
            "graph branch_resolve, local: {} allocations in {COUNTED_CALLS} calls, {:.1} a call",
            after - before,
            (after - before) as f64 / COUNTED_CALLS as f64
        ),
        None => println!("graph branch_resolve, local: allocations not counted"),
    }
}
