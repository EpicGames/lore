// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore::interface::LoreGlobalArgs;
use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::RevisionForm;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::locations::CallOutcome;
use lore::revision_graph::locations::LocationAnswer;
use lore::revision_graph::locations::LocationMiss;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::revision_graph::revision_resolve::LoreRevisionGraphRevisionResolveArgs;
use lore::revision_graph::revision_resolve::Resolved;
use lore::revision_graph::revision_resolve::listed_revision;
use lore::revision_graph::revision_resolve::revision_resolve;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::add::add;
use lore::revision_tree::close::LoreRevisionTreeCloseArgs;
use lore::revision_tree::close::close;
use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
use lore::revision_tree::commit::commit;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
use lore::revision_tree::metadata_set::metadata_set;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::BranchNotFound;
use lore_base::error::Disconnected;
use lore_base::error::InvalidArguments;
use lore_base::error::NotConnected;
use lore_base::error::RevisionNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::Internal;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::event::LoreEvent;
use lore_revision::immutable;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::metadata::BRANCH;
use lore_revision::metadata::Metadata;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::test_util::write_not_a_state;
use lore_revision::test_util::write_state_revision;
use lore_storage::test_util::StoreCall;
use lore_transport::RevisionItem;

use crate::call::test_support::recording_callback;
use crate::call::test_support::take_without_log_lines;
use crate::revision_graph::support::ListAnswer;
use crate::revision_graph::support::NamingRevision;
use crate::revision_graph::support::QueryAnswer;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::create;
use crate::revision_graph::support::handle_failing;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::location_outcomes;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::revision_graph::support::partial_metadata;
use crate::revision_graph::support::revision_records;
use crate::revision_graph::support::store_corrupt_metadata;
use crate::revision_graph::support::store_with_remote;
use crate::revision_tree::support::load_tree;

/// The `id` every call in this file passes.
const CALL_ID: u64 = 24;

/// The identifier of the latest revision of `branch`.
fn latest(branch: BranchId) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        branch,
        ..Default::default()
    }
}

/// The identifier of revision `number` on `branch`.
fn numbered(branch: BranchId, number: u64) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        branch,
        revision_number: number,
        ..Default::default()
    }
}

/// The identifier of the revision with `hash`.
fn by_hash(hash: Hash) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        hash,
        ..Default::default()
    }
}

/// Runs `revision_resolve` on `store` for `identifier` in `repository` under `globals`, and returns
/// its status and the events it sent other than log lines.
async fn resolve_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    repository: Partition,
    identifier: LoreRevisionIdentifier,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = revision_resolve(
        globals,
        LoreRevisionGraphRevisionResolveArgs {
            id: CALL_ID,
            store,
            repository,
            revision: identifier,
        },
        callback,
    )
    .await;
    (status, take_without_log_lines(&events))
}

/// [`resolve_with`] under default globals.
async fn resolve(
    store: LoreStore,
    repository: Partition,
    identifier: LoreRevisionIdentifier,
) -> (i32, Vec<LoreEvent>) {
    resolve_with(LoreGlobalArgs::default(), store, repository, identifier).await
}

/// The revision records of `events` as (location, hash, number, branch), each echoing the call's
/// id.
fn resolved(events: &[LoreEvent]) -> Vec<(LoreBranchLocation, Hash, u64, BranchId)> {
    revision_records(events)
        .into_iter()
        .map(|record| {
            assert_eq!(record.id, CALL_ID);
            (
                record.location,
                record.revision_hash,
                record.revision_number,
                record.branch,
            )
        })
        .collect()
}

/// The local store's location complete event of `events` as (records, error code), after
/// asserting that it is the only location complete event and echoes the call's id.
fn local_outcome(events: &[LoreEvent]) -> (u64, i32) {
    let [(LoreBranchLocation::Local, records, code)] = location_outcomes(events, CALL_ID)[..]
    else {
        panic!("expected one location complete event, the local store's");
    };
    (records, code)
}

/// Writes through `write` a revision metadata blob that names `branch`, and returns its hash.
async fn metadata_naming(write: &Arc<RepositoryContext>, branch: BranchId) -> Hash {
    let mut metadata = Metadata::new();
    metadata.set_branch(branch).expect("sets the branch");
    metadata
        .serialize(write.clone())
        .await
        .expect("writes the metadata")
}

/// Writes through `write` a line of `count` revisions numbered from 1, each with metadata naming
/// `branch`, and returns them oldest first.
async fn write_line(write: &Arc<RepositoryContext>, branch: BranchId, count: u64) -> Vec<Hash> {
    let metadata = metadata_naming(write, branch).await;
    let mut line = Vec::new();
    let mut parent = Hash::default();
    for number in 1..=count {
        parent = write_state_revision(write, parent, number, metadata).await;
        line.push(parent);
    }
    line
}

/// Creates `branch`, named `name`, in the local store through `write`, at `latest`.
async fn create_at(write: &Arc<RepositoryContext>, branch: BranchId, name: &str, latest: Hash) {
    create(
        write,
        branch,
        name,
        "ana",
        Vec::new(),
        latest,
        BranchLatestStatus::Convergent,
    )
    .await;
}

/// Commits a revision on `tree` that adds the file `name`, and returns the revision.
async fn commit_file(tree: LoreRevisionTree, name: &str) -> Hash {
    let entry = LoreRevisionTreeAddEntry {
        entry_id: 1,
        parent_node_id: ROOT_NODE,
        parent_entry_index: 0,
        name: LoreString::from(name),
        kind: LoreNodeType::File as u32,
        mode: 0o644,
        size: 10,
        address: Address {
            hash: hash(4),
            context: Context::from(rand::random::<[u8; 16]>()),
        },
    };
    let args = LoreRevisionTreeAddArgs {
        batch_id: 1,
        handle: tree,
        entries: LoreArray::from_vec(vec![entry]),
    };
    assert_eq!(add(LoreGlobalArgs::default(), args, None).await, 0);
    let (events, callback) = recording_callback();
    let args = LoreRevisionTreeCommitArgs {
        id: 1,
        handle: tree,
        options: Default::default(),
    };
    assert_eq!(commit(LoreGlobalArgs::default(), args, callback).await, 0);
    events
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            LoreEvent::RevisionTreeCommitComplete(data) => Some(data.revision_hash),
            _ => None,
        })
        .expect("the commit reports its revision")
}

/// Commits `count` revisions on `branch` through the revision tree API on `store`, each adding a
/// file, and returns them oldest first.
async fn commit_line(
    store: LoreStore,
    repository: Partition,
    branch: BranchId,
    count: usize,
) -> Vec<Hash> {
    let tree = load_tree(store, repository).await;
    let entry = LoreRevisionTreeMetadataSetEntry {
        entry_id: 1,
        key: LoreString::from(BRANCH),
        value: LoreMetadata::Context(branch),
    };
    let args = LoreRevisionTreeMetadataSetArgs {
        batch_id: 1,
        handle: tree,
        entries: LoreArray::from_vec(vec![entry]),
    };
    assert_eq!(metadata_set(LoreGlobalArgs::default(), args, None).await, 0);
    let mut line = Vec::new();
    for index in 0..count {
        line.push(commit_file(tree, &format!("file-{index}")).await);
    }
    let args = LoreRevisionTreeCloseArgs {
        id: 1,
        handle: tree,
        ..Default::default()
    };
    assert_eq!(close(LoreGlobalArgs::default(), args, None).await, 0);
    line
}

/// An identifier that names no revision, or names one in two ways, fails the call with
/// `InvalidArguments` before the call consults any location: no record and no location complete
/// event.
#[tokio::test]
async fn a_malformed_identifier_fails_invalid_arguments_before_any_read() {
    let (_store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();

    for identifier in [
        LoreRevisionIdentifier {
            hash: hash(1),
            branch,
            ..Default::default()
        },
        LoreRevisionIdentifier {
            hash: hash(1),
            revision_number: 1,
            ..Default::default()
        },
        LoreRevisionIdentifier {
            revision_number: 1,
            ..Default::default()
        },
        LoreRevisionIdentifier::default(),
    ] {
        let (status, events) = resolve(handle, repository, identifier).await;

        assert_eq!(status, InvalidArguments::FFI_CODE, "{identifier:?}");
        let [LoreEvent::Complete(_), LoreEvent::End(_)] = &events[..] else {
            panic!("expected Complete, then End, for {identifier:?}");
        };
    }
    close_handle(handle).await;
}

/// The latest revision of a branch the local store holds resolves to one local record with its
/// hash, its number and the branch its metadata names.
#[tokio::test]
async fn latest_on_a_local_branch_emits_one_local_record() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let write = local_write_context(&store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, branch, 2).await;
        create_at(&write, branch, "main", line[1]).await;
        line
    })
    .await;

    let (status, events) = resolve(handle, repository, latest(branch)).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        resolved(&events),
        [(LoreBranchLocation::Local, line[1], 2, branch)]
    );
}

/// A revision number resolves to the revision a walk from the branch's latest finds, on a branch
/// whose revisions the revision tree API committed.
#[tokio::test]
async fn a_number_on_a_local_branch_walks_to_it() {
    let (_store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let line = commit_line(handle, repository, branch, 3).await;

    let (status, events) = resolve(handle, repository, numbered(branch, 2)).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        resolved(&events),
        [(LoreBranchLocation::Local, line[1], 2, branch)]
    );
}

/// A hash resolves to its revision's number and the branch the revision's metadata names, though
/// the local store holds no record of that branch.
#[tokio::test]
async fn a_hash_resolves_to_its_number_and_branch() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let write = local_write_context(&store, repository);
    let line = in_setup_context(write_line(&write, branch, 2)).await;

    let (status, events) = resolve(handle, repository, by_hash(line[0])).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        resolved(&events),
        [(LoreBranchLocation::Local, line[0], 1, branch)]
    );
}

/// The latest revision and a revision number on a branch the local store doesn't hold fail the
/// call with `BranchNotFound`, a miss at the local store: no record, and a location complete event
/// with no error.
#[tokio::test]
async fn an_unknown_branch_fails_branch_not_found() {
    let (_store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();

    for identifier in [latest(branch), numbered(branch, 1)] {
        let (status, events) = resolve(handle, repository, identifier).await;

        assert_eq!(status, BranchNotFound::FFI_CODE, "{identifier:?}");
        assert!(resolved(&events).is_empty());
        assert_eq!(local_outcome(&events), (0, 0), "{identifier:?}");
    }
    close_handle(handle).await;
}

/// A branch the local store holds without a latest revision, as a branch created with none is,
/// fails the call with `RevisionNotFound` for its latest and for a number, a miss at the local
/// store.
#[tokio::test]
async fn a_branch_without_a_latest_fails_revision_not_found() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(create_at(&write, branch, "empty", Hash::default())).await;

    for identifier in [latest(branch), numbered(branch, 1)] {
        let (status, events) = resolve(handle, repository, identifier).await;

        assert_eq!(status, RevisionNotFound::FFI_CODE, "{identifier:?}");
        assert!(resolved(&events).is_empty());
        assert_eq!(local_outcome(&events), (0, 0), "{identifier:?}");
    }
    close_handle(handle).await;
}

/// A number above the branch's latest fails the call with `RevisionNotFound`, a miss at the local
/// store.
#[tokio::test]
async fn a_number_past_the_latest_fails_revision_not_found() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        let line = write_line(&write, branch, 2).await;
        create_at(&write, branch, "main", line[1]).await;
    })
    .await;

    let (status, events) = resolve(handle, repository, numbered(branch, 3)).await;
    close_handle(handle).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert!(resolved(&events).is_empty());
    assert_eq!(local_outcome(&events), (0, 0));
}

/// A hash whose state the local store doesn't hold fails the call with `RevisionNotFound`, a miss
/// at the local store.
#[tokio::test]
async fn a_hash_the_store_lacks_fails_revision_not_found() {
    let (_store, handle) = open_in_memory_handle().await;

    let (status, events) = resolve(handle, new_repository(), by_hash(hash(9))).await;
    close_handle(handle).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert!(resolved(&events).is_empty());
    assert_eq!(local_outcome(&events), (0, 0));
}

/// A revision whose metadata blob the local store doesn't hold, as a branch point cached from a
/// remote revision list is, or whose metadata names no branch, reports a zero branch.
#[tokio::test]
async fn a_branch_point_without_local_metadata_reports_no_branch() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, without_blob, without_key) = (new_branch(), new_branch(), new_branch());
    let write = local_write_context(&store, repository);
    let (point, unnamed) = in_setup_context(async {
        let point = write_state_revision(&write, Hash::default(), 1, hash(9)).await;
        let nameless = partial_metadata(None, &[("message", "no branch")])
            .serialize(write.clone())
            .await
            .expect("writes the metadata");
        let unnamed = write_state_revision(&write, Hash::default(), 1, nameless).await;
        let stack = |revision| {
            vec![BranchPoint {
                branch: main,
                revision,
            }]
        };
        for (branch, name, revision) in [(without_blob, "x", point), (without_key, "y", unnamed)] {
            create(
                &write,
                branch,
                name,
                "ana",
                stack(revision),
                revision,
                BranchLatestStatus::Convergent,
            )
            .await;
        }
        (point, unnamed)
    })
    .await;
    let globals = LoreGlobalArgs {
        local: 1,
        ..Default::default()
    };

    let (status, without_blob_events) =
        resolve_with(globals.clone(), handle, repository, latest(without_blob)).await;
    assert_eq!(status, 0);
    let (status, without_key_events) =
        resolve_with(globals, handle, repository, latest(without_key)).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        resolved(&without_blob_events),
        [(LoreBranchLocation::Local, point, 1, BranchId::default())]
    );
    assert_eq!(
        resolved(&without_key_events),
        [(LoreBranchLocation::Local, unnamed, 1, BranchId::default())]
    );
}

/// A walk that reaches a revision whose state the local store doesn't hold finds no revision: the
/// call fails with `RevisionNotFound`, a miss at the local store.
#[tokio::test]
async fn a_walk_through_a_state_the_store_lacks_is_a_miss() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        let metadata = metadata_naming(&write, branch).await;
        let latest = write_state_revision(&write, hash(0xA0), 2, metadata).await;
        create_at(&write, branch, "main", latest).await;
    })
    .await;

    let (status, events) = resolve(handle, repository, numbered(branch, 1)).await;
    close_handle(handle).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert_eq!(local_outcome(&events), (0, 0));
}

/// The local record is followed by the local store's location complete event, which counts it,
/// and a handle without a remote sends no remote location complete event.
#[tokio::test]
async fn a_local_record_is_followed_by_its_location_complete_event() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let write = local_write_context(&store, repository);
    let line = in_setup_context(write_line(&write, branch, 1)).await;

    let (status, events) = resolve(handle, repository, by_hash(line[0])).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    let [
        LoreEvent::RevisionGraphRevision(record),
        LoreEvent::RevisionGraphLocationComplete(complete),
        LoreEvent::Complete(_),
        LoreEvent::End(_),
    ] = &events[..]
    else {
        panic!("expected a revision record, its location complete event, Complete and End");
    };
    assert_eq!(record.location, LoreBranchLocation::Local);
    assert_eq!(complete.location, LoreBranchLocation::Local);
    assert_eq!((complete.records, complete.error.error_code), (1, 0));
}

/// A hash the identifier names, whose blob in the local store isn't a revision state, names no
/// revision there: the call fails with `RevisionNotFound`, a miss at the local store.
#[tokio::test]
async fn a_hash_naming_a_blob_that_is_not_a_revision_state_is_a_miss() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    let not_a_state = in_setup_context(write_not_a_state(&write)).await;

    let (status, events) = resolve(handle, repository, by_hash(not_a_state)).await;
    close_handle(handle).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert_eq!(local_outcome(&events), (0, 0));
}

/// A blob that isn't a revision state, as the branch's latest or reached by a walk through a
/// parent link, fails the local store's location with `Internal` rather than reading as a miss:
/// a latest and a parent link name a revision state.
#[tokio::test]
async fn a_latest_or_parent_that_is_not_a_revision_state_fails_the_local_location() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (at_latest, walked) = (new_branch(), new_branch());
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        let not_a_state = write_not_a_state(&write).await;
        create_at(&write, at_latest, "at-latest", not_a_state).await;
        let metadata = metadata_naming(&write, walked).await;
        let above = write_state_revision(&write, not_a_state, 2, metadata).await;
        create_at(&write, walked, "walked", above).await;
    })
    .await;

    for identifier in [latest(at_latest), numbered(walked, 1)] {
        let (status, events) = resolve(handle, repository, identifier).await;

        assert_eq!(status, Internal::FFI_CODE, "{identifier:?}");
        assert_eq!(
            local_outcome(&events),
            (0, Internal::FFI_CODE),
            "{identifier:?}"
        );
    }
    close_handle(handle).await;
}

/// A number at or below a branch's branch point resolves to the revision a walk reaches past the
/// branch point, with the branch the parent's metadata names rather than the child's.
#[tokio::test]
async fn a_number_past_the_branch_point_reports_the_parents_branch() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, child) = (new_branch(), new_branch());
    let write = local_write_context(&store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, main, 2).await;
        create_at(&write, main, "main", line[1]).await;
        let metadata = metadata_naming(&write, child).await;
        let on_child = write_state_revision(&write, line[1], 3, metadata).await;
        let stack = vec![BranchPoint {
            branch: main,
            revision: line[1],
        }];
        create(
            &write,
            child,
            "child",
            "ana",
            stack,
            on_child,
            BranchLatestStatus::Convergent,
        )
        .await;
        line
    })
    .await;

    let (status, events) = resolve(handle, repository, numbered(child, 2)).await;
    close_handle(handle).await;

    assert_eq!(status, 0);
    assert_eq!(
        resolved(&events),
        [(LoreBranchLocation::Local, line[1], 2, main)]
    );
}

/// A revision whose metadata blob is corrupt fails the local store's location with `Internal`,
/// rather than reporting a zero branch.
#[tokio::test]
async fn a_corrupt_revision_metadata_blob_fails_the_local_location() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let holder = new_branch();
    let write = local_write_context(&store, repository);
    let revision = in_setup_context(async {
        store_corrupt_metadata(&write, holder).await;
        let corrupt = branch::mutable_load(write.clone(), branch::METADATA, holder)
            .await
            .expect("loads the corrupt blob's hash");
        write_state_revision(&write, Hash::default(), 1, corrupt).await
    })
    .await;

    let (status, events) = resolve(handle, repository, by_hash(revision)).await;
    close_handle(handle).await;

    assert_eq!(status, Internal::FFI_CODE);
    assert_eq!(local_outcome(&events), (0, Internal::FFI_CODE));
}

/// A failed load of the branch's latest, or of its metadata key where it has no latest, fails the
/// local store's location with the store's error, rather than reading as a missing branch.
#[tokio::test]
async fn a_failed_load_fails_the_local_location() {
    let store = in_memory_for_tests("graph-revision-resolve-failed-load").await;
    let repository = new_repository();
    let branch = new_branch();
    let salt = local_read_context(&store, repository).salt();

    for function in [branch::LATEST, branch::METADATA] {
        let failing = branch::mutable_key(salt, function, repository, branch).0;
        let handle = handle_failing(&store, move |call, key, _| {
            call == StoreCall::Load && key == failing
        });

        let (status, events) = resolve(handle, repository, latest(branch)).await;
        close_handle(handle).await;

        assert_eq!(
            status,
            Disconnected::FFI_CODE,
            "a failed load of {function}"
        );
        assert_eq!(
            local_outcome(&events),
            (0, Disconnected::FFI_CODE),
            "a failed load of {function}"
        );
    }
}

/// A store whose remote reaches `service`, and in whose local store `branch` has a line of two
/// revisions, the second its latest. Returns the store's handle and the line, oldest first.
async fn remote_and_local_line(
    repository: Partition,
    branch: BranchId,
    service: Arc<NamingRevision>,
) -> (LoreStore, Vec<Hash>) {
    let store = store_with_remote(repository, service).await;
    let write = local_write_context(&store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, branch, 2).await;
        create_at(&write, branch, "main", line[1]).await;
        line
    })
    .await;
    (handle::register(Arc::new(store)), line)
}

/// A branch query the remote refuses fails the remote's location for the latest revision, rather
/// than reading as a miss, and the call with its error, though the local store's record follows.
#[tokio::test]
async fn a_refused_branch_query_fails_the_call_after_the_local_record() {
    let repository = new_repository();
    let branch = new_branch();
    let service = NamingRevision::new(QueryAnswer::SlowDown);
    let (handle, line) = remote_and_local_line(repository, branch, service.clone()).await;

    let (status, events) = resolve(handle, repository, latest(branch)).await;
    close_handle(handle).await;

    assert_eq!(status, SlowDown::FFI_CODE);
    assert_eq!(
        resolved(&events),
        [(LoreBranchLocation::Local, line[1], 2, branch)]
    );
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, SlowDown::FFI_CODE),
            (LoreBranchLocation::Local, 1, 0)
        ]
    );
    assert_eq!(*service.queries.lock().unwrap(), [(Some(branch), None)]);
}

/// A revision list the remote refuses fails the remote's location for a number, rather than
/// falling back to the branch query that tells a miss apart, and the call with its error, though
/// the local store's record follows.
#[tokio::test]
async fn a_refused_revision_list_fails_the_call_without_a_branch_query() {
    let repository = new_repository();
    let branch = new_branch();
    let service = NamingRevision::listing(QueryAnswer::NotFound, ListAnswer::SlowDown);
    let (handle, line) = remote_and_local_line(repository, branch, service.clone()).await;

    let (status, events) = resolve(handle, repository, numbered(branch, 1)).await;
    close_handle(handle).await;

    assert_eq!(status, SlowDown::FFI_CODE);
    assert_eq!(
        resolved(&events),
        [(LoreBranchLocation::Local, line[0], 1, branch)]
    );
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [
            (LoreBranchLocation::Remote, 0, SlowDown::FFI_CODE),
            (LoreBranchLocation::Local, 1, 0)
        ]
    );
    assert!(service.queries.lock().unwrap().is_empty());
}

/// Where the remote's revision list answers `NotFound` for a number, its branch query by the
/// branch's id tells the misses apart: a branch the remote doesn't hold is a `BranchNotFound`
/// miss, and one it holds a `RevisionNotFound` miss, which fails the call though the local store
/// lacks the branch.
#[tokio::test]
async fn a_number_the_remote_list_lacks_is_a_miss_its_branch_query_names() {
    let repository = new_repository();
    let branch = new_branch();

    for (query, code) in [
        (QueryAnswer::NotFound, BranchNotFound::FFI_CODE),
        (QueryAnswer::Record(branch), RevisionNotFound::FFI_CODE),
    ] {
        let service = NamingRevision::listing(query, ListAnswer::NotFound);
        let store = store_with_remote(repository, service.clone()).await;
        let handle = handle::register(Arc::new(store));

        let (status, events) = resolve(handle, repository, numbered(branch, 1)).await;
        close_handle(handle).await;

        assert_eq!(status, code);
        assert!(resolved(&events).is_empty());
        assert_eq!(
            location_outcomes(&events, CALL_ID),
            [
                (LoreBranchLocation::Remote, 0, 0),
                (LoreBranchLocation::Local, 0, 0)
            ]
        );
        assert_eq!(*service.queries.lock().unwrap(), [(Some(branch), None)]);
    }
}

/// A resolved revision for the outcome tests.
fn resolution() -> LocationAnswer<Resolved> {
    Ok(Ok(Resolved {
        hash: hash(1),
        number: 1,
        branch: new_branch(),
    }))
}

/// The outcome of a call whose locations, remote first, gave `answers`, for the latest revision of
/// a branch.
fn outcome(
    answers: impl IntoIterator<Item = LocationAnswer<Resolved>>,
) -> Result<(), RevisionGraphError> {
    let mut outcome = CallOutcome::default();
    for answer in answers {
        outcome.add(answer);
    }
    outcome.finish(|miss| miss.error(RevisionForm::Latest(new_branch())))
}

/// A failure at any location fails the call with that failure, even where the other location
/// resolved the revision, and with the remote's, the first location's, where both failed.
#[test]
fn a_failure_at_any_location_fails_the_call() {
    let remote_failed = outcome([Err(Disconnected.into()), resolution()]);
    let local_failed = outcome([
        resolution(),
        Err(NotConnected {
            reason: "the local store".into(),
        }
        .into()),
    ]);
    let both_failed = outcome([Err(SlowDown.into()), Err(Disconnected.into())]);

    assert!(
        remote_failed
            .as_ref()
            .is_err_and(|err| err.is_disconnected()),
        "{remote_failed:?}"
    );
    assert!(
        local_failed
            .as_ref()
            .is_err_and(|err| err.is_not_connected()),
        "{local_failed:?}"
    );
    assert!(
        both_failed.as_ref().is_err_and(|err| err.is_slow_down()),
        "{both_failed:?}"
    );
}

/// A call succeeds where any location resolved the revision and none failed, whatever the other
/// location missed.
#[test]
fn a_record_at_any_location_succeeds_the_call() {
    for answers in [
        [Ok(Err(LocationMiss::BranchNotFound)), resolution()],
        [resolution(), Ok(Err(LocationMiss::RevisionNotFound))],
        [resolution(), resolution()],
    ] {
        let outcome = outcome(answers);

        assert!(outcome.is_ok(), "{outcome:?}");
    }
    assert!(outcome([resolution()]).is_ok());
}

/// A call no location resolved fails with `BranchNotFound` only where every location lacked the
/// branch, and with `RevisionNotFound` otherwise.
#[test]
fn a_call_without_a_record_fails_branch_not_found_only_where_no_location_holds_the_branch() {
    use LocationMiss::BranchNotFound;
    use LocationMiss::RevisionNotFound;

    for (misses, branch_missing) in [
        (vec![BranchNotFound, BranchNotFound], true),
        (vec![BranchNotFound], true),
        (vec![BranchNotFound, RevisionNotFound], false),
        (vec![RevisionNotFound, BranchNotFound], false),
        (vec![RevisionNotFound], false),
    ] {
        let outcome = outcome(misses.iter().map(|miss| Ok(Err(*miss))));

        let err = outcome.expect_err("no location resolved the revision");
        assert_eq!(err.is_branch_not_found(), branch_missing, "{misses:?}");
        assert_eq!(err.is_revision_not_found(), !branch_missing, "{misses:?}");
    }
}

/// The bytes of the state of `revision`, read through `ctx`.
async fn state_bytes(ctx: &Arc<RepositoryContext>, revision: Hash) -> Bytes {
    immutable::read(
        ctx.clone(),
        Address::zero_context_hash(revision),
        None,
        immutable::read_options_from_repository(ctx),
    )
    .await
    .expect("reads the state")
}

/// A remote page's item for a revision, labelled `number`, carrying `state`.
fn item(number: u64, signature: Hash, state: Bytes) -> RevisionItem {
    RevisionItem {
        number,
        signature,
        metadata: hash(5),
        state,
    }
}

/// A remote page yields the revision whose own state has the number asked for, wherever the page
/// holds it, with the metadata its state records rather than the item's, and none where no item is
/// labelled the number. An item labelled the number whose state has another number, or isn't its
/// revision's own though it has that number, fails as internal.
#[tokio::test]
async fn a_remote_page_yields_the_revision_its_state_numbers() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let branch = new_branch();
    let write = local_write_context(&store, repository);
    let (line, states, metadata, elsewhere) = in_setup_context(async {
        let line = write_line(&write, branch, 3).await;
        let metadata = metadata_naming(&write, branch).await;
        let mut states = Vec::new();
        for revision in &line {
            states.push(state_bytes(&write, *revision).await);
        }
        let elsewhere = write_state_revision(&write, hash(0xB0), 2, metadata).await;
        let elsewhere = state_bytes(&write, elsewhere).await;
        (line, states, metadata, elsewhere)
    })
    .await;
    close_handle(handle).await;

    let page = [
        item(3, line[2], states[2].clone()),
        item(2, line[1], states[1].clone()),
        item(1, line[0], states[0].clone()),
    ];
    assert_eq!(
        listed_revision(&page, 2).expect("the page holds number 2"),
        Some((line[1], metadata))
    );
    assert_eq!(
        listed_revision(&page, 4).expect("no item is labelled 4"),
        None
    );
    let mislabelled = [item(2, line[2], states[2].clone())];
    let not_its_own = [item(2, line[1], elsewhere)];
    for (named, page) in [("mislabelled", mislabelled), ("not its own", not_its_own)] {
        let listed = listed_revision(&page, 2);

        assert!(
            listed.as_ref().is_err_and(|err| err.is_internal()),
            "{named}: {listed:?}"
        );
    }
}
