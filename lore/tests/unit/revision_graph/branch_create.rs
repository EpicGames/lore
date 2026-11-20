// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use lore::interface::LoreGlobalArgs;
use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::RevisionForm;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::branch_create::Checked;
use lore::revision_graph::branch_create::LoreRevisionGraphBranchCreateArgs;
use lore::revision_graph::branch_create::ParentAt;
use lore::revision_graph::branch_create::branch_create;
use lore::revision_graph::branch_create::classify_create_refusal;
use lore::revision_graph::branch_create::name_free_at;
use lore::revision_graph::branch_create::parent_at;
use lore::revision_graph::branch_create::settle;
use lore::revision_graph::branch_list::LoreRevisionGraphBranchListArgs;
use lore::revision_graph::branch_list::branch_list;
use lore::revision_graph::locations::PreCheck;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::revision_graph::locations::remote_revision_service;
use lore::revision_graph::record::local_branch_record;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::BoundFlags;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::error::BranchAlreadyExists;
use lore_base::error::BranchNotFound;
use lore_base::error::Disconnected;
use lore_base::error::Divergent;
use lore_base::error::InvalidArguments;
use lore_base::error::Maintenance;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::error::RevisionNotFound;
use lore_base::error::SlowDown;
use lore_base::lore_spawn;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_error_set::FfiError;
use lore_error_set::Internal;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::branch::NamedBranch;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchEventData;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreBranchPoint;
use lore_revision::interface::LoreString;
use lore_revision::repository::RepositoryContext;
use lore_revision::test_util::BranchStep;
use lore_revision::test_util::step_branch_writes;
use lore_revision::test_util::write_state_revision;
use lore_storage::local::store_lock::StoreHold;
use lore_transport::BranchListResponse;
use lore_transport::BranchPushError;
use lore_transport::BranchPushResponse;
use lore_transport::BranchRecord;
use lore_transport::MetadataSetResult;
use lore_transport::ProtocolError;
use lore_transport::Revision;
use lore_transport::RevisionListResponse;
use lore_transport::RevisionListStart;
use tokio::sync::Notify;

use crate::call::test_support::recording_callback;
use crate::call::test_support::take_without_log_lines;
use crate::revision_graph::support::CATEGORY;
use crate::revision_graph::support::CREATED;
use crate::revision_graph::support::NamingRevision;
use crate::revision_graph::support::QueryAnswer;
use crate::revision_graph::support::archive;
use crate::revision_graph::support::branch_records;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::create;
use crate::revision_graph::support::handle_failing;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::in_setup_context;
use crate::revision_graph::support::location_outcomes;
use crate::revision_graph::support::metadata_naming;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::revision_graph::support::store_latest;
use crate::revision_graph::support::store_metadata;
use crate::revision_graph::support::store_repository_metadata;
use crate::revision_graph::support::store_with_remote;
use crate::revision_graph::support::store_with_remote_as;
use crate::revision_graph::support::write_line;

/// A new in-memory store and a new repository id.
async fn local_store() -> (Arc<StoreInternal>, Partition) {
    (
        in_memory_for_tests("graph-branch-create").await,
        new_repository(),
    )
}

/// Runs [`parent_at`] for `form` in the local store of `store`, through a context that can't
/// write.
async fn parent_in_store(
    store: &StoreInternal,
    repository: Partition,
    form: RevisionForm,
) -> PreCheck<ParentAt> {
    in_setup_context(parent_at(
        &local_read_context(store, repository),
        None,
        form,
    ))
    .await
}

/// Runs [`name_free_at`] for `name` in the local store of `store`, through a context that can't
/// write.
async fn name_in_store(
    store: &StoreInternal,
    repository: Partition,
    name: &str,
) -> PreCheck<Context> {
    in_setup_context(name_free_at(
        &local_read_context(store, repository),
        None,
        name,
    ))
    .await
}

/// The FFI code of the refusal `check` answers, after asserting that the location's read didn't
/// fail.
fn refusal<T: std::fmt::Debug>(check: PreCheck<T>) -> i32 {
    match check {
        Ok(Err(refusal)) => refusal.ffi_code(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// What `check` found, after asserting that the location neither failed nor refused.
fn found<T: std::fmt::Debug>(check: PreCheck<T>) -> T {
    match check {
        Ok(Ok(found)) => found,
        other => panic!("expected an answer, got {other:?}"),
    }
}

/// Creates `branch` named `name` in category `category` through `write`, with `stack`, at
/// `latest`.
async fn create_in_category(
    write: &Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
    category: &str,
    stack: Vec<BranchPoint>,
    latest: Hash,
) {
    branch::create_record(
        write.clone(),
        branch,
        name,
        category,
        "alice",
        CREATED,
        stack,
        latest,
        BranchLatestStatus::Divergent,
        Context::default(),
    )
    .await
    .expect("creates the branch");
}

/// A branch's latest revision is the branch point, and its stack follows it.
#[tokio::test]
async fn parent_at_a_branch_takes_its_latest_and_stack() {
    let (store, repository) = local_store().await;
    let (parent, root) = (new_branch(), new_branch());
    let stack = vec![BranchPoint {
        branch: root,
        revision: hash(3),
    }];
    let write = local_write_context(&store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, parent, 2).await;
        create(
            &write,
            parent,
            "parent",
            "alice",
            stack.clone(),
            line[1],
            BranchLatestStatus::Divergent,
        )
        .await;
        line
    })
    .await;

    let at = found(parent_in_store(&store, repository, RevisionForm::Latest(parent)).await);

    assert_eq!(
        at,
        ParentAt {
            parent,
            branch_point: line[1],
            stack: Some(stack),
            default_unchecked: false,
        }
    );
}

/// A hash starts the branch at that revision, on the branch its metadata names.
#[tokio::test]
async fn parent_at_a_hash_takes_the_revisions_own_branch() {
    let (store, repository) = local_store().await;
    let parent = new_branch();
    let write = local_write_context(&store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, parent, 2).await;
        create(
            &write,
            parent,
            "parent",
            "alice",
            Vec::new(),
            line[1],
            BranchLatestStatus::Divergent,
        )
        .await;
        line
    })
    .await;

    let at = found(parent_in_store(&store, repository, RevisionForm::Hash(line[0])).await);

    assert_eq!(
        at,
        ParentAt {
            parent,
            branch_point: line[0],
            stack: Some(Vec::new()),
            default_unchecked: false,
        }
    );
}

/// A revision number starts the branch at the revision with that number on the branch, not at its
/// latest.
#[tokio::test]
async fn parent_at_a_number_branches_at_that_revision() {
    let (store, repository) = local_store().await;
    let parent = new_branch();
    let write = local_write_context(&store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, parent, 3).await;
        create(
            &write,
            parent,
            "parent",
            "alice",
            Vec::new(),
            line[2],
            BranchLatestStatus::Divergent,
        )
        .await;
        line
    })
    .await;

    let at = found(parent_in_store(&store, repository, RevisionForm::Number(parent, 2)).await);

    assert_eq!((at.parent, at.branch_point), (parent, line[1]));
    assert_eq!(at.stack, Some(Vec::new()));
}

/// A parent with metadata and no latest revision starts the branch at a zero branch point, which
/// only the repository's default branch can have.
#[tokio::test]
async fn a_zero_branch_point_off_the_default_branch_fails_invalid_arguments() {
    let (store, repository) = local_store().await;
    let (parent, default) = (new_branch(), new_branch());
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        create(
            &write,
            parent,
            "parent",
            "alice",
            Vec::new(),
            Hash::default(),
            BranchLatestStatus::Divergent,
        )
        .await;
        store_repository_metadata(&write, default).await;
    })
    .await;

    let check = parent_in_store(&store, repository, RevisionForm::Latest(parent)).await;

    assert_eq!(refusal(check), InvalidArguments::FFI_CODE);
}

/// The default branch without a latest revision starts a branch at a zero branch point.
#[tokio::test]
async fn a_zero_branch_point_on_the_default_branch_is_where_the_branch_starts() {
    let (store, repository) = local_store().await;
    let default = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        create(
            &write,
            default,
            "main",
            "alice",
            Vec::new(),
            Hash::default(),
            BranchLatestStatus::Divergent,
        )
        .await;
        store_repository_metadata(&write, default).await;
    })
    .await;

    let at = found(parent_in_store(&store, repository, RevisionForm::Latest(default)).await);

    assert_eq!(
        (at.branch_point, at.default_unchecked),
        (Hash::default(), false)
    );
}

/// A local store without repository metadata can't tell whether the parent of a zero branch point
/// is the default branch, and says so.
#[tokio::test]
async fn a_zero_branch_point_without_repository_metadata_is_unchecked() {
    let (store, repository) = local_store().await;
    let parent = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(create(
        &write,
        parent,
        "parent",
        "alice",
        Vec::new(),
        Hash::default(),
        BranchLatestStatus::Divergent,
    ))
    .await;

    let at = found(parent_in_store(&store, repository, RevisionForm::Latest(parent)).await);

    assert_eq!(
        (at.branch_point, at.default_unchecked),
        (Hash::default(), true)
    );
}

/// A personal branch can't have children, whichever form names it.
#[tokio::test]
async fn a_personal_parent_fails_invalid_arguments() {
    let (store, repository) = local_store().await;
    let parent = new_branch();
    let write = local_write_context(&store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, parent, 1).await;
        create_in_category(
            &write,
            parent,
            "mine",
            branch::personal_category(),
            Vec::new(),
            line[0],
        )
        .await;
        line
    })
    .await;

    for form in [
        RevisionForm::Latest(parent),
        RevisionForm::Number(parent, 1),
        RevisionForm::Hash(line[0]),
    ] {
        let check = parent_in_store(&store, repository, form).await;
        assert_eq!(refusal(check), InvalidArguments::FFI_CODE, "{form:?}");
    }
}

/// A number past the branch's latest, and a number on a branch with metadata and no latest
/// revision, fail with `RevisionNotFound`.
#[tokio::test]
async fn a_number_the_location_does_not_hold_fails_revision_not_found() {
    let (store, repository) = local_store().await;
    let (parent, empty) = (new_branch(), new_branch());
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        let line = write_line(&write, parent, 2).await;
        create(
            &write,
            parent,
            "parent",
            "alice",
            Vec::new(),
            line[1],
            BranchLatestStatus::Divergent,
        )
        .await;
        create(
            &write,
            empty,
            "empty",
            "alice",
            Vec::new(),
            Hash::default(),
            BranchLatestStatus::Divergent,
        )
        .await;
    })
    .await;

    for form in [
        RevisionForm::Number(parent, 3),
        RevisionForm::Number(empty, 1),
    ] {
        let check = parent_in_store(&store, repository, form).await;
        assert_eq!(refusal(check), RevisionNotFound::FFI_CODE, "{form:?}");
    }
}

/// A branch with neither metadata nor a latest revision fails with `BranchNotFound`, by its latest
/// and by a number, and a hash the store doesn't hold with `RevisionNotFound`.
#[tokio::test]
async fn parent_at_a_branch_the_location_lacks_fails_branch_not_found() {
    let (store, repository) = local_store().await;
    let absent = new_branch();

    for (form, code) in [
        (RevisionForm::Latest(absent), BranchNotFound::FFI_CODE),
        (RevisionForm::Number(absent, 1), BranchNotFound::FFI_CODE),
        (RevisionForm::Hash(hash(4)), RevisionNotFound::FFI_CODE),
    ] {
        let check = parent_in_store(&store, repository, form).await;
        assert_eq!(refusal(check), code, "{form:?}");
    }
}

/// A revision whose metadata names no branch has parent zero and no stack: the call takes the
/// branch from another location, or fails.
#[tokio::test]
async fn parent_at_a_hash_without_a_recorded_branch_answers_parent_zero() {
    let (store, repository) = local_store().await;
    let write = local_write_context(&store, repository);
    let revision = in_setup_context(async {
        let unnamed = metadata_naming(&write, BranchId::default()).await;
        write_state_revision(&write, Hash::default(), 1, unnamed).await
    })
    .await;

    let at = found(parent_in_store(&store, repository, RevisionForm::Hash(revision)).await);

    assert_eq!(
        at,
        ParentAt {
            parent: BranchId::default(),
            branch_point: revision,
            stack: None,
            default_unchecked: false,
        }
    );
}

/// A parent the local store holds a latest revision for, and no metadata, starts the branch at
/// that latest without a stack.
#[tokio::test]
async fn a_parent_without_local_metadata_has_no_parent_record() {
    let (store, repository) = local_store().await;
    let parent = new_branch();
    store_latest(&store, repository, parent, hash(5)).await;

    let at = found(parent_in_store(&store, repository, RevisionForm::Latest(parent)).await);

    assert_eq!(
        at,
        ParentAt {
            parent,
            branch_point: hash(5),
            stack: None,
            default_unchecked: false,
        }
    );
}

/// A failed read of the parent fails the location rather than refusing the call.
#[tokio::test]
async fn a_failed_parent_read_fails_the_location() {
    let (store, repository) = local_store().await;
    let parent = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(crate::revision_graph::support::store_corrupt_metadata(
        &write, parent,
    ))
    .await;

    let check = parent_in_store(&store, repository, RevisionForm::Latest(parent)).await;

    assert!(check.is_err(), "{check:?}");
}

/// A name a live branch holds is taken. An archived name, and a stale mapping to a branch without
/// metadata, are free: the first claims from zero, the second from the stale id.
#[tokio::test]
async fn name_free_at_refuses_a_live_name_and_returns_an_archived_names_mapping() {
    let (store, repository) = local_store().await;
    let (live, archived, stale) = (new_branch(), new_branch(), new_branch());
    let write = local_write_context(&store, repository);
    in_setup_context(async {
        create(
            &write,
            live,
            "live",
            "alice",
            Vec::new(),
            hash(1),
            BranchLatestStatus::Divergent,
        )
        .await;
        create(
            &write,
            archived,
            "archived",
            "alice",
            Vec::new(),
            hash(1),
            BranchLatestStatus::Divergent,
        )
        .await;
        branch::claim_name(write.clone(), "stale", Context::default(), stale)
            .await
            .expect("maps the stale name");
    })
    .await;
    archive(&store, repository, archived, "archived").await;

    assert_eq!(
        refusal(name_in_store(&store, repository, "LIVE").await),
        BranchAlreadyExists::FFI_CODE
    );
    assert_eq!(
        found(name_in_store(&store, repository, "archived").await),
        Context::default()
    );
    assert_eq!(
        found(name_in_store(&store, repository, "stale").await),
        stale
    );
    assert_eq!(
        found(name_in_store(&store, repository, "unused").await),
        Context::default()
    );
}

/// A name with no mapping whose derived id has a latest revision and no metadata key is free, and
/// claims from zero.
#[tokio::test]
async fn name_free_at_expects_zero_when_only_a_derived_id_answers() {
    let (store, repository) = local_store().await;
    store_latest(&store, repository, branch::fallback_id("legacy"), hash(2)).await;

    assert_eq!(
        found(name_in_store(&store, repository, "legacy").await),
        Context::default()
    );
}

/// A name with no mapping whose derived id has a latest revision and a metadata key that doesn't
/// say the branch left the name is taken.
#[tokio::test]
async fn name_free_at_refuses_a_derived_id_with_metadata() {
    let (store, repository) = local_store().await;
    let derived = branch::fallback_id("legacy");
    store_latest(&store, repository, derived, hash(2)).await;
    let write = local_write_context(&store, repository);
    in_setup_context(store_metadata(
        &write,
        derived,
        crate::revision_graph::support::partial_metadata(
            Some(derived),
            &[(branch::NAME, "legacy"), (branch::CATEGORY, CATEGORY)],
        ),
    ))
    .await;

    assert_eq!(
        refusal(name_in_store(&store, repository, "legacy").await),
        BranchAlreadyExists::FFI_CODE
    );
}

/// Runs `check` on the remote read context and revision service of `store`.
async fn at_remote<T>(
    store: &StoreInternal,
    repository: Partition,
    check: impl AsyncFnOnce(&Arc<RepositoryContext>, &Arc<dyn Revision>) -> T,
) -> T {
    let remote = store.remote.clone().expect("the store has a remote");
    in_setup_context(async {
        let (ctx, service) = remote_revision_service(store, repository, &remote)
            .await
            .expect("reaches the remote");
        check(&ctx, &service).await
    })
    .await
}

/// At the remote, a name a live branch holds is taken, a name no branch holds is free, and a
/// failed query fails the location.
#[tokio::test]
async fn name_free_at_the_remote_asks_for_the_name() {
    let repository = new_repository();
    for (answer, expected) in [
        (
            QueryAnswer::Record(new_branch()),
            Some(Err(BranchAlreadyExists::FFI_CODE)),
        ),
        (QueryAnswer::NotFound, Some(Ok(Context::default()))),
        (QueryAnswer::SlowDown, None),
    ] {
        let service = NamingRevision::new(answer);
        let store = store_with_remote(repository, service.clone()).await;
        let check = at_remote(&store, repository, async |ctx, service| {
            name_free_at(ctx, Some(service), "feature").await
        })
        .await;
        let outcome = check
            .map(|answer| answer.map_err(|refusal: RevisionGraphError| refusal.ffi_code()))
            .map_err(|failure| failure.ffi_code());
        assert_eq!(outcome, expected.ok_or(SlowDown::FFI_CODE));
        assert_eq!(
            *service.queries.lock().unwrap(),
            [(None, Some("feature".to_owned()))]
        );
    }
}

/// At the remote, a parent the remote doesn't hold fails with `BranchNotFound`, and a failed query
/// fails the location.
#[tokio::test]
async fn parent_at_the_remote_queries_the_parent() {
    let repository = new_repository();
    let parent = new_branch();
    for (answer, expected) in [
        (QueryAnswer::NotFound, Ok(BranchNotFound::FFI_CODE)),
        (QueryAnswer::SlowDown, Err(SlowDown::FFI_CODE)),
    ] {
        let service = NamingRevision::new(answer);
        let store = store_with_remote(repository, service.clone()).await;
        let check = at_remote(&store, repository, async |ctx, service| {
            parent_at(ctx, Some(service), RevisionForm::Latest(parent)).await
        })
        .await;
        let outcome = match check {
            Ok(Err(refusal)) => Ok(refusal.ffi_code()),
            Err(failure) => Err(failure.ffi_code()),
            Ok(Ok(at)) => panic!("expected no parent, got {at:?}"),
        };
        assert_eq!(outcome, expected);
        assert_eq!(*service.queries.lock().unwrap(), [(Some(parent), None)]);
    }
}

/// The `id` every call of the verb in this file passes.
const CALL_ID: u64 = 36;

/// Runs `branch_create` on `store` in `repository` under `globals`, for a branch named `name` in
/// `category` from `from`, and returns its status and the events it sent other than log lines.
async fn create_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    repository: Partition,
    name: &str,
    category: &str,
    from: LoreRevisionIdentifier,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_create(
        globals,
        LoreRevisionGraphBranchCreateArgs {
            id: CALL_ID,
            store,
            repository,
            name: LoreString::from(name),
            category: LoreString::from(category),
            from,
        },
        callback,
    )
    .await;
    (status, take_without_log_lines(&events))
}

/// [`create_with`] under default globals, in [`CATEGORY`].
async fn create_branch(
    store: LoreStore,
    repository: Partition,
    name: &str,
    from: LoreRevisionIdentifier,
) -> (i32, Vec<LoreEvent>) {
    create_with(
        LoreGlobalArgs::default(),
        store,
        repository,
        name,
        CATEGORY,
        from,
    )
    .await
}

/// The identifier of the latest revision of `branch`.
fn latest_of(branch: BranchId) -> LoreRevisionIdentifier {
    LoreRevisionIdentifier {
        branch,
        ..Default::default()
    }
}

/// Creates `main` in the local store of `store` at the second revision of a line of two, with an
/// empty stack, and returns its id and the line.
async fn seed_main(store: &StoreInternal, repository: Partition) -> (BranchId, Vec<Hash>) {
    let main = new_branch();
    let write = local_write_context(store, repository);
    let line = in_setup_context(async {
        let line = write_line(&write, main, 2).await;
        create(
            &write,
            main,
            "main",
            "alice",
            Vec::new(),
            line[1],
            BranchLatestStatus::Convergent,
        )
        .await;
        line
    })
    .await;
    (main, line)
}

/// Asserts that `events` holds only the call's terminal events: `Complete`, then `End`.
fn assert_terminal_only(events: &[LoreEvent]) {
    let [LoreEvent::Complete(_), LoreEvent::End(_)] = events else {
        panic!("expected Complete, then End");
    };
}

/// The one branch record of `events`, after asserting that it is the local store's, echoes the
/// call's id, and is followed by the local store's location complete event with one record.
fn created_record(events: &[LoreEvent]) -> &LoreRevisionGraphBranchEventData {
    let [record] = branch_records(events)[..] else {
        panic!("expected one branch record");
    };
    assert_eq!(
        (record.id, record.location),
        (CALL_ID, LoreBranchLocation::Local)
    );
    assert_eq!(
        location_outcomes(events, CALL_ID),
        [(LoreBranchLocation::Local, 1, 0)]
    );
    record
}

/// A name that isn't valid, a category or creator over Lore Server's limit, and a malformed
/// identifier fail with `InvalidArguments` before any read: no location complete event, and no
/// `NoRemote` for the `remote` flag on a handle without a remote.
#[tokio::test]
async fn an_invalid_name_fails_before_any_read() {
    let (_store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let long = "x".repeat(1001);
    let remote_only = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };
    let too_long_creator = LoreGlobalArgs {
        identity: LoreString::from(long.as_str()),
        ..Default::default()
    };
    let valid = latest_of(new_branch());
    for (globals, name, category, from) in [
        (remote_only.clone(), "", CATEGORY, valid),
        (remote_only.clone(), long.as_str(), CATEGORY, valid),
        (
            remote_only.clone(),
            "01a1e5bbee4570f9bfb124ef87745907",
            CATEGORY,
            valid,
        ),
        (remote_only.clone(), "feature", long.as_str(), valid),
        (
            remote_only.clone(),
            "feature",
            CATEGORY,
            LoreRevisionIdentifier::default(),
        ),
        (too_long_creator, "feature", CATEGORY, valid),
    ] {
        let (status, events) = create_with(globals, handle, repository, name, category, from).await;
        assert_eq!(
            status,
            InvalidArguments::FFI_CODE,
            "{name:?} in {category:?}"
        );
        assert_terminal_only(&events);
    }
    let at_limit = "x".repeat(1000);
    let (status, _) = create_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        "feature",
        at_limit.as_str(),
        valid,
    )
    .await;
    assert_eq!(
        status,
        BranchNotFound::FFI_CODE,
        "a category at the limit is valid"
    );
}

/// A local create from a branch's latest starts at that latest with the branch point on the
/// parent, then the parent's stack, records the values it wrote, and starts divergent.
#[tokio::test]
async fn a_local_create_from_a_branch_builds_the_stack_and_starts_divergent() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, line) = seed_main(&store, repository).await;

    let (status, events) = create_branch(handle, repository, "feature", latest_of(main)).await;

    assert_eq!(status, 0);
    let record = created_record(&events);
    let stack = [BranchPoint {
        branch: main,
        revision: line[1],
    }];
    assert_eq!(record.name.as_str(), "feature");
    assert_eq!(record.category.as_str(), CATEGORY);
    assert_eq!(record.latest, line[1]);
    assert_eq!(
        record.stack.as_slice(),
        stack.map(|point| LoreBranchPoint::from(&point))
    );
    assert_eq!(
        (record.protect, record.archived, record.divergent),
        (0, 0, 1)
    );
    let stored = in_setup_context(local_branch_record(
        local_read_context(&store, repository),
        record.branch,
    ))
    .await
    .expect("reads the new branch")
    .expect("the new branch has metadata");
    assert_eq!(stored.name.as_str(), "feature");
    assert_eq!(stored.latest, line[1]);
    assert_eq!(stored.stack.as_slice(), record.stack.as_slice());
    assert_eq!(
        (stored.created, stored.creator.as_str()),
        (record.created, record.creator.as_str())
    );
    assert!(stored.divergent);
    let named = in_setup_context(branch::lookup_name_local_unwritten(
        local_read_context(&store, repository),
        "feature",
    ))
    .await
    .expect("looks up the name");
    assert_eq!(named.map(|named| named.branch()), Some(record.branch));
    close_handle(handle).await;
}

/// A create from a hash starts at that revision on the branch its metadata names.
#[tokio::test]
async fn a_local_create_from_a_hash_branches_at_that_revision() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, line) = seed_main(&store, repository).await;
    let from = LoreRevisionIdentifier {
        hash: line[0],
        ..Default::default()
    };

    let (status, events) = create_branch(handle, repository, "feature", from).await;

    assert_eq!(status, 0);
    let record = created_record(&events);
    assert_eq!(record.latest, line[0]);
    assert_eq!(
        record.stack.as_slice(),
        [LoreBranchPoint::from(&BranchPoint {
            branch: main,
            revision: line[0],
        })]
    );
    close_handle(handle).await;
}

/// A name a live branch holds fails the call with `BranchAlreadyExists`, a refused pre-check: no
/// record, no location complete event, and nothing written.
#[tokio::test]
async fn a_live_name_fails_branch_already_exists() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;

    let (status, events) = create_branch(handle, repository, "MAIN", latest_of(main)).await;

    assert_eq!(status, BranchAlreadyExists::FFI_CODE);
    assert_terminal_only(&events);
    let named = in_setup_context(branch::lookup_name_local_unwritten(
        local_read_context(&store, repository),
        "main",
    ))
    .await
    .expect("looks up the name");
    assert_eq!(named.map(|named| named.branch()), Some(main));
    close_handle(handle).await;
}

/// A refused pre-check emits no location complete event, and a failed pre-check read emits the
/// location's, with its error.
#[tokio::test]
async fn a_refused_pre_check_emits_no_location_complete_event_and_a_failed_read_emits_one() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;

    let (status, events) = create_branch(handle, repository, "main", latest_of(main)).await;
    assert_eq!(status, BranchAlreadyExists::FFI_CODE);
    assert!(location_outcomes(&events, CALL_ID).is_empty());

    let failing = handle_failing(&store, |_, _, key_type| key_type == KeyType::BranchMetadata);
    let (status, events) = create_branch(failing, repository, "feature", latest_of(main)).await;
    assert_eq!(status, Disconnected::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, Disconnected::FFI_CODE)]
    );
    assert!(branch_records(&events).is_empty());
    close_handle(handle).await;
}

/// An archived name gets a new branch with a new id, and the archived branch stays listable with
/// `include_archived`.
#[tokio::test]
async fn an_archived_name_gets_a_new_id() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;
    let (status, events) = create_branch(handle, repository, "feature", latest_of(main)).await;
    assert_eq!(status, 0);
    let first = created_record(&events).branch;
    archive(&store, repository, first, "feature").await;

    let (status, events) = create_branch(handle, repository, "feature", latest_of(main)).await;

    assert_eq!(status, 0);
    let second = created_record(&events).branch;
    assert_ne!(first, second);
    let (status, events) = list_with_archived(handle, repository).await;
    assert_eq!(status, 0);
    let listed: Vec<_> = branch_records(&events)
        .into_iter()
        .map(|record| (record.branch, record.archived))
        .collect();
    assert!(listed.contains(&(first, 1)), "{listed:?}");
    assert!(listed.contains(&(second, 0)), "{listed:?}");
    close_handle(handle).await;
}

/// Lists the branches of `repository` on `store`, archived ones included.
async fn list_with_archived(store: LoreStore, repository: Partition) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_list(
        LoreGlobalArgs::default(),
        LoreRevisionGraphBranchListArgs {
            id: CALL_ID,
            store,
            repository,
            include_archived: 1,
            ..Default::default()
        },
        callback,
    )
    .await;
    (status, take_without_log_lines(&events))
}

/// A name whose mapping names a branch without metadata, as a lost create leaves it, is free, and
/// the create claims it over that stale mapping.
#[tokio::test]
async fn a_stale_mapping_is_claimed_over() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;
    let stale = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(branch::claim_name(
        write,
        "feature",
        Context::default(),
        stale,
    ))
    .await
    .expect("maps the stale name");

    let (status, events) = create_branch(handle, repository, "feature", latest_of(main)).await;

    assert_eq!(status, 0);
    let record = created_record(&events);
    let named = in_setup_context(branch::lookup_name_local_unwritten(
        local_read_context(&store, repository),
        "feature",
    ))
    .await
    .expect("looks up the name");
    assert_eq!(named, Some(NamedBranch::Mapped(record.branch)));
    close_handle(handle).await;
}

/// Each refusal of the remote's create maps to its row: a confirming record of the call's own
/// branch carries on, one of another branch is a taken name, the refusals of the caller, an
/// overloaded remote and a lost connection keep their codes, and any other refusal is internal.
#[test]
fn classify_create_refusal_maps_each_row() {
    let id = new_branch();
    let record = |branch: BranchId, deleted: bool| BranchRecord {
        id: branch,
        name: "feature".to_owned(),
        deleted,
        ..Default::default()
    };
    let code =
        |outcome: Result<BranchRecord, RevisionGraphError>| outcome.map_err(|err| err.ffi_code());

    assert_eq!(
        classify_create_refusal(
            ProtocolError::from(Disconnected),
            Some(record(id, false)),
            id
        )
        .map(|record| record.id)
        .ok(),
        Some(id)
    );
    for (refusal, confirming, expected) in [
        (
            ProtocolError::internal("taken"),
            Some(record(new_branch(), false)),
            BranchAlreadyExists::FFI_CODE,
        ),
        (
            ProtocolError::from(NotAuthorized),
            None,
            NotAuthorized::FFI_CODE,
        ),
        (
            ProtocolError::from(NotAuthenticated),
            None,
            NotAuthenticated::FFI_CODE,
        ),
        (ProtocolError::from(SlowDown), None, SlowDown::FFI_CODE),
        (
            ProtocolError::from(Disconnected),
            None,
            Disconnected::FFI_CODE,
        ),
        (
            ProtocolError::from(Maintenance),
            None,
            Maintenance::FFI_CODE,
        ),
        (
            ProtocolError::from(SlowDown),
            Some(record(id, true)),
            SlowDown::FFI_CODE,
        ),
        (ProtocolError::from(NotFound), None, Internal::FFI_CODE),
        (
            ProtocolError::internal("refused"),
            Some(record(new_branch(), true)),
            Internal::FFI_CODE,
        ),
    ] {
        assert_eq!(
            code(classify_create_refusal(refusal, confirming, id)).map(|_| ()),
            Err(expected)
        );
    }
}

/// With `local` alone, a parent the local store holds a latest for and no metadata fails with
/// `BranchNotFound`: the call can't build the new branch's stack.
#[tokio::test]
async fn a_local_parent_without_metadata_fails_branch_not_found_with_the_local_flag() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let parent = new_branch();
    store_latest(&store, repository, parent, hash(5)).await;
    let local = LoreGlobalArgs {
        local: 1,
        ..Default::default()
    };

    let (status, events) = create_with(
        local,
        handle,
        repository,
        "feature",
        CATEGORY,
        latest_of(parent),
    )
    .await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// A parent the local store doesn't hold fails with `BranchNotFound`, and a revision whose
/// metadata names no branch with `BranchNotFound` too, since no location records its branch.
#[tokio::test]
async fn a_local_create_from_a_branch_the_store_lacks_fails_branch_not_found() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let write = local_write_context(&store, repository);
    let unnamed = in_setup_context(async {
        let metadata = metadata_naming(&write, BranchId::default()).await;
        write_state_revision(&write, Hash::default(), 1, metadata).await
    })
    .await;

    for from in [
        latest_of(new_branch()),
        LoreRevisionIdentifier {
            hash: unnamed,
            ..Default::default()
        },
    ] {
        let (status, events) = create_branch(handle, repository, "feature", from).await;
        assert_eq!(status, BranchNotFound::FFI_CODE, "{from:?}");
        assert_terminal_only(&events);
    }
    close_handle(handle).await;
}

/// With `local` alone, a zero branch point the local store can't check against the default branch,
/// for want of repository metadata, fails with `InvalidArguments`.
#[tokio::test]
async fn a_zero_branch_point_the_local_store_cannot_check_fails_with_the_local_flag() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let parent = new_branch();
    let write = local_write_context(&store, repository);
    in_setup_context(create(
        &write,
        parent,
        "parent",
        "alice",
        Vec::new(),
        Hash::default(),
        BranchLatestStatus::Divergent,
    ))
    .await;

    let (status, events) = create_branch(handle, repository, "feature", latest_of(parent)).await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_terminal_only(&events);
    close_handle(handle).await;
}

/// Without a user id, the creator is `globals.identity`.
#[tokio::test]
async fn the_creator_falls_back_to_globals_identity() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;
    let globals = LoreGlobalArgs {
        identity: LoreString::from("alice"),
        ..Default::default()
    };

    let (status, events) = create_with(
        globals,
        handle,
        repository,
        "feature",
        CATEGORY,
        latest_of(main),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(created_record(&events).creator.as_str(), "alice");
    close_handle(handle).await;
}

/// Without a user id or `globals.identity`, the creator is the identity the storage handle was
/// opened with.
#[tokio::test]
async fn the_creator_falls_back_to_the_handle_identity() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;
    let bob = handle::register(Arc::new(StoreInternal::new(
        "bob",
        store.immutable.clone(),
        store.mutable.clone(),
        None,
        BoundFlags::default(),
        false,
        StoreHold::default(),
    )));

    let (status, events) = create_branch(bob, repository, "feature", latest_of(main)).await;

    assert_eq!(status, 0);
    assert_eq!(created_record(&events).creator.as_str(), "bob");
    close_handle(handle).await;
}

/// An empty category records the default category.
#[tokio::test]
async fn an_empty_category_records_the_default_category() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;

    let (status, events) = create_with(
        LoreGlobalArgs::default(),
        handle,
        repository,
        "feature",
        "",
        latest_of(main),
    )
    .await;

    assert_eq!(status, 0);
    let record = created_record(&events);
    assert_eq!(record.category.as_str(), branch::default_category());
    let stored = in_setup_context(branch::local_metadata(
        local_read_context(&store, repository),
        record.branch,
    ))
    .await
    .expect("reads the metadata")
    .expect("the branch has metadata")
    .1;
    assert_eq!(
        branch::category(&stored).ok(),
        Some(branch::default_category())
    );
    close_handle(handle).await;
}

/// `globals.dry_run` doesn't stop the create.
#[tokio::test]
async fn globals_dry_run_is_ignored() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;
    let dry_run = LoreGlobalArgs {
        dry_run: 1,
        ..Default::default()
    };

    let (status, events) = create_with(
        dry_run,
        handle,
        repository,
        "feature",
        CATEGORY,
        latest_of(main),
    )
    .await;

    assert_eq!(status, 0);
    let branch = created_record(&events).branch;
    let stored = in_setup_context(branch::local_metadata(
        local_read_context(&store, repository),
        branch,
    ))
    .await
    .expect("reads the metadata");
    assert!(stored.is_some(), "the create wrote the branch");
    close_handle(handle).await;
}

/// A [`Checked`] of the local store or the remote whose parent is `parent` and whose name claims
/// from zero.
fn checked(parent: Result<ParentAt, RevisionGraphError>) -> Checked {
    Checked {
        parent,
        name: Ok(Context::default()),
    }
}

/// Where `parent` starts a branch at `branch_point`, with `stack`.
fn start(parent: BranchId, branch_point: Hash, stack: Option<Vec<BranchPoint>>) -> ParentAt {
    ParentAt {
        parent,
        branch_point,
        stack,
        default_unchecked: false,
    }
}

/// The code of the error `settle` fails with.
fn settle_code(form: RevisionForm, remote: Option<Checked>, local: Option<Checked>) -> i32 {
    match settle(form, remote, local) {
        Err(err) => err.ffi_code(),
        Ok(settled) => panic!("expected a refusal, got {settled:?}"),
    }
}

/// Branch points that differ between the locations fail with `Divergent`, and a branch point one
/// location can't resolve with `RevisionNotFound`, whichever location lacks it.
#[test]
fn settle_requires_one_branch_point_at_every_location() {
    let parent = new_branch();
    let form = RevisionForm::Latest(parent);
    let missing_branch = || {
        Err(RevisionGraphError::from(BranchNotFound {
            branch: parent.to_string(),
        }))
    };
    let missing_revision = || {
        Err(RevisionGraphError::from(RevisionNotFound {
            revision: "r".to_owned(),
        }))
    };
    let at = |revision| Ok(start(parent, revision, Some(Vec::new())));

    assert_eq!(
        settle_code(form, Some(checked(at(hash(1)))), Some(checked(at(hash(2))))),
        Divergent::FFI_CODE
    );
    for (remote, local) in [
        (at(hash(1)), missing_branch()),
        (missing_branch(), at(hash(1))),
        (at(hash(1)), missing_revision()),
        (missing_revision(), missing_branch()),
        (missing_branch(), missing_revision()),
    ] {
        assert_eq!(
            settle_code(form, Some(checked(remote)), Some(checked(local))),
            RevisionNotFound::FFI_CODE
        );
    }
    assert_eq!(
        settle_code(
            form,
            Some(checked(missing_branch())),
            Some(checked(missing_branch()))
        ),
        BranchNotFound::FFI_CODE
    );
}

/// A revision one location holds without its branch takes the branch and the stack the other
/// location records, and fails with `BranchNotFound` where no location records the branch.
#[test]
fn settle_takes_the_branch_and_stack_from_the_location_that_records_them() {
    let (parent, root) = (new_branch(), new_branch());
    let revision = hash(3);
    let form = RevisionForm::Hash(revision);
    let stack = vec![BranchPoint {
        branch: root,
        revision: hash(1),
    }];
    let unnamed = || Ok(start(BranchId::default(), revision, None));

    let settled = settle(
        form,
        Some(checked(Ok(start(parent, revision, Some(stack.clone()))))),
        Some(checked(unnamed())),
    )
    .expect("settles");

    assert_eq!(settled.branch_point, revision);
    assert_eq!(
        settled.stack,
        [
            BranchPoint {
                branch: parent,
                revision,
            },
            stack[0].clone(),
        ]
    );
    assert_eq!(
        settle_code(form, Some(checked(unnamed())), Some(checked(unnamed()))),
        BranchNotFound::FFI_CODE
    );
    assert_eq!(
        settle_code(form, None, Some(checked(unnamed()))),
        BranchNotFound::FFI_CODE
    );
}

/// The local store's stack is the parent's where it holds the parent's metadata, and the remote's
/// otherwise. A parent no location holds metadata for fails with `BranchNotFound`.
#[test]
fn settle_prefers_the_local_stack() {
    let parent = new_branch();
    let form = RevisionForm::Latest(parent);
    let point = |byte| BranchPoint {
        branch: new_branch(),
        revision: hash(byte),
    };
    let (remote_stack, local_stack) = (vec![point(1)], vec![point(2)]);
    let at = |stack| Ok(start(parent, hash(4), stack));

    let both = settle(
        form,
        Some(checked(at(Some(remote_stack.clone())))),
        Some(checked(at(Some(local_stack.clone())))),
    )
    .expect("settles");
    assert_eq!(both.stack[1..], local_stack);
    let remote_only = settle(
        form,
        Some(checked(at(Some(remote_stack.clone())))),
        Some(checked(at(None))),
    )
    .expect("settles");
    assert_eq!(remote_only.stack[1..], remote_stack);
    assert_eq!(
        settle_code(form, Some(checked(at(None))), Some(checked(at(None)))),
        BranchNotFound::FFI_CODE
    );
}

/// A refusal of the parent other than a miss fails first, then a name either location holds, and
/// the name's claim swaps from the value the local store answered.
#[test]
fn settle_reports_refusals_and_the_local_name_expected() {
    let parent = new_branch();
    let form = RevisionForm::Latest(parent);
    let at = || Ok(start(parent, hash(4), Some(Vec::new())));
    let taken = || {
        Err(RevisionGraphError::from(BranchAlreadyExists {
            branch: "feature".to_owned(),
        }))
    };
    let personal = || {
        Err(RevisionGraphError::from(InvalidArguments {
            reason: "personal".to_owned(),
        }))
    };
    let stale = new_branch();

    assert_eq!(
        settle_code(
            form,
            Some(Checked {
                parent: at(),
                name: taken()
            }),
            Some(checked(personal()))
        ),
        InvalidArguments::FFI_CODE
    );
    for (remote, local) in [
        (taken(), Ok(Context::default())),
        (Ok(Context::default()), taken()),
    ] {
        assert_eq!(
            settle_code(
                form,
                Some(Checked {
                    parent: at(),
                    name: remote
                }),
                Some(Checked {
                    parent: at(),
                    name: local
                })
            ),
            BranchAlreadyExists::FFI_CODE
        );
    }
    let settled = settle(
        form,
        Some(checked(at())),
        Some(Checked {
            parent: at(),
            name: Ok(stale),
        }),
    )
    .expect("settles");
    assert_eq!(settled.name_expected, stale);
}

/// A zero branch point fails with `InvalidArguments` where only the local store, without
/// repository metadata, answered; the remote's check stands in for it.
#[test]
fn settle_refuses_a_zero_branch_point_no_location_checked() {
    let parent = new_branch();
    let form = RevisionForm::Latest(parent);
    let unchecked = || {
        Ok(ParentAt {
            default_unchecked: true,
            ..start(parent, Hash::default(), Some(Vec::new()))
        })
    };

    assert_eq!(
        settle_code(form, None, Some(checked(unchecked()))),
        InvalidArguments::FFI_CODE
    );
    let settled = settle(
        form,
        Some(checked(Ok(start(
            parent,
            Hash::default(),
            Some(Vec::new()),
        )))),
        Some(checked(unchecked())),
    )
    .expect("settles");
    assert_eq!(settled.branch_point, Hash::default());
}

/// A remote revision service that holds `parent` live, at `latest` and in `category`, answers a
/// query of any other branch or of a name `NotFound`, and creates each branch it's asked to with
/// `creator` as its creator, recording the creator each create sent and each query. It serves no
/// other request.
struct CreatingRevision {
    parent: BranchId,
    latest: Hash,
    category: &'static str,
    creator: &'static str,
    sent: Mutex<Vec<String>>,
    queries: Mutex<usize>,
}

impl CreatingRevision {
    fn new(parent: BranchId, latest: Hash, category: &'static str) -> Arc<Self> {
        Arc::new(Self {
            parent,
            latest,
            category,
            creator: "remote-ana",
            sent: Mutex::default(),
            queries: Mutex::default(),
        })
    }
}

#[async_trait]
impl Revision for CreatingRevision {
    async fn branch_create(
        &self,
        branch: BranchId,
        name: &str,
        category: &str,
        creator: &str,
        stack: &[BranchPoint],
    ) -> Result<BranchRecord, ProtocolError> {
        self.sent.lock().unwrap().push(creator.to_owned());
        Ok(BranchRecord {
            id: branch,
            name: name.to_owned(),
            category: category.to_owned(),
            creator: self.creator.to_owned(),
            created: CREATED,
            latest: stack.first().map_or_default(|point| point.revision),
            stack: stack.to_vec(),
            ..Default::default()
        })
    }

    async fn branch_delete(&self, _: BranchId) -> Result<(), ProtocolError> {
        Err(ProtocolError::internal("not served"))
    }

    async fn branch_query(
        &self,
        branch: Option<BranchId>,
        _: Option<&str>,
    ) -> Result<BranchRecord, ProtocolError> {
        *self.queries.lock().unwrap() += 1;
        if branch != Some(self.parent) {
            return Err(ProtocolError::from(NotFound));
        }
        Ok(BranchRecord {
            id: self.parent,
            name: "parent".to_owned(),
            category: self.category.to_owned(),
            latest: self.latest,
            ..Default::default()
        })
    }

    async fn branch_push(
        &self,
        _: BranchId,
        _: Hash,
        _: bool,
        _: bool,
    ) -> Result<BranchPushResponse, BranchPushError> {
        Err(BranchPushError::internal("not served"))
    }

    async fn branch_list(
        &self,
        _: Option<&str>,
        _: bool,
    ) -> Result<BranchListResponse, ProtocolError> {
        Err(ProtocolError::internal("not served"))
    }

    async fn revision_list(
        &self,
        _: RevisionListStart,
    ) -> Result<RevisionListResponse, ProtocolError> {
        Err(ProtocolError::internal("not served"))
    }

    async fn branch_metadata_get(&self, _: BranchId) -> Result<Hash, ProtocolError> {
        Err(ProtocolError::internal("not served"))
    }

    async fn branch_metadata_set(
        &self,
        _: BranchId,
        _: Hash,
        _: Hash,
    ) -> Result<MetadataSetResult, ProtocolError> {
        Err(ProtocolError::internal("not served"))
    }
}

/// Without a user id, `globals.identity` or a handle identity, the creator is the identity the
/// remote connection authenticated as, which the remote's create is sent, and the local store
/// records the creator the remote returns.
#[tokio::test]
async fn the_creator_falls_back_to_the_connection_identity_and_the_local_store_records_the_remotes()
{
    let repository = new_repository();
    let parent = new_branch();
    let service = CreatingRevision::new(parent, hash(1), CATEGORY);
    let store = Arc::new(store_with_remote_as(repository, service.clone(), "conn-bob").await);
    let write = local_write_context(&store, repository);
    in_setup_context(create(
        &write,
        parent,
        "parent",
        "alice",
        Vec::new(),
        hash(1),
        BranchLatestStatus::Convergent,
    ))
    .await;
    let handle = handle::register(store.clone());

    let (status, events) = create_branch(handle, repository, "feature", latest_of(parent)).await;

    assert_eq!(status, 0);
    assert_eq!(*service.sent.lock().unwrap(), ["conn-bob"]);
    let creators: Vec<_> = branch_records(&events)
        .into_iter()
        .map(|record| (record.location, record.creator.as_str().to_owned()))
        .collect();
    assert_eq!(
        creators,
        [
            (LoreBranchLocation::Remote, "remote-ana".to_owned()),
            (LoreBranchLocation::Local, "remote-ana".to_owned()),
        ]
    );
    let created = branch_records(&events)[1].branch;
    let stored = in_setup_context(branch::local_metadata(
        local_read_context(&store, repository),
        created,
    ))
    .await
    .expect("reads the metadata")
    .expect("the branch has metadata")
    .1;
    assert_eq!(branch::creator(&stored).ok(), Some("remote-ana"));
    close_handle(handle).await;
}

/// A creator the remote connection gives that is longer than 1000 bytes fails with
/// `InvalidArguments` before any read at the remote: no query, no create, and no location complete
/// event.
#[tokio::test]
async fn a_connection_creator_over_the_limit_fails_before_any_read() {
    let repository = new_repository();
    let parent = new_branch();
    let service = CreatingRevision::new(parent, hash(1), CATEGORY);
    let long = "c".repeat(1001);
    let handle = handle::register(Arc::new(
        store_with_remote_as(repository, service.clone(), &long).await,
    ));

    let (status, events) = create_branch(handle, repository, "feature", latest_of(parent)).await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_terminal_only(&events);
    assert_eq!(*service.queries.lock().unwrap(), 0);
    assert!(service.sent.lock().unwrap().is_empty());
    close_handle(handle).await;
}

/// A parent the remote records in the personal category refuses the create with
/// `InvalidArguments`, whichever form names it, before the remote's create.
#[tokio::test]
async fn a_personal_parent_at_the_remote_fails_invalid_arguments() {
    let repository = new_repository();
    let parent = new_branch();
    let service = CreatingRevision::new(parent, hash(1), branch::personal_category());
    let handle = handle::register(Arc::new(
        store_with_remote_as(repository, service.clone(), "conn-bob").await,
    ));
    let remote = LoreGlobalArgs {
        remote: 1,
        ..Default::default()
    };

    let (status, events) = create_with(
        remote,
        handle,
        repository,
        "feature",
        CATEGORY,
        latest_of(parent),
    )
    .await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_terminal_only(&events);
    assert!(service.sent.lock().unwrap().is_empty());
    close_handle(handle).await;
}

/// A create whose name another writer claims between its metadata write and its claim fails with
/// `BranchAlreadyExists` in the local store: the local complete event carries the code with no
/// record, and the create's metadata key is gone, so the store holds the metadata of the parent
/// alone.
#[tokio::test]
async fn a_create_that_loses_its_name_claim_reports_it_at_the_local_store_and_rolls_back() {
    let (store, handle) = open_in_memory_handle().await;
    let repository = new_repository();
    let (main, _) = seed_main(&store, repository).await;
    let (reached_tx, mut reached) = tokio::sync::mpsc::unbounded_channel();
    let resume = Arc::new(Notify::new());
    let racing = lore_spawn!(step_branch_writes(
        reached_tx,
        resume.clone(),
        create_branch(handle, repository, "race", latest_of(main)),
    ));
    assert_eq!(reached.recv().await, Some(BranchStep::CreateMetadataStored));
    let write = local_write_context(&store, repository);
    in_setup_context(branch::claim_name(
        write,
        "race",
        Context::default(),
        new_branch(),
    ))
    .await
    .expect("another writer claims the name");
    resume.notify_one();

    let (status, events) = racing.await.expect("the create task ends");

    assert_eq!(status, BranchAlreadyExists::FFI_CODE);
    assert_eq!(
        location_outcomes(&events, CALL_ID),
        [(LoreBranchLocation::Local, 0, BranchAlreadyExists::FFI_CODE)]
    );
    assert!(branch_records(&events).is_empty());
    let held = in_setup_context(async {
        let mut listed = store
            .mutable
            .clone()
            .list(repository, KeyType::BranchMetadata)
            .await
            .expect("lists the metadata keys")
            .channel();
        let mut held = 0;
        while let Some((_, value)) = listed.recv().await {
            held += usize::from(!value.is_zero());
        }
        held
    })
    .await;
    assert_eq!(held, 1, "the parent's metadata key alone");
    close_handle(handle).await;
}
