// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::revision_graph::record::BranchRecordOut;
use lore::revision_graph::record::emit_branch;
use lore::revision_graph::record::emit_local_branch;
use lore::revision_graph::record::emit_location_complete;
use lore::revision_graph::record::emit_revision;
use lore::revision_graph::record::local_branch_record;
use lore::revision_graph::record::remote_branch_record;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::BranchAdvanced;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::event::LoreEvent;
use lore_revision::instance::InstanceId;
use lore_revision::instance::InstanceRecord;
use lore_revision::instance::InstanceStaleness;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreBranchPoint;
use lore_revision::interface::LoreString;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryContext;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::StoreCall;
use lore_transport::BranchRecord;

use crate::call::test_support::recording_callback;
use crate::call::test_support::without_log_lines;
use crate::revision_graph::support::CATEGORY;
use crate::revision_graph::support::CREATED;
use crate::revision_graph::support::create;
use crate::revision_graph::support::failing_store;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::partial_metadata;
use crate::revision_graph::support::store_corrupt_metadata;
use crate::revision_graph::support::store_metadata;
use crate::revision_graph::support::with_mutable_store;

const CREATOR: &str = "ana";

/// A write and a read context over in-memory stores, for a repository of the test's own.
async fn contexts(label: &str) -> (Arc<RepositoryContext>, Arc<RepositoryContext>) {
    let store = in_memory_for_tests(label).await;
    let repository = new_repository();
    (
        local_write_context(&store, repository),
        local_read_context(&store, repository),
    )
}

/// Runs `body` under an execution context of its own, and returns its output and every event the
/// context's callback received, in order: those `body` sent, then the `End` its drain sends.
async fn recorded<T>(body: impl Future<Output = T>) -> (T, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::new(callback),
    ));
    let output = LORE_CONTEXT.scope(execution.clone(), body).await;
    execution.dispatcher.drain().await;
    let events = std::mem::take(&mut *events.lock().unwrap());
    (output, events)
}

/// The value of the metadata key of `branch`.
async fn metadata_key(ctx: &Arc<RepositoryContext>, branch: BranchId) -> Hash {
    branch::mutable_load(ctx.clone(), branch::METADATA, branch)
        .await
        .unwrap_or_else(|err| panic!("loading the metadata key of {branch}: {err}"))
}

/// The local record of a branch with a latest and a divergent status, read through a context
/// whose mutable store fails the load of the branch's `function` key with `Disconnected` and
/// passes every other call to the store it wraps.
async fn record_with_a_failed_load(
    label: &str,
    function: &str,
) -> Result<Option<BranchRecordOut>, RevisionGraphError> {
    let store = in_memory_for_tests(label).await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    let id = new_branch();
    let failed = branch::mutable_key(write.salt(), function, repository, id).0;
    let failing = failing_store(&store, move |call, key, _| {
        call == StoreCall::Load && key == failed
    });
    let read = local_read_context(&failing, repository);

    in_setup_context(async {
        create(
            &write,
            id,
            "unreadable",
            CREATOR,
            Vec::new(),
            hash(3),
            BranchLatestStatus::Divergent,
        )
        .await;
        local_branch_record(read, id).await
    })
    .await
}

fn as_event_stack(stack: &[BranchPoint]) -> Vec<LoreBranchPoint> {
    stack.iter().map(LoreBranchPoint::from).collect()
}

/// A local record carries the branch's metadata, its latest and its latest status.
#[tokio::test]
async fn local_branch_record_carries_metadata_latest_and_status() {
    let (write, read) = contexts("record-carries").await;
    let divergent = new_branch();
    let convergent = new_branch();
    let stack = vec![
        BranchPoint {
            branch: new_branch(),
            revision: hash(1),
        },
        BranchPoint {
            branch: new_branch(),
            revision: hash(2),
        },
    ];

    let (divergent_record, convergent_record) = in_setup_context(async {
        create(
            &write,
            divergent,
            "Feature",
            CREATOR,
            stack.clone(),
            hash(7),
            BranchLatestStatus::Divergent,
        )
        .await;
        create(
            &write,
            convergent,
            "release",
            CREATOR,
            Vec::new(),
            hash(8),
            BranchLatestStatus::Convergent,
        )
        .await;
        (
            local_branch_record(read.clone(), divergent).await,
            local_branch_record(read.clone(), convergent).await,
        )
    })
    .await;

    let record = divergent_record.expect("reads").expect("a record");
    assert_eq!(record.branch, divergent);
    assert_eq!(record.name.as_bytes(), b"Feature");
    assert_eq!(record.category.as_bytes(), CATEGORY.as_bytes());
    assert_eq!(record.creator.as_bytes(), CREATOR.as_bytes());
    assert_eq!(record.created, CREATED);
    assert_eq!(record.stack.as_slice(), as_event_stack(&stack).as_slice());
    assert_eq!(record.latest, hash(7));
    assert!(record.divergent);
    assert!(!record.protect);
    assert!(!record.archived);

    let record = convergent_record.expect("reads").expect("a record");
    assert_eq!(record.name.as_bytes(), b"release");
    assert_eq!(record.latest, hash(8));
    assert!(!record.divergent);
    assert!(record.stack.is_empty());
}

/// A protected branch's local record says so.
#[tokio::test]
async fn local_branch_record_reports_protection() {
    let (write, read) = contexts("record-protected").await;
    let id = new_branch();

    let record = in_setup_context(async {
        create(
            &write,
            id,
            "guarded",
            CREATOR,
            Vec::new(),
            hash(3),
            BranchLatestStatus::Divergent,
        )
        .await;
        branch::protect(write.clone(), id)
            .await
            .expect("protects the branch");
        local_branch_record(read.clone(), id).await
    })
    .await;

    let record = record.expect("reads").expect("a record");
    assert!(record.protect);
}

/// A failed load of the metadata key fails the record, rather than reading as a miss.
#[tokio::test]
async fn local_branch_record_fails_when_the_metadata_key_load_fails() {
    let record = record_with_a_failed_load("record-metadata-fails", branch::METADATA).await;

    assert!(
        matches!(&record, Err(err) if err.is_disconnected()),
        "{record:?}"
    );
}

/// A failed load of the latest fails the record, rather than reporting a zero latest.
#[tokio::test]
async fn local_branch_record_fails_when_the_latest_load_fails() {
    let record = record_with_a_failed_load("record-latest-fails", branch::LATEST).await;

    assert!(
        matches!(&record, Err(err) if err.is_disconnected()),
        "{record:?}"
    );
}

/// A failed load of the latest status fails the record, rather than reporting the branch
/// convergent.
#[tokio::test]
async fn local_branch_record_fails_when_the_latest_status_load_fails() {
    let record = record_with_a_failed_load("record-status-fails", branch::LATEST_STATUS).await;

    assert!(
        matches!(&record, Err(err) if err.is_disconnected()),
        "{record:?}"
    );
}

/// A metadata key naming a blob the local store doesn't hold is a miss.
#[tokio::test]
async fn local_branch_record_is_none_without_metadata() {
    let (write, read) = contexts("record-no-blob").await;
    let id = new_branch();

    let record = in_setup_context(async {
        branch::mutable_store_metadata(write.clone(), id, hash(9))
            .await
            .expect("stores the metadata key");
        local_branch_record(read.clone(), id).await
    })
    .await;

    assert!(matches!(record, Ok(None)), "{record:?}");
}

/// A metadata key naming bytes that aren't a metadata blob fails the record, as a corrupt read
/// does, rather than reading as a miss.
#[tokio::test]
async fn local_branch_record_fails_on_a_corrupt_metadata_blob() {
    let (write, read) = contexts("record-corrupt-blob").await;
    let id = new_branch();

    let record = in_setup_context(async {
        store_corrupt_metadata(&write, id).await;
        local_branch_record(read.clone(), id).await
    })
    .await;

    assert!(
        matches!(&record, Err(err) if err.is_internal()),
        "{record:?}"
    );
}

/// A metadata blob without a name fails the record.
#[tokio::test]
async fn local_branch_record_fails_on_a_blob_without_a_name() {
    let (write, read) = contexts("record-nameless").await;
    let id = new_branch();

    let record = in_setup_context(async {
        store_metadata(
            &write,
            id,
            partial_metadata(
                Some(id),
                &[(branch::CATEGORY, CATEGORY), (branch::CREATOR, CREATOR)],
            ),
        )
        .await;
        local_branch_record(read.clone(), id).await
    })
    .await;

    assert!(
        matches!(&record, Err(err) if err.is_internal()),
        "{record:?}"
    );
}

/// A metadata blob with only an id and a name, as older clients wrote, gives a record with an
/// empty category and creator; a branch without a latest has a zero one.
#[tokio::test]
async fn local_branch_record_of_an_older_blob_has_an_empty_category_and_creator() {
    let (write, read) = contexts("record-older-blob").await;
    let id = new_branch();

    let record = in_setup_context(async {
        store_metadata(
            &write,
            id,
            partial_metadata(Some(id), &[(branch::NAME, "older")]),
        )
        .await;
        local_branch_record(read.clone(), id).await
    })
    .await;

    let record = record.expect("reads").expect("a record");
    assert_eq!(record.name.as_bytes(), b"older");
    assert!(record.category.is_empty());
    assert!(record.creator.is_empty());
    assert_eq!(record.created, 0);
    assert!(record.stack.is_empty());
    assert_eq!(record.latest, Hash::default());
    assert!(!record.divergent);
}

/// A branch whose metadata key holds zero has no record: one never created, and one whose key a
/// create swapped back to zero, as a create that loses its name claim does.
#[tokio::test]
async fn local_branch_record_is_none_for_a_zero_metadata_value() {
    let (write, read) = contexts("record-zero-key").await;
    let rolled_back = new_branch();

    let (never_created, after_rollback) = in_setup_context(async {
        create(
            &write,
            rolled_back,
            "lost",
            CREATOR,
            Vec::new(),
            Hash::default(),
            BranchLatestStatus::Convergent,
        )
        .await;
        let created = metadata_key(&write, rolled_back).await;
        let held = branch::mutable_try_store(
            write.clone(),
            branch::METADATA,
            rolled_back,
            created,
            Hash::default(),
        )
        .await
        .expect("swaps the metadata key");
        assert_eq!(held, created, "the swap back to zero is made");
        (
            local_branch_record(read.clone(), new_branch()).await,
            local_branch_record(read.clone(), rolled_back).await,
        )
    })
    .await;

    assert!(matches!(never_created, Ok(None)), "{never_created:?}");
    assert!(matches!(after_rollback, Ok(None)), "{after_rollback:?}");
}

/// On a store that keeps zero keys, a key holding zero reads as absent: a convergent branch, whose
/// latest status key holds zero there, isn't divergent, and a branch whose metadata key holds zero
/// has no record.
#[tokio::test]
async fn local_branch_record_reads_zero_values_as_absent_on_a_store_that_keeps_zero_keys() {
    let store = in_memory_for_tests("record-zero-keys").await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    let read = local_read_context(
        &with_mutable_store(
            &store,
            HookedStore::keeping_zero_keys(store.mutable.clone()),
        ),
        repository,
    );
    let convergent = new_branch();
    let (status_key, status_type) =
        branch::mutable_key(read.salt(), branch::LATEST_STATUS, repository, convergent);

    let (status, record, never_created) = in_setup_context(async {
        create(
            &write,
            convergent,
            "release",
            CREATOR,
            Vec::new(),
            hash(8),
            BranchLatestStatus::Convergent,
        )
        .await;
        (
            read.read_mutable_store()
                .load(repository, status_key, status_type)
                .await,
            local_branch_record(read.clone(), convergent).await,
            local_branch_record(read.clone(), new_branch()).await,
        )
    })
    .await;

    assert_eq!(status.expect("the store answers"), Hash::default());
    let record = record.expect("reads").expect("a record");
    assert_eq!(record.latest, hash(8));
    assert!(!record.divergent);
    assert!(matches!(never_created, Ok(None)), "{never_created:?}");
}

/// A remote record naming an unprotected metadata blob is unprotected, whether the remote reports
/// the branch live or deleted, and archived only where it reports it deleted.
#[tokio::test]
async fn remote_branch_record_of_an_unprotected_blob_is_unprotected() {
    let (write, read) = contexts("record-remote-unprotected").await;
    let id = new_branch();

    let (live, deleted) = in_setup_context(async {
        create(
            &write,
            id,
            "open",
            CREATOR,
            Vec::new(),
            hash(3),
            BranchLatestStatus::Divergent,
        )
        .await;
        let metadata = metadata_key(&read, id).await;
        let remote = |deleted| BranchRecord {
            id,
            name: "open".to_owned(),
            metadata,
            deleted,
            ..Default::default()
        };
        (
            remote_branch_record(read.clone(), remote(false)).await,
            remote_branch_record(read.clone(), remote(true)).await,
        )
    })
    .await;

    let live = live.expect("reads the blob");
    assert!(!live.protect);
    assert!(!live.archived);
    let deleted = deleted.expect("reads the blob");
    assert!(!deleted.protect);
    assert!(deleted.archived);
}

/// A remote record carries the remote's record of the branch, `protect` from the metadata blob
/// the record names, `archived` from `deleted`, and `divergent` clear. The local read context
/// stands in for the remote's, since it holds the blob.
#[tokio::test]
async fn remote_branch_record_reads_protect_from_the_blob_it_names() {
    let (write, read) = contexts("record-remote").await;
    let id = new_branch();
    let stack = vec![BranchPoint {
        branch: new_branch(),
        revision: hash(4),
    }];

    let record = in_setup_context(async {
        create(
            &write,
            id,
            "guarded",
            CREATOR,
            Vec::new(),
            hash(3),
            BranchLatestStatus::Divergent,
        )
        .await;
        branch::protect(write.clone(), id)
            .await
            .expect("protects the branch");
        let metadata = metadata_key(&read, id).await;
        let remote = BranchRecord {
            id,
            name: "remote name".to_owned(),
            category: "remote category".to_owned(),
            creator: "remote creator".to_owned(),
            created: 42,
            latest: hash(5),
            metadata,
            deleted: true,
            stack: stack.clone(),
        };
        remote_branch_record(read.clone(), remote).await
    })
    .await;

    let record = record.expect("reads the blob");
    assert_eq!(record.branch, id);
    assert_eq!(record.name.as_bytes(), b"remote name");
    assert_eq!(record.category.as_bytes(), b"remote category");
    assert_eq!(record.creator.as_bytes(), b"remote creator");
    assert_eq!(record.created, 42);
    assert_eq!(record.latest, hash(5));
    assert_eq!(record.stack.as_slice(), as_event_stack(&stack).as_slice());
    assert!(record.protect);
    assert!(record.archived);
    assert!(!record.divergent);
}

/// A remote record naming a metadata blob the store doesn't hold fails, rather than reading as
/// unprotected.
#[tokio::test]
async fn remote_branch_record_fails_without_the_blob_it_names() {
    let (_, read) = contexts("record-remote-no-blob").await;
    let remote = BranchRecord {
        id: new_branch(),
        name: "remote name".to_owned(),
        category: String::new(),
        creator: String::new(),
        created: 42,
        latest: hash(5),
        metadata: hash(9),
        deleted: false,
        stack: Vec::new(),
    };

    let record = in_setup_context(remote_branch_record(read.clone(), remote)).await;

    assert!(
        matches!(&record, Err(err) if err.is_internal()),
        "{record:?}"
    );
}

/// A record of `branch` with `point` as its stack, protected and divergent.
fn record_of(branch: BranchId, point: LoreBranchPoint) -> BranchRecordOut {
    BranchRecordOut {
        branch,
        name: LoreString::from("name"),
        category: LoreString::from("category"),
        latest: hash(2),
        stack: LoreArray::from_vec(vec![point]),
        creator: LoreString::from("creator"),
        created: 5,
        protect: true,
        archived: false,
        divergent: true,
    }
}

/// The branch record event carries the call's id, the location and the record.
#[tokio::test]
async fn emit_branch_echoes_the_call_id() {
    let branch = new_branch();
    let point = LoreBranchPoint {
        branch: new_branch(),
        revision: hash(1),
    };

    let (sent, events) =
        recorded(async { emit_branch(7, LoreBranchLocation::Remote, record_of(branch, point)) })
            .await;

    assert_eq!(sent, 1);
    let events = without_log_lines(&events);
    let [LoreEvent::RevisionGraphBranch(data), LoreEvent::End(_)] = events.as_slice() else {
        panic!(
            "expected one branch record, then End, got {} events",
            events.len()
        );
    };
    assert_eq!(data.id, 7);
    assert_eq!(data.location, LoreBranchLocation::Remote);
    assert_eq!(data.branch, branch);
    assert_eq!(data.name.as_bytes(), b"name");
    assert_eq!(data.category.as_bytes(), b"category");
    assert_eq!(data.latest, hash(2));
    assert_eq!(data.stack.as_slice(), [point]);
    assert_eq!(data.creator.as_bytes(), b"creator");
    assert_eq!(data.created, 5);
    assert_eq!((data.protect, data.archived, data.divergent), (1, 0, 1));
}

/// The local branch record, the instance records that follow it, and the revision and location
/// complete events carry the call's id, and the instance records carry the registrations they
/// report, at the local store.
#[tokio::test]
async fn every_emitter_echoes_the_call_id() {
    let branch = new_branch();
    let point = LoreBranchPoint {
        branch: new_branch(),
        revision: hash(1),
    };
    let instances = [
        InstanceRecord {
            instance_id: InstanceId::generate(),
            path: LoreString::from("/work/live"),
            revision: hash(4),
            staleness: InstanceStaleness::Active,
        },
        InstanceRecord {
            instance_id: InstanceId::generate(),
            path: LoreString::from("/work/gone"),
            revision: hash(5),
            staleness: InstanceStaleness::PathMissing,
        },
    ];
    let revision_branch = new_branch();

    let ((local_records, revision_records), events) = recorded(async {
        let sent = (
            emit_local_branch(7, record_of(branch, point), instances.to_vec()),
            emit_revision(7, LoreBranchLocation::Remote, hash(6), 3, revision_branch),
        );
        emit_location_complete(
            7,
            LoreBranchLocation::Remote,
            3,
            &Ok::<(), RevisionGraphError>(()),
        );
        sent
    })
    .await;

    assert_eq!((local_records, revision_records), (3, 1));
    let events = without_log_lines(&events);
    let [
        LoreEvent::RevisionGraphBranch(local),
        LoreEvent::RevisionGraphInstance(live),
        LoreEvent::RevisionGraphInstance(gone),
        LoreEvent::RevisionGraphRevision(revision),
        LoreEvent::RevisionGraphLocationComplete(complete),
        LoreEvent::End(_),
    ] = events.as_slice()
    else {
        panic!(
            "expected a branch record, two instance records, a revision record, a location complete event, then End, got {} events",
            events.len()
        );
    };
    assert_eq!(
        (local.id, local.location, local.branch),
        (7, LoreBranchLocation::Local, branch)
    );
    for (data, record) in [(live, &instances[0]), (gone, &instances[1])] {
        assert_eq!(data.id, 7);
        assert_eq!(data.location, LoreBranchLocation::Local);
        assert_eq!(data.instance_id, record.instance_id);
        assert_eq!(data.path, record.path);
        assert_eq!(data.branch, branch);
        assert_eq!(data.revision, record.revision);
        assert_eq!(data.stale, record.staleness.as_event_flag());
    }
    assert_eq!((live.stale, gone.stale), (0, 1));
    assert_eq!(revision.id, 7);
    assert_eq!(revision.location, LoreBranchLocation::Remote);
    assert_eq!(revision.revision_hash, hash(6));
    assert_eq!(revision.revision_number, 3);
    assert_eq!(revision.branch, revision_branch);
    assert_eq!(complete.id, 7);
    assert_eq!(complete.location, LoreBranchLocation::Remote);
}

/// The location complete event counts the location's records, with code 0 for a location that
/// succeeded and the error's code and message for one that failed.
#[tokio::test]
async fn emit_location_complete_reports_records_and_the_error_code() {
    let ((), events) = recorded(async {
        emit_location_complete(1, LoreBranchLocation::Remote, 2, &Ok::<(), _>(()));
        emit_location_complete(
            1,
            LoreBranchLocation::Local,
            0,
            &Err::<(), _>(RevisionGraphError::from(BranchAdvanced)),
        );
    })
    .await;

    let events = without_log_lines(&events);
    let [
        LoreEvent::RevisionGraphLocationComplete(succeeded),
        LoreEvent::RevisionGraphLocationComplete(failed),
        LoreEvent::End(_),
    ] = events.as_slice()
    else {
        panic!(
            "expected two location complete events, then End, got {} events",
            events.len()
        );
    };
    assert_eq!(succeeded.location, LoreBranchLocation::Remote);
    assert_eq!(succeeded.records, 2);
    assert_eq!(succeeded.error.error_code, 0);
    assert!(succeeded.error.message.is_empty());
    assert_eq!(failed.location, LoreBranchLocation::Local);
    assert_eq!(failed.records, 0);
    assert_eq!(failed.error.error_code, BranchAdvanced::FFI_CODE);
    assert!(!failed.error.message.is_empty());
}
