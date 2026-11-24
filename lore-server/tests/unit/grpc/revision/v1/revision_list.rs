// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::revision::v1::RevisionListRequest;
use lore_proto::lore::revision::v1::revision_list_request::Start;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::allow_all_repositories;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::handlers::branch_push::PushHandler;
use lore_server::grpc::handlers::branch_push::PushReport;
use lore_server::grpc::revision::v1::revision_list::*;
use lore_server::grpc::revision::v1::service::RevisionListInstruments;
use lore_storage::FlushOptions;
use lore_storage::StoreError;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use lore_transport::grpc::REVISION_LIST_STRATEGY_HEADER;
use opentelemetry::KeyValue;
use rand::random;
use tonic::Request;

use crate::store::test_support::FailingLoadStore;
use crate::store::test_support::test_store_create;

struct TestInstrumentProvider {}

impl InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
    fn labels(&self) -> &[KeyValue] {
        &[]
    }
}

fn make_instruments() -> RevisionListInstruments {
    let provider = TestInstrumentProvider {};
    RevisionListInstruments {
        resolve_start_duration: provider.latency_histogram_ms("test.resolve_start.duration"),
        relative_age_seconds: provider
            .length_histogram("test.relative_age_seconds", vec![1.0, 2.0, 3.0]),
        walk_duration: provider.latency_histogram_ms("test.walk.duration"),
    }
}

fn make_request_identifier(
    repository: RepositoryId,
    branch: BranchId,
    number: u64,
) -> Request<RevisionListRequest> {
    let mut request = Request::new(RevisionListRequest {
        start: Some(Start::Identifier(model_v1::RevisionIdentifier {
            branch_id: branch.into(),
            number,
        })),
    });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

fn make_request_signature(
    repository: RepositoryId,
    signature: Hash,
) -> Request<RevisionListRequest> {
    let mut request = Request::new(RevisionListRequest {
        start: Some(Start::Signature(signature.into())),
    });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

/// Push `count` chained revisions to a freshly-created branch.
/// Returns `(branch_id, signatures-newest-first)`.
async fn create_branch_with_history(
    repository: &Arc<RepositoryContext>,
    count: u64,
) -> (BranchId, Vec<Hash>) {
    let write_token = get_write_token();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository.clone(),
        &write_token,
        branch_id,
        "test-branch",
        branch::default_category(),
        "creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("create branch");

    let mut signatures = Vec::with_capacity(count as usize);
    let mut parent = Hash::default();
    for n in 1..=count {
        // The state's metadata blob has to carry `branch` so the
        // forward-cursor lookup can derive it from items[0].
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = State::new();
        state.set_parent_self(parent);
        state.set_revision_number(n);
        state.set_metadata_hash(metadata_hash);
        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let pushed = branch_push::push(
            repository.clone(),
            None,
            branch_id,
            serialized,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &mut PushReport::new(PushHandler::Legacy, Default::default()),
        )
        .await
        .expect("push revision")
        .revision;
        signatures.push(pushed);
        parent = pushed;
    }
    signatures.reverse();
    (branch_id, signatures)
}

/// Serialize a revision without pushing it, for use as the `parent_other`
/// of a merge. Its revision number is what drags the branch's number up.
async fn serialize_detached_revision(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    revision_number: u64,
) -> Hash {
    serialize_chained_revision(
        repository,
        branch_id,
        Hash::default(),
        Hash::default(),
        revision_number,
    )
    .await
}

/// Serialize one revision chained onto `parent_self`, without pushing it.
async fn serialize_chained_revision(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    parent_self: Hash,
    parent_other: Hash,
    revision_number: u64,
) -> Hash {
    let write_token = get_write_token();
    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = State::new();
    state.set_parent_self(parent_self);
    if !parent_other.is_zero() {
        state.set_parent_other(parent_other);
    }
    state.set_revision_number(revision_number);
    state.set_metadata_hash(metadata_hash);
    state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state")
}

/// Push `revision` to `branch_id`, returning its signature and number.
async fn push_revision(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    revision: Hash,
) -> (Hash, u64) {
    let result = branch_push::push(
        repository.clone(),
        None,
        branch_id,
        revision,
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
        &mut PushReport::new(PushHandler::Legacy, Default::default()),
    )
    .await
    .expect("push revision");
    (result.revision, result.revision_number)
}

/// Push one revision chained onto `parent_self`. `revision_number` is a
/// hint only — `push` recomputes it from both parents.
async fn push_chained_revision(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    parent_self: Hash,
    parent_other: Hash,
    revision_number: u64,
) -> (Hash, u64) {
    let serialized = serialize_chained_revision(
        repository,
        branch_id,
        parent_self,
        parent_other,
        revision_number,
    )
    .await;
    push_revision(repository, branch_id, serialized).await
}

/// Build a branch whose revision numbers are not contiguous: a linear run
/// of `linear_before` revisions, then a merge whose `parent_other` is
/// numbered `jump_other_number` (so the branch number jumps to
/// `jump_other_number + 1`), then `linear_after` more revisions.
/// Returns `(branch_id, revision number -> signature)`.
async fn create_branch_with_jump_history(
    repository: &Arc<RepositoryContext>,
    linear_before: u64,
    jump_other_number: u64,
    linear_after: u64,
) -> (BranchId, BTreeMap<u64, Hash>) {
    let write_token = get_write_token();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository.clone(),
        &write_token,
        branch_id,
        "test-branch",
        branch::default_category(),
        "creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("create branch");

    let mut revisions = BTreeMap::new();
    let mut parent = Hash::default();
    for number in 1..=linear_before {
        let (revision, revision_number) =
            push_chained_revision(repository, branch_id, parent, Hash::default(), number).await;
        revisions.insert(revision_number, revision);
        parent = revision;
    }

    let other = serialize_detached_revision(repository, branch_id, jump_other_number).await;
    let (revision, jumped_number) =
        push_chained_revision(repository, branch_id, parent, other, 0).await;
    revisions.insert(jumped_number, revision);
    parent = revision;

    for offset in 1..=linear_after {
        let (revision, revision_number) = push_chained_revision(
            repository,
            branch_id,
            parent,
            Hash::default(),
            jumped_number + offset,
        )
        .await;
        revisions.insert(revision_number, revision);
        parent = revision;
    }

    (branch_id, revisions)
}

#[tokio::test]
async fn unset_start_returns_invalid_argument() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let mut request = Request::new(RevisionListRequest { start: None });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        let err = handler(
            request,
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect_err("unset start should fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }))
    .await;
}

#[tokio::test]
async fn lists_branch_history_via_tip_identifier() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 3).await;

        let response = handler(
            make_request_identifier(repository, branch_id, 0),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed");

        // Strategy header should reflect the direct tip path.
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("direct"),
        );

        let inner = response.into_inner();
        assert_eq!(inner.items.len(), 3);
        assert_eq!(Hash::from(inner.items[0].signature.as_ref()), signatures[0]);
        assert_eq!(inner.items[0].number, 3);
        assert_eq!(Hash::from(inner.items[2].signature.as_ref()), signatures[2]);
        assert_eq!(inner.items[2].number, 1);
        assert!(inner.signature_forward.is_none());
        assert!(inner.signature_backward.is_none());
    }))
    .await;
}

#[tokio::test]
async fn empty_branch_returns_no_items() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository_context,
            &write_token,
            branch_id,
            "empty-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create empty branch");

        let response = handler(
            make_request_identifier(repository, branch_id, 0),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        // Empty branch resolves tip to zero hash; walk exits with
        // no items, no cursors.
        assert!(response.items.is_empty());
        assert!(response.signature_forward.is_none());
        assert!(response.signature_backward.is_none());
    }))
    .await;
}

#[tokio::test]
async fn pages_via_signature_backward_cursor_segment_aligned() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 250 revisions, step=100. Segments 100 and 200 are closed
        // (their +step boundary was crossed by subsequent pushes).
        // Segment 300 is open (rev 250 sits in it).
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Page 1: tip → rev 250, in open segment 300. Walk is
        // segment-aligned: floor = 201, items 250..201 (50), then
        // current_number=200 < floor, so next_older = rev 200.
        let first_page = handler(
            make_request_identifier(repository, branch_id, 0),
            immutable_store.clone(),
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("first page")
        .into_inner();
        assert_eq!(first_page.items.len(), 50);
        assert_eq!(first_page.items[0].number, 250);
        assert_eq!(first_page.items[49].number, 201);
        let backward = first_page
            .signature_backward
            .clone()
            .expect("backward cursor");
        assert_eq!(Hash::from(backward.as_ref()), signatures[250 - 200]);
        assert!(first_page.signature_forward.is_none());

        // Page 2: anchor = rev 200, in closed segment 200 (cached).
        // Cache serves items 200..101. Rev 201 exists (in the open
        // latest band, since only 250 revisions exist and seg 300's
        // step key isn't registered), so the forward cursor still
        // resolves to it via the latest-anchored fallback.
        let second_page = handler(
            make_request_signature(repository, Hash::from(backward.as_ref())),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("second page");
        assert_eq!(
            second_page
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache"),
        );
        let second_page = second_page.into_inner();
        assert_eq!(second_page.items.len(), MAX_REVISION_LIST_RESPONSE_ITEMS);
        assert_eq!(second_page.items[0].number, 200);
        assert_eq!(
            second_page.items[MAX_REVISION_LIST_RESPONSE_ITEMS - 1].number,
            101,
        );
        let forward = second_page.signature_forward.expect("forward cursor");
        assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 201]);
        // Backward cursor: parent of items[N-1] = rev 101 is rev 100,
        // the segment-100 anchor for the next-older page.
        let next_backward = second_page
            .signature_backward
            .clone()
            .expect("backward cursor on second page");
        assert_eq!(Hash::from(next_backward.as_ref()), signatures[250 - 100]);
    }))
    .await;
}

#[tokio::test]
async fn lists_via_by_number_identifier_uses_list_cache_strategy() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Revision 100 sits in the closed segment whose List_100
        // cache entry was populated when revision 101 was pushed.
        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache"),
        );
        let inner = response.into_inner();
        assert_eq!(inner.items.len(), 100);
        assert_eq!(inner.items[0].number, 100);
        assert_eq!(
            Hash::from(inner.items[0].signature.as_ref()),
            signatures[250 - 100],
        );
        // Cache items carry the serialized state header.
        assert_eq!(
            inner.items[0].state.len(),
            std::mem::size_of::<lore_revision::state::StateData>(),
        );
    }))
    .await;
}

#[tokio::test]
async fn lists_via_signature_anchor() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 250 revisions: segment 100 closes when revision 101 is
        // pushed, so List_100 is populated. The signature anchor
        // at revision 100 hits that cache.
        let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

        let anchor = signatures[250 - 100];
        let response = handler(
            make_request_signature(repository, anchor),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache"),
        );
        let inner = response.into_inner();
        assert_eq!(inner.items[0].number, 100);
        // Forward cursor for target=101 walks from the step key at
        // 200 down to 101.
        let forward = inner.signature_forward.expect("forward cursor");
        assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 101]);
        // Backward cursor is None: cached items cover 100..1 and
        // items[N-1] = revision 1's parent is the zero hash.
        assert!(inner.signature_backward.is_none());
        // Cache items carry the serialized state header.
        assert_eq!(
            inner.items[0].state.len(),
            std::mem::size_of::<lore_revision::state::StateData>(),
        );
    }))
    .await;
}

/// The forward cursor is served from the `BranchLatestPointer` step key.
/// Failing that one lookup with backpressure must reach the client as
/// `RESOURCE_EXHAUSTED`; reporting an absent cursor instead would send
/// the client back to paging from the branch tip.
#[tokio::test]
async fn forward_cursor_slow_down_returns_resource_exhausted() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // The cursor for the page anchored at revision 100 targets
        // revision 101, whose step key is the only lookup failed here.
        let (forward_key, _key_type) = branch::revision_step_key(
            repository::SALT_LORE,
            repository,
            branch_id,
            101,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        let throttled = FailingLoadStore::for_key(
            mutable_store,
            forward_key,
            lore_storage::StoreError::from(lore_base::error::SlowDown),
        );

        let status = handler(
            make_request_signature(repository, signatures[250 - 100]),
            immutable_store,
            throttled,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect_err("backpressure must not be reported as an absent cursor");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
    }))
    .await;
}

/// Wipe the cached `List_100` entry to simulate eviction. The next
/// identifier-anchored request must rebuild it via the
/// list-cache-backfill path, and a subsequent request must hit the
/// fast path.
#[tokio::test]
async fn identifier_backfills_when_cache_missing_but_next_skip_exists() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

        let (key, key_type) = branch::revision_list_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            100,
            DEFAULT_HISTORY_STEP_SIZE,
        );

        // Storing zero deletes the mutable store entry.
        mutable_store
            .clone()
            .store(repository, key, Hash::default(), key_type)
            .await
            .expect("evict cache entry");
        assert!(
            mutable_store
                .clone()
                .load(repository, key, key_type)
                .await
                .is_err(),
            "cache should be evicted",
        );

        let response = handler(
            make_request_identifier(repository, branch_id, 50),
            immutable_store.clone(),
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("first call");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache-backfill"),
        );

        let response = handler(
            make_request_identifier(repository, branch_id, 50),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("second call");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache"),
        );
    }))
    .await;
}

#[tokio::test]
async fn forward_cursor_resolves_within_open_latest_band_when_no_step_key_covers_target() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 50 revisions: only block 0 is populated; no step key
        // ever registered (no boundary crossing happened). Anchor
        // at revision 25 — target 26 has no step key registered,
        // but it still exists in the branch's open latest band, so
        // the forward cursor must resolve to it rather than report
        // no newer page.
        let (_branch, signatures) = create_branch_with_history(&repository_context, 50).await;

        let anchor = signatures[50 - 25];
        let response = handler(
            make_request_signature(repository, anchor),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 25);
        let forward = response.signature_forward.expect("forward cursor");
        assert_eq!(Hash::from(forward.as_ref()), signatures[50 - 26]);
    }))
    .await;
}

/// Serialize a revision chained onto `parent_self` without pushing it, as a client stores a
/// revision it has not pushed.
async fn serialize_unpushed_revision(
    repository: &Arc<RepositoryContext>,
    branch_id: BranchId,
    parent_self: Hash,
    revision_number: u64,
) -> Hash {
    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = State::new();
    state.set_parent_self(parent_self);
    state.set_revision_number(revision_number);
    state.set_metadata_hash(metadata_hash);
    state
        .serialize(repository.clone(), &get_write_token())
        .await
        .expect("serialize unpushed state")
}

/// A page starting at a revision numbered above the branch's latest, which the branch does not
/// reach, lists it and its history with no forward cursor, as `acceleration` resolves it.
async fn page_above_latest_has_no_forward_cursor(
    acceleration: lore_server::grpc::server::RevisionListAcceleration,
) {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 5).await;
        let unpushed =
            serialize_unpushed_revision(&repository_context, branch_id, signatures[0], 6).await;

        let response = handler(
            make_request_signature(repository, unpushed),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        let inner = response.into_inner();
        assert_eq!(inner.items[0].number, 6);
        assert!(inner.signature_forward.is_none());
    }))
    .await;
}

#[tokio::test]
async fn page_above_latest_has_no_forward_cursor_from_the_latest_band() {
    page_above_latest_has_no_forward_cursor(
        lore_server::grpc::server::RevisionListAcceleration::default(),
    )
    .await;
}

#[tokio::test]
async fn page_above_latest_has_no_forward_cursor_from_an_unverified_gap() {
    page_above_latest_has_no_forward_cursor(lore_server::grpc::server::RevisionListAcceleration {
        step_keys: false,
        list_cache: false,
    })
    .await;
}

#[tokio::test]
async fn unknown_signature_returns_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let bogus = Hash::from(random::<[u8; 32]>());
        let err = handler(
            make_request_signature(repository, bogus),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect_err("unknown signature should fail");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

#[tokio::test]
async fn unknown_identifier_returns_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let unknown_branch = BranchId::from(uuid::Uuid::now_v7());
        let err = handler(
            make_request_identifier(repository, unknown_branch, 0),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect_err("unknown branch should fail");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

/// Signature anchor pointing mid-segment must return the full
/// cached segment (200..101), not just the items from the anchor
/// down. The anchor is guaranteed to appear in the response per
/// the relaxed v1 contract.
#[tokio::test]
async fn mid_segment_signature_returns_full_cached_segment() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Revision 150 sits mid-way in closed segment 200.
        let anchor = signatures[250 - 150];
        let response = handler(
            make_request_signature(repository, anchor),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache"),
        );
        let inner = response.into_inner();
        // Full segment served: 200..101 inclusive.
        assert_eq!(inner.items.len(), MAX_REVISION_LIST_RESPONSE_ITEMS);
        assert_eq!(inner.items[0].number, 200);
        assert_eq!(
            inner.items[MAX_REVISION_LIST_RESPONSE_ITEMS - 1].number,
            101,
        );
        // The anchor lives somewhere in the page (not items[0]).
        let anchor_position = inner
            .items
            .iter()
            .position(|item| Hash::from(item.signature.as_ref()) == anchor)
            .expect("anchor must appear in cached page");
        assert_eq!(inner.items[anchor_position].number, 150);
        assert_ne!(anchor_position, 0, "anchor is mid-page, not items[0]");
    }))
    .await;
}

/// Evict `List_100` and request `rev_50` by signature. The handler's
/// signature path must rebuild the segment via backfill (the +step
/// skip pointer at seg 200 exists, so the segment is backfillable)
/// and report the `list-cache-backfill` strategy. Subsequent calls
/// then hit the warm cache.
#[tokio::test]
async fn signature_path_backfills_evicted_segment() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Evict the List_100 cache entry.
        let (key, key_type) = branch::revision_list_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            100,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        mutable_store
            .clone()
            .store(repository, key, Hash::default(), key_type)
            .await
            .expect("evict cache");

        let anchor = signatures[250 - 50];
        let response = handler(
            make_request_signature(repository, anchor),
            immutable_store.clone(),
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("first call");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache-backfill"),
        );
        // Backfill returned the same page the cache would now hold.
        let inner = response.into_inner();
        assert_eq!(inner.items.len(), MAX_REVISION_LIST_RESPONSE_ITEMS);
        assert_eq!(inner.items[0].number, 100);
        assert_eq!(inner.items[MAX_REVISION_LIST_RESPONSE_ITEMS - 1].number, 1,);

        // Subsequent call: warm cache.
        let response = handler(
            make_request_signature(repository, anchor),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("second call");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache"),
        );
    }))
    .await;
}

/// Signature in an open segment (no cache, no backfill possible
/// because the +step skip pointer doesn't exist yet) must fall
/// through to the direct walk, and the walk must be segment-aligned.
#[tokio::test]
async fn open_segment_signature_walk_is_segment_aligned() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 250 revs: segment 300 is open (rev 250 lives there but
        // nothing crosses into seg 400 to register the +step key).
        let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Anchor rev 220 — mid-open-segment-300. Floor = 201.
        let anchor = signatures[250 - 220];
        let response = handler(
            make_request_signature(repository, anchor),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("direct"),
        );
        let inner = response.into_inner();
        // Walk segment-aligned: items 220..201 (20 items), not the
        // full 100-item walk.
        assert_eq!(inner.items.len(), 20);
        assert_eq!(inner.items[0].number, 220);
        assert_eq!(inner.items[19].number, 201);
        // Backward = parent_self of rev_201 = rev_200.
        let backward = inner.signature_backward.expect("backward cursor");
        assert_eq!(Hash::from(backward.as_ref()), signatures[250 - 200]);
    }))
    .await;
}

/// Stuff the mutable store with a cache blob whose header version
/// is wrong. The loader must discard it (debug-logged), backfill
/// rebuilds with the current version, and the strategy is reported
/// as `list-cache-backfill`.
#[tokio::test]
async fn mismatched_cache_version_is_discarded_and_rebuilt() {
    use zerocopy::IntoBytes;

    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

        // Overwrite the List_100 entry with a blob whose header
        // carries a future/unknown version. Correct magic, wrong
        // version — exercises the version-mismatch branch of the
        // header check.
        let bogus_header = branch::CachedRevisionListHeader {
            magic: branch::CACHED_REVISION_LIST_MAGIC,
            version: branch::CACHED_REVISION_LIST_VERSION + 99,
        };
        let bogus_item = branch::CachedRevisionItem {
            number: 100,
            signature: Hash::default(),
            metadata: Hash::default(),
            state: lore_revision::state::StateData::default(),
        };
        let mut buffer = bytes::BytesMut::new();
        buffer.extend_from_slice(bogus_header.as_bytes());
        buffer.extend_from_slice([bogus_item].as_bytes());
        let address = lore_revision::immutable::write(
            repository_context.clone(),
            lore_storage::Context::default(),
            buffer.freeze(),
            lore_revision::immutable::write_options_from_repository(repository_context.clone()),
        )
        .await
        .expect("write bogus blob");

        let (key, key_type) = branch::revision_list_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            100,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        mutable_store
            .clone()
            .store(repository, key, address.hash, key_type)
            .await
            .expect("install bogus blob");

        // First call must reject the bogus blob and rebuild.
        let response = handler(
            make_request_identifier(repository, branch_id, 50),
            immutable_store.clone(),
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("first call");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache-backfill"),
        );
        let inner = response.into_inner();
        assert_eq!(inner.items.len(), 100);
        assert_eq!(inner.items[0].number, 100);
        assert_eq!(inner.items[99].number, 1);

        // Second call: cache is now rebuilt with the current
        // format, so the fast path takes over.
        let response = handler(
            make_request_identifier(repository, branch_id, 50),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("second call");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("list-cache"),
        );
    }))
    .await;
}

/// Every response item carries a `state` field that round-trips
/// back to a `StateData` whose `revision_number` matches the item.
/// Covers both the cache fast path (item 100) and the walk path
/// (item 220 in the open segment 300).
#[tokio::test]
async fn item_state_round_trips_to_state_data() {
    use zerocopy::FromBytes;

    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Cache fast path: identifier rev 100.
        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store.clone(),
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("cache fast path")
        .into_inner();
        for item in &response.items {
            let state = lore_revision::state::StateData::read_from_bytes(item.state.as_ref())
                .expect("state bytes must round-trip");
            assert_eq!(state.revision_number, item.number);
        }

        // Walk path: signature for rev 220 (open seg 300, no cache).
        let response = handler(
            make_request_signature(repository, signatures[250 - 220]),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("walk path")
        .into_inner();
        assert!(!response.items.is_empty());
        for item in &response.items {
            let state = lore_revision::state::StateData::read_from_bytes(item.state.as_ref())
                .expect("state bytes must round-trip");
            assert_eq!(state.revision_number, item.number);
        }
    }))
    .await;
}

/// With `list_cache = false`, identifier lookups for revisions in
/// closed segments must NOT serve from cache. The handler falls
/// through to the step-key path (history-step strategy here, since
/// `step_keys` is still on).
#[tokio::test]
async fn list_cache_disabled_skips_cache() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: true,
            list_cache: false,
        };
        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("history-step"),
        );
    }))
    .await;
}

/// With `step_keys = false` (and cache also off), identifier
/// lookups fall through to the full-iteration walk.
#[tokio::test]
async fn both_disabled_falls_through_to_full_iteration() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: false,
            list_cache: false,
        };
        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("full-iteration"),
        );
    }))
    .await;
}

/// The full-iteration path resolves `branch@number` through
/// `revision::resolve`, which reads the branch latest first. Throttling that
/// read must reach the client as `RESOURCE_EXHAUSTED`: reporting it as
/// `NOT_FOUND` claims a revision does not exist and gives the client no
/// reason to retry.
#[tokio::test]
async fn throttled_branch_latest_resolves_to_resource_exhausted_not_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

        let (latest_key, _key_type) =
            branch::mutable_key(repository::SALT_LORE, branch::LATEST, repository, branch_id);
        let throttled = FailingLoadStore::for_key(
            mutable_store,
            latest_key,
            lore_storage::StoreError::from(lore_base::error::SlowDown),
        );

        // Acceleration off, so the request takes the full-iteration path
        // through revision::resolve rather than a step key or the cache.
        let status = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            throttled,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration {
                step_keys: false,
                list_cache: false,
            },
            &make_instruments(),
        )
        .await
        .expect_err("a throttled branch latest must not resolve");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
    }))
    .await;
}

/// With `list_cache = false`, a signature lookup that would
/// otherwise hit the cache must instead walk directly. The walker
/// is still segment-aligned.
#[tokio::test]
async fn list_cache_disabled_signature_uses_direct_walk() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Anchor rev 150, mid-segment 200. Cached, but we disable.
        let anchor = signatures[250 - 150];
        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: true,
            list_cache: false,
        };
        let response = handler(
            make_request_signature(repository, anchor),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed");
        assert_eq!(
            response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("direct"),
        );
        let inner = response.into_inner();
        // Segment-aligned walk: rev 150 down to floor 101.
        assert_eq!(inner.items.len(), 50);
        assert_eq!(inner.items[0].number, 150);
        assert_eq!(inner.items[49].number, 101);
    }))
    .await;
}

/// Every revision that exists resolves by number, including on a branch
/// whose numbering has a gap left by a merge.
#[tokio::test]
async fn finds_revision_below_a_jump_that_skipped_a_boundary() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 1..=99, then a merge jumping to 105, then 106..=150.
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 99, 104, 45).await;
        assert!(revisions.contains_key(&105));
        assert_eq!(revisions.keys().next_back(), Some(&150));
        // 100..=104 were skipped by the jump.
        assert!(!revisions.contains_key(&100));

        for number in [99, 105, 120, 150] {
            let response = handler(
                make_request_identifier(repository, branch_id, number),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .unwrap_or_else(|err| panic!("revision {number} should resolve: {err}"))
            .into_inner();
            assert_eq!(response.items[0].number, number);
            assert_eq!(
                Hash::from(response.items[0].signature.as_ref()),
                revisions[&number],
            );
        }
    }))
    .await;
}

/// With several boundaries skipped in one jump, each sealed boundary
/// answers with the highest revision at or below it, so lookups inside
/// the pre-jump segment still resolve. A request served from the list
/// cache returns that whole segment, so the requested revision is located
/// within the items rather than assumed to head them.
#[tokio::test]
async fn finds_revisions_below_a_multi_boundary_jump() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 1..=150, then a merge jumping to 400 (skipping boundaries 200
        // and 300), then 401..=405.
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 150, 399, 5).await;
        assert!(revisions.contains_key(&400));

        for number in [101, 120, 150, 400, 405] {
            let response = handler(
                make_request_identifier(repository, branch_id, number),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .unwrap_or_else(|err| panic!("revision {number} should resolve: {err}"))
            .into_inner();
            let item = response
                .items
                .iter()
                .find(|item| item.number == number)
                .unwrap_or_else(|| panic!("revision {number} missing from response"));
            assert_eq!(Hash::from(item.signature.as_ref()), revisions[&number]);
        }
    }))
    .await;
}

/// The segment holding the branch head stays open until the head moves
/// past it, so a lookup inside it resolves by iteration.
#[tokio::test]
async fn jump_does_not_seal_the_segment_it_landed_in() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // Head lands at 105 and stops, leaving segment 200 open.
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 99, 104, 0).await;

        let (key, key_type) = branch::revision_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            200,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        let err = mutable_store
            .clone()
            .load(repository, key, key_type)
            .await
            .expect_err("segment 200 holds the head and must not be sealed");
        assert!(
            matches!(err, StoreError::AddressNotFound(_)),
            "an unsealed boundary must read as missing, got {err:?}",
        );

        let (crossed_key, crossed_key_type) = branch::revision_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            100,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        let sealed = mutable_store
            .load(repository, crossed_key, crossed_key_type)
            .await
            .expect("boundary 100 was crossed and must be sealed");
        assert_eq!(
            sealed, revisions[&99],
            "boundary 100 holds the highest revision numbered at or below it",
        );
    }))
    .await;
}

/// The forward cursor resolves to the real next revision across a
/// boundary the branch's numbering skipped, rather than assuming
/// `items[0].number + 1` exists.
#[tokio::test]
async fn forward_cursor_finds_the_real_revision_across_a_jump() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 1..=150, then a merge jumping to 400, then 401..=405 —
        // the last five pushes seal boundary 400, so the target
        // sits behind a sealed skip pointer rather than the open
        // latest band.
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 150, 399, 5).await;
        assert!(revisions.contains_key(&400));

        let response = handler(
            make_request_identifier(repository, branch_id, 150),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 150);
        let forward = response
            .signature_forward
            .expect("forward cursor across the jump");
        assert_eq!(Hash::from(forward.as_ref()), revisions[&400]);
    }))
    .await;
}

/// Several consecutive empty step boundaries above the current page
/// must all be skipped, and the real target beyond them still found.
#[tokio::test]
async fn forward_cursor_skips_several_empty_bands_to_find_the_target() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 1..=150, then a merge jumping to 700 — skipping the empty
        // boundaries 200, 300, 400, 500 and 600 — then 701..=705 to
        // seal boundary 700.
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 150, 699, 5).await;
        assert!(revisions.contains_key(&700));

        let response = handler(
            make_request_identifier(repository, branch_id, 150),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 150);
        let forward = response
            .signature_forward
            .expect("forward cursor past five empty bands");
        assert_eq!(Hash::from(forward.as_ref()), revisions[&700]);
    }))
    .await;
}

/// A jump wide enough to leave dozens of consecutive sealed boundaries
/// pointing at the same pre-jump revision must still resolve in a
/// bounded number of probes: the binary search finds the real target's
/// boundary directly rather than degrading into a walk proportional
/// to the branch's history since the jump.
#[tokio::test]
async fn forward_cursor_resolves_a_jump_spanning_many_boundaries() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 1..=150, then a merge jumping to 5100 — sealing 48
        // consecutive empty boundaries (200..=5000) at the pre-jump
        // revision — then 5101..=5110 to seal boundary 5100 itself.
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 150, 5099, 10).await;
        assert!(revisions.contains_key(&5100));

        let response = handler(
            make_request_identifier(repository, branch_id, 150),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 150);
        let forward = response
            .signature_forward
            .expect("forward cursor across a wide jump");
        assert_eq!(Hash::from(forward.as_ref()), revisions[&5100]);
    }))
    .await;
}

/// Repeatedly following `signature_forward` from a page anchored well
/// before a jump must reach the branch's latest revision, and the
/// union of every page visited along the way must cover every
/// revision that exists — proving the cursor never skips a real
/// revision on its way there.
#[tokio::test]
async fn paging_forward_via_signature_forward_reaches_every_revision() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // 1..=150, then a merge jumping to 400, then 401..=405.
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 150, 399, 5).await;

        let mut seen: BTreeSet<u64> = BTreeSet::new();
        let mut request = make_request_identifier(repository, branch_id, 1);
        let mut reached_latest = false;

        // Bounded generously above the number of pages this fixture
        // can produce; a bug that stalls forward progress should fail
        // loudly here rather than hang the test.
        for _ in 0..30 {
            let response = handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("paginated request failed")
            .into_inner();

            seen.extend(response.items.iter().map(|item| item.number));

            let Some(forward) = response.signature_forward else {
                reached_latest = response.items.iter().any(|item| item.number == 405);
                break;
            };
            request = make_request_signature(repository, Hash::from(forward.as_ref()));
        }

        assert!(
            reached_latest,
            "paging forward never reached the branch's latest revision"
        );
        let expected: BTreeSet<u64> = revisions.keys().copied().collect();
        assert_eq!(seen, expected, "paging forward skipped some revisions");
    }))
    .await;
}

/// Evicting the anchor band's cached list — but not its skip pointer
/// — must not cause the forward cursor to skip past revisions that
/// exist. It falls back to walking the band directly instead.
#[tokio::test]
async fn forward_cursor_survives_an_evicted_anchor_list_cache() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Evict segment 200's cached list — the anchor band above
        // revision 100 — leaving its skip pointer intact.
        let (key, key_type) = branch::revision_list_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            200,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        mutable_store
            .clone()
            .store(repository, key, Hash::default(), key_type)
            .await
            .expect("evict anchor list cache");

        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 100);
        let forward = response
            .signature_forward
            .expect("forward cursor via band walk");
        assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 101]);
    }))
    .await;
}

/// A step boundary can go missing below a boundary that is still
/// present — the seal write is best-effort, and the key type has been
/// renamed once already, orphaning older entries. The forward cursor
/// must still return the real successor: a missing boundary is not
/// proof its band is empty, so the gap above it has to be walked
/// rather than assumed to hold nothing.
/// The missing pointer is also repaired as a side effect, so a
/// second call resolves the fast path directly rather than repeating
/// the gap descent.
#[tokio::test]
async fn forward_cursor_repairs_a_missing_skip_pointer_below_a_found_anchor() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 350).await;

        // Evict ONLY boundary 200's skip pointer (REVISION_NUMBER_STEP),
        // leaving its list cache and boundary 300's skip pointer intact.
        let (key, key_type) = branch::revision_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            200,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        mutable_store
            .clone()
            .store(repository, key, Hash::default(), key_type)
            .await
            .expect("evict boundary 200 skip pointer");

        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 100);
        let forward = response
            .signature_forward
            .expect("forward cursor across the missing pointer");
        assert_eq!(Hash::from(forward.as_ref()), signatures[350 - 101]);

        // The gap descent repaired boundary 200's skip pointer: it now
        // points at revision 200, the highest revision at or below it.
        let repaired = mutable_store
            .load(repository, key, key_type)
            .await
            .expect("boundary 200 should be repaired");
        assert_eq!(repaired, signatures[350 - 200]);
    }))
    .await;
}

/// The gap descent runs down to `first_number`, so it crosses and
/// repairs the boundary immediately above it — the one a page anchored
/// this deep probes first — even when that is the very boundary whose
/// absence sent the request down the gap descent in the first place. A
/// second, identical request then takes the fast path directly, for both
/// the page resolution and the forward cursor: the first call's cost
/// heals the exact spot future requests need, not just spots nearer the
/// branch's latest revision.
#[tokio::test]
async fn forward_cursor_repairs_the_boundary_nearest_first_number() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Evict boundary 100 — the very first boundary `forward_anchor`
        // would probe for a page anchored at revision 50.
        let (key, key_type) = branch::revision_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            100,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        mutable_store
            .clone()
            .store(repository, key, Hash::default(), key_type)
            .await
            .expect("evict boundary 100 skip pointer");

        // Disable the list cache so the page resolves to exactly
        // revision 50 (not the whole cached segment headed at 100),
        // landing `first_number` well below the evicted boundary.
        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: true,
            list_cache: false,
        };

        let first_response = handler(
            make_request_identifier(repository, branch_id, 50),
            immutable_store.clone(),
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("first request failed");
        assert_eq!(
            first_response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("full-iteration"),
            "no acceleration is usable yet for the first call",
        );
        let first_response = first_response.into_inner();
        assert_eq!(first_response.items[0].number, 50);
        let forward = first_response
            .signature_forward
            .expect("forward cursor via the gap descent");
        assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 51]);

        // The descent crossed boundary 100 on its way down and
        // repaired it — the boundary nearest `first_number`, not just
        // ones nearer the branch's latest revision.
        let repaired = mutable_store
            .clone()
            .load(repository, key, key_type)
            .await
            .expect("boundary 100 should be repaired");
        assert_eq!(repaired, signatures[250 - 100]);

        // An identical second request now takes the fast path for
        // both the page and the forward cursor, with no gap descent
        // needed at all.
        let second_response = handler(
            make_request_identifier(repository, branch_id, 50),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("second request failed");
        assert_eq!(
            second_response
                .metadata()
                .get(REVISION_LIST_STRATEGY_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("history-step"),
            "the repaired boundary now serves the page directly",
        );
        let second_response = second_response.into_inner();
        assert_eq!(second_response.items[0].number, 50);
        let forward = second_response
            .signature_forward
            .expect("forward cursor via the now-repaired boundary");
        assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 51]);
    }))
    .await;
}

/// The widest gap the open latest band can present is exactly
/// `history_step_size` revisions, which the band walk covers in
/// `history_step_size + 1` items — its entire budget. A page anchored
/// that far below an unsealed latest revision must still resolve its
/// cursor rather than exhaust the walk and fail the request.
#[tokio::test]
async fn forward_cursor_resolves_a_page_a_full_band_below_latest() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        // Revision 200 is the branch's latest, so boundary 200 is
        // unsealed: sealing it waits on a revision above it. A page
        // anchored at revision 100 sits a full band below it.
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 200).await;

        // The list cache would serve the segment headed at 100 and lift
        // the page's first item above revision 100.
        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: true,
            list_cache: false,
        };

        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 100);
        let forward = response
            .signature_forward
            .expect("forward cursor across the full band");
        assert_eq!(Hash::from(forward.as_ref()), signatures[200 - 101]);
    }))
    .await;
}

/// A skip pointer missing at the first boundary probed leaves the binary
/// search above it intact: the descent anchors on the lowest boundary
/// that search still proves sealed, not on the branch's latest revision.
/// Boundaries above that anchor stay untouched — a descent from the
/// latest revision would cross and rewrite every one of them — so the
/// one evicted above the anchor is still missing afterwards.
#[tokio::test]
async fn forward_cursor_binary_searches_past_a_missing_first_boundary() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 350).await;

        let step_key = |boundary: u64| {
            branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                boundary,
                DEFAULT_HISTORY_STEP_SIZE,
            )
        };

        // Boundary 100 is the first one probed for a page anchored at
        // revision 50. Boundary 300 sits above the anchor the search
        // settles on, so only a descent from the latest revision reaches
        // it.
        for boundary in [100, 300] {
            let (key, key_type) = step_key(boundary);
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict skip pointer");
        }

        // The list cache would serve the whole segment headed at 100 and
        // lift `first_number` above the evicted boundary.
        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: true,
            list_cache: false,
        };

        let response = handler(
            make_request_signature(repository, signatures[350 - 50]),
            immutable_store,
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 50);
        let forward = response
            .signature_forward
            .expect("forward cursor across the missing pointer");
        assert_eq!(Hash::from(forward.as_ref()), signatures[350 - 51]);

        let (key, key_type) = step_key(100);
        let repaired = mutable_store
            .clone()
            .load(repository, key, key_type)
            .await
            .expect("boundary 100 lies on the descent");
        assert_eq!(repaired, signatures[350 - 100]);

        let (key, key_type) = step_key(300);
        let err = mutable_store
            .load(repository, key, key_type)
            .await
            .expect_err("boundary 300 lies above the descent's anchor");
        assert!(err.is_address_not_found(), "{err:?}");
    }))
    .await;
}

/// A skip pointer missing part-way through the binary search narrows the
/// search upward instead of ending it, so the descent still anchors on a
/// boundary proven sealed above the gap rather than on the branch's
/// latest revision. The cursor it returns also pins that such a gap is
/// never reported as a single band: the anchor here sits 350 revisions
/// above the page, far past the one-band walk a sealed answer licenses.
#[tokio::test]
async fn forward_cursor_binary_searches_past_a_missing_mid_boundary() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 550).await;

        let step_key = |boundary: u64| {
            branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                boundary,
                DEFAULT_HISTORY_STEP_SIZE,
            )
        };

        // For a page anchored at revision 50 the search probes 100, then
        // 300, then 400. Evicting the first two makes 300 the mid-search
        // gap; 500 is never probed and sits above the anchor, so only a
        // descent from the latest revision reaches it.
        for boundary in [100, 300, 500] {
            let (key, key_type) = step_key(boundary);
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict skip pointer");
        }

        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: true,
            list_cache: false,
        };

        let response = handler(
            make_request_signature(repository, signatures[550 - 50]),
            immutable_store,
            mutable_store.clone(),
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 50);
        let forward = response
            .signature_forward
            .expect("forward cursor across the missing pointers");
        assert_eq!(Hash::from(forward.as_ref()), signatures[550 - 51]);

        for boundary in [100, 300] {
            let (key, key_type) = step_key(boundary);
            let repaired = mutable_store
                .clone()
                .load(repository, key, key_type)
                .await
                .unwrap_or_else(|err| panic!("boundary {boundary} lies on the descent: {err:?}"));
            assert_eq!(repaired, signatures[550 - boundary as usize]);
        }

        let (key, key_type) = step_key(500);
        let err = mutable_store
            .load(repository, key, key_type)
            .await
            .expect_err("boundary 500 lies above the descent's anchor");
        assert!(err.is_address_not_found(), "{err:?}");
    }))
    .await;
}

/// `MutableStore` wrapper that fails `load` for one specific key with
/// a non-retryable, non-`AddressNotFound` error, delegating
/// everything else to `inner`. Used to exercise the forward cursor's
/// error path, which a real store failure should reach unmasked.
struct FailingMutableStore {
    inner: Arc<dyn lore_storage::MutableStore>,
    fail_key: Hash,
}

#[async_trait::async_trait]
impl lore_storage::MutableStore for FailingMutableStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::MutableStore::hold_for_command(self.inner.clone()).await
    }

    async fn load(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        key: Hash,
        key_type: lore_storage::KeyType,
    ) -> Result<Hash, lore_storage::StoreError> {
        if key == self.fail_key {
            return Err(lore_storage::StoreError::from(
                lore_storage::errors::Maintenance,
            ));
        }
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        key: Hash,
        value: Hash,
        key_type: lore_storage::KeyType,
    ) -> Result<(), lore_storage::StoreError> {
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: lore_storage::KeyType,
    ) -> Result<Hash, lore_storage::StoreError> {
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: lore_storage::Partition,
        key_type: lore_storage::KeyType,
    ) -> Result<lore_storage::KeyValueStream, lore_storage::StoreError> {
        self.inner.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), lore_storage::StoreError> {
        self.inner.clone().flush(options).await
    }
}

/// A genuine store failure while probing for the forward cursor must
/// surface as an error, not silently collapse into "no newer page".
#[tokio::test]
async fn forward_cursor_propagates_a_genuine_store_failure() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

        // Fail the probe for the anchor band above revision 100
        // (boundary 200) with something other than `AddressNotFound`
        // or `SlowDown`.
        let (fail_key, _) = branch::revision_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            200,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        let failing_store: Arc<dyn lore_storage::MutableStore> = Arc::new(FailingMutableStore {
            inner: mutable_store,
            fail_key,
        });

        let err = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            failing_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect_err("a genuine store failure must surface, not become a missing cursor");
        assert_eq!(err.code(), tonic::Code::Internal);
    }))
    .await;
}

/// With `step_keys` disabled, the forward cursor must never read the
/// skip pointer at all — not just tolerate it being absent. Corrupts
/// the pointer with a value that would produce a visibly wrong
/// answer if read, and confirms the real answer comes back anyway,
/// via the same walk `resolve_start` falls back to for the main page
/// when step keys are off.
#[tokio::test]
async fn forward_cursor_step_keys_disabled_never_reads_a_wrong_skip_pointer() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Corrupt boundary 100's skip pointer to point at revision 1
        // instead of revision 100. If this is read despite step_keys
        // being disabled, the forward cursor would resolve to
        // something other than revision 51.
        let (key, key_type) = branch::revision_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            100,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        mutable_store
            .clone()
            .store(repository, key, signatures[250 - 1], key_type)
            .await
            .expect("corrupt boundary 100 skip pointer");

        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: false,
            list_cache: false,
        };
        let response = handler(
            make_request_identifier(repository, branch_id, 50),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 50);
        let forward = response
            .signature_forward
            .expect("forward cursor via the uncapped walk");
        assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 51]);
    }))
    .await;
}

/// With `list_cache` disabled, the forward cursor must never read the
/// cached segment list for a sealed anchor — not just tolerate it
/// being missing or malformed. Plants a well-formed but wrong cached
/// list at the anchor's boundary and confirms the real answer, from
/// the direct walk, comes back instead.
#[tokio::test]
async fn forward_cursor_list_cache_disabled_never_reads_a_wrong_cached_list() {
    use zerocopy::IntoBytes;

    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, signatures) = create_branch_with_history(&repository_context, 250).await;

        // Overwrite segment 200's cached list with a well-formed blob
        // whose sole item is a fabricated revision numbered far above
        // anything real. If this is read despite list_cache being
        // disabled, the forward cursor would resolve to that bogus
        // signature instead of the real revision 101.
        let bogus_header = branch::CachedRevisionListHeader {
            magic: branch::CACHED_REVISION_LIST_MAGIC,
            version: branch::CACHED_REVISION_LIST_VERSION,
        };
        let bogus_item = branch::CachedRevisionItem {
            number: 9999,
            signature: Hash::from(random::<[u8; 32]>()),
            metadata: Hash::default(),
            state: lore_revision::state::StateData::default(),
        };
        let mut buffer = bytes::BytesMut::new();
        buffer.extend_from_slice(bogus_header.as_bytes());
        buffer.extend_from_slice([bogus_item].as_bytes());
        let address = lore_revision::immutable::write(
            repository_context.clone(),
            lore_storage::Context::default(),
            buffer.freeze(),
            lore_revision::immutable::write_options_from_repository(repository_context.clone()),
        )
        .await
        .expect("write bogus blob");
        let (key, key_type) = branch::revision_list_step_key(
            lore_revision::repository::SALT_LORE,
            repository,
            branch_id,
            200,
            DEFAULT_HISTORY_STEP_SIZE,
        );
        mutable_store
            .clone()
            .store(repository, key, address.hash, key_type)
            .await
            .expect("install bogus cached list");

        let acceleration = lore_server::grpc::server::RevisionListAcceleration {
            step_keys: true,
            list_cache: false,
        };
        let response = handler(
            make_request_identifier(repository, branch_id, 100),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            acceleration,
            &make_instruments(),
        )
        .await
        .expect("Request failed")
        .into_inner();
        assert_eq!(response.items[0].number, 100);
        let forward = response
            .signature_forward
            .expect("forward cursor via the direct walk");
        assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 101]);
    }))
    .await;
}

/// Create a branch with the given stack (nearest ancestor first) and push
/// `count` revisions onto it. Returns `(branch_id, (number, signature)
/// newest-first)`.
async fn create_child_branch_with_history(
    repository: &Arc<RepositoryContext>,
    stack: Vec<BranchPoint>,
    count: u64,
) -> (BranchId, Vec<(u64, Hash)>) {
    let branch_point = stack[0].revision;
    let write_token = get_write_token();
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository.clone(),
        &write_token,
        branch_id,
        &format!("test-child-branch-{branch_id}"),
        branch::default_category(),
        "creator",
        1,
        stack,
        false,
        false,
    )
    .await
    .expect("create child branch");

    let mut revisions = Vec::with_capacity(count as usize);
    let mut parent = branch_point;
    for hint in 1..=count {
        let (revision, number) =
            push_chained_revision(repository, branch_id, parent, Hash::default(), hint).await;
        revisions.push((number, revision));
        parent = revision;
    }
    revisions.reverse();
    (branch_id, revisions)
}

fn strategy_header(
    response: &tonic::Response<lore_proto::lore::revision::v1::RevisionListResponse>,
) -> Option<&str> {
    response
        .metadata()
        .get(REVISION_LIST_STRATEGY_HEADER)
        .map(|value| value.to_str().unwrap())
}

mod handler_on_child_branch {
    use super::*;

    /// Parent branch `main` holds revisions 1..=250. The child branches from
    /// revision `branch_point` and gets `child_count` revisions of its own.
    struct Fixture {
        repository: RepositoryId,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        child: BranchId,
        parent_signatures: Vec<Hash>,
        child_revisions: Vec<(u64, Hash)>,
    }

    impl Fixture {
        async fn call(
            &self,
            number: u64,
            acceleration: lore_server::grpc::server::RevisionListAcceleration,
        ) -> tonic::Response<lore_proto::lore::revision::v1::RevisionListResponse> {
            handler(
                make_request_identifier(self.repository, self.child, number),
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("Request failed")
        }

        fn parent_signature(&self, number: u64) -> Hash {
            self.parent_signatures[250 - number as usize]
        }
    }

    async fn with_fixture<F, Fut>(branch_point: u64, child_count: u64, body: F)
    where
        F: FnOnce(Fixture) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let (parent, parent_signatures) =
                create_branch_with_history(&repository_context, 250).await;
            let (child, child_revisions) = create_child_branch_with_history(
                &repository_context,
                vec![BranchPoint {
                    branch: parent,
                    revision: parent_signatures[250 - branch_point as usize],
                }],
                child_count,
            )
            .await;
            body(Fixture {
                repository,
                immutable_store,
                mutable_store,
                child,
                parent_signatures,
                child_revisions,
            })
            .await;
        }))
        .await;
    }

    #[tokio::test]
    async fn number_below_the_branch_point_is_served_from_the_ancestors_list_cache() {
        with_fixture(220, 10, |fixture| async move {
            let response = fixture
                .call(
                    50,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                )
                .await;
            assert_eq!(strategy_header(&response), Some("list-cache"));
            let inner = response.into_inner();
            assert_eq!(inner.items.len(), 100);
            assert_eq!(inner.items[0].number, 100);
            assert_eq!(
                Hash::from(inner.items[0].signature.as_ref()),
                fixture.parent_signature(100),
            );
        })
        .await;
    }

    #[tokio::test]
    async fn number_below_the_branch_point_uses_the_ancestors_step_key() {
        with_fixture(220, 10, |fixture| async move {
            let acceleration = lore_server::grpc::server::RevisionListAcceleration {
                step_keys: true,
                list_cache: false,
            };
            let response = fixture.call(120, acceleration).await;
            assert_eq!(strategy_header(&response), Some("history-step"));
            let inner = response.into_inner();
            assert_eq!(inner.items[0].number, 120);
            assert_eq!(
                Hash::from(inner.items[0].signature.as_ref()),
                fixture.parent_signature(120),
            );
        })
        .await;
    }

    #[tokio::test]
    async fn cached_segment_is_cut_at_the_branch_point() {
        with_fixture(150, 10, |fixture| async move {
            let response = fixture
                .call(
                    120,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                )
                .await;
            assert_eq!(strategy_header(&response), Some("list-cache"));
            let inner = response.into_inner();
            let numbers: Vec<u64> = inner.items.iter().map(|item| item.number).collect();
            assert_eq!(numbers, (101..=150).rev().collect::<Vec<u64>>());
            assert_eq!(
                Hash::from(inner.items[0].signature.as_ref()),
                fixture.parent_signature(150),
            );
        })
        .await;
    }

    #[tokio::test]
    async fn page_ending_at_the_branch_point_points_forward_into_the_child() {
        with_fixture(150, 10, |fixture| async move {
            let response = fixture
                .call(
                    150,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .into_inner();
            let (first_number, first_signature) =
                *fixture.child_revisions.last().expect("child has revisions");
            assert_eq!(first_number, 151);
            assert_eq!(
                Hash::from(response.signature_forward.expect("forward cursor").as_ref()),
                first_signature,
            );
        })
        .await;
    }

    #[tokio::test]
    async fn page_below_the_branch_point_points_forward_along_shared_history() {
        with_fixture(150, 10, |fixture| async move {
            let response = fixture
                .call(
                    50,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .into_inner();
            assert_eq!(
                Hash::from(response.signature_forward.expect("forward cursor").as_ref()),
                fixture.parent_signature(101),
            );
        })
        .await;
    }

    #[tokio::test]
    async fn number_covered_by_the_childs_own_cache_is_served_uncut() {
        with_fixture(150, 60, |fixture| async move {
            let response = fixture
                .call(
                    120,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                )
                .await;
            assert_eq!(strategy_header(&response), Some("list-cache"));
            let inner = response.into_inner();
            let numbers: Vec<u64> = inner.items.iter().map(|item| item.number).collect();
            assert_eq!(numbers, (101..=200).rev().collect::<Vec<u64>>());
            let child_200 = fixture
                .child_revisions
                .iter()
                .find(|(number, _)| *number == 200)
                .map(|(_, signature)| *signature)
                .expect("child revision 200");
            assert_eq!(Hash::from(inner.items[0].signature.as_ref()), child_200,);
        })
        .await;
    }

    #[tokio::test]
    async fn signature_of_a_child_revision_lists_across_the_branch_point() {
        with_fixture(150, 10, |fixture| async move {
            let (tip_number, tip_signature) = fixture.child_revisions[0];
            let inner = handler(
                make_request_signature(fixture.repository, tip_signature),
                fixture.immutable_store.clone(),
                fixture.mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(tip_number, 160);
            let numbers: Vec<u64> = inner.items.iter().map(|item| item.number).collect();
            assert_eq!(numbers, (101..=160).rev().collect::<Vec<u64>>());
            assert_eq!(Hash::from(inner.items[0].signature.as_ref()), tip_signature,);
            assert!(inner.signature_forward.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn child_without_revisions_has_no_forward_cursor_at_its_tip() {
        with_fixture(150, 0, |fixture| async move {
            let response = fixture
                .call(
                    0,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .into_inner();
            assert_eq!(response.items[0].number, 150);
            assert!(response.signature_forward.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn number_above_the_branch_point_is_served_from_the_child() {
        with_fixture(150, 10, |fixture| async move {
            let response = fixture
                .call(
                    155,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .into_inner();
            assert_eq!(response.items[0].number, 155);
            let child_signature = fixture
                .child_revisions
                .iter()
                .find(|(number, _)| *number == 155)
                .map(|(_, signature)| *signature)
                .expect("child revision 155");
            assert_eq!(
                Hash::from(response.items[0].signature.as_ref()),
                child_signature,
            );
        })
        .await;
    }
}

mod handler_on_grandchild_branch {
    use super::*;

    /// `main` holds 1..=250, `child` branches at 150 and gets 60 revisions
    /// (151..=210), and `grandchild` branches from the child's 155 and gets 5.
    struct Fixture {
        repository: RepositoryId,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        grandchild: BranchId,
        main_signatures: Vec<Hash>,
        child_revisions: Vec<(u64, Hash)>,
        grandchild_revisions: Vec<(u64, Hash)>,
    }

    impl Fixture {
        async fn call(
            &self,
            number: u64,
        ) -> tonic::Response<lore_proto::lore::revision::v1::RevisionListResponse> {
            handler(
                make_request_identifier(self.repository, self.grandchild, number),
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
        }

        fn child_signature(&self, number: u64) -> Hash {
            self.child_revisions
                .iter()
                .find(|(candidate, _)| *candidate == number)
                .map(|(_, signature)| *signature)
                .expect("child revision")
        }
    }

    async fn with_fixture<F, Fut>(body: F)
    where
        F: FnOnce(Fixture) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let (main, main_signatures) =
                create_branch_with_history(&repository_context, 250).await;
            let main_point = BranchPoint {
                branch: main,
                revision: main_signatures[250 - 150],
            };
            let (child, child_revisions) =
                create_child_branch_with_history(&repository_context, vec![main_point.clone()], 60)
                    .await;
            let child_point = BranchPoint {
                branch: child,
                revision: child_revisions
                    .iter()
                    .find(|(number, _)| *number == 155)
                    .map(|(_, signature)| *signature)
                    .expect("child revision 155"),
            };
            let (grandchild, grandchild_revisions) = create_child_branch_with_history(
                &repository_context,
                vec![child_point, main_point],
                5,
            )
            .await;
            body(Fixture {
                repository,
                immutable_store,
                mutable_store,
                grandchild,
                main_signatures,
                child_revisions,
                grandchild_revisions,
            })
            .await;
        }))
        .await;
    }

    #[tokio::test]
    async fn number_between_the_branch_points_is_served_from_the_middle_branch() {
        with_fixture(|fixture| async move {
            let response = fixture.call(153).await;
            assert_eq!(strategy_header(&response), Some("list-cache"));
            let inner = response.into_inner();
            let numbers: Vec<u64> = inner.items.iter().map(|item| item.number).collect();
            assert_eq!(numbers, (101..=155).rev().collect::<Vec<u64>>());
            assert_eq!(
                Hash::from(inner.items[0].signature.as_ref()),
                fixture.child_signature(155),
            );
        })
        .await;
    }

    #[tokio::test]
    async fn number_below_both_branch_points_is_served_from_the_root_branch() {
        with_fixture(|fixture| async move {
            let response = fixture.call(120).await;
            assert_eq!(strategy_header(&response), Some("list-cache"));
            let inner = response.into_inner();
            let numbers: Vec<u64> = inner.items.iter().map(|item| item.number).collect();
            assert_eq!(numbers, (101..=150).rev().collect::<Vec<u64>>());
            assert_eq!(
                Hash::from(inner.items[0].signature.as_ref()),
                fixture.main_signatures[250 - 150],
            );
        })
        .await;
    }

    #[tokio::test]
    async fn number_above_both_branch_points_is_served_from_the_grandchild() {
        with_fixture(|fixture| async move {
            let response = fixture.call(158).await.into_inner();
            assert_eq!(response.items[0].number, 158);
            let grandchild_158 = fixture
                .grandchild_revisions
                .iter()
                .find(|(number, _)| *number == 158)
                .map(|(_, signature)| *signature)
                .expect("grandchild revision 158");
            assert_eq!(
                Hash::from(response.items[0].signature.as_ref()),
                grandchild_158,
            );
            assert_ne!(grandchild_158, fixture.child_signature(158));
        })
        .await;
    }
}

/// A start by a number the branch's first-parent line skips answers the page
/// holding the line's nearest revision numbered below that number.
mod by_skipped_number {
    use lore_proto::lore::revision::v1::RevisionListResponse;
    use lore_server::grpc::server::RevisionListAcceleration;
    use lore_storage::test_util::HookedImmutableStore;
    use lore_storage::test_util::HookedStore;
    use lore_storage::test_util::ImmutableCall;
    use lore_storage::test_util::StoreCall;

    use super::*;

    const LIST_CACHE_OFF: RevisionListAcceleration = RevisionListAcceleration {
        step_keys: true,
        list_cache: false,
    };

    /// Each acceleration setting, the list cache without skip pointers among
    /// them, which a server's configuration doesn't produce.
    const SETTINGS: [RevisionListAcceleration; 4] = [
        RevisionListAcceleration {
            step_keys: true,
            list_cache: true,
        },
        LIST_CACHE_OFF,
        RevisionListAcceleration {
            step_keys: false,
            list_cache: true,
        },
        RevisionListAcceleration {
            step_keys: false,
            list_cache: false,
        },
    ];

    /// A branch and its first-parent line, by revision number.
    struct Line {
        repository: RepositoryId,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        branch: BranchId,
        line: BTreeMap<u64, Hash>,
    }

    impl Line {
        async fn call(
            &self,
            number: u64,
            acceleration: RevisionListAcceleration,
        ) -> Result<tonic::Response<RevisionListResponse>, tonic::Status> {
            self.call_on(
                number,
                acceleration,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            )
            .await
        }

        async fn call_on(
            &self,
            number: u64,
            acceleration: RevisionListAcceleration,
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Result<tonic::Response<RevisionListResponse>, tonic::Status> {
            handler(
                make_request_identifier(self.repository, self.branch, number),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
        }

        /// The page for `number`, which must be listed, with its strategy.
        async fn page(
            &self,
            number: u64,
            acceleration: RevisionListAcceleration,
        ) -> (String, RevisionListResponse) {
            let response = self
                .call(number, acceleration)
                .await
                .unwrap_or_else(|err| panic!("number {number} lists a page: {err}"));
            let strategy = strategy_header(&response)
                .expect("strategy header")
                .to_owned();
            (strategy, response.into_inner())
        }

        /// The number of the line's highest revision numbered at or below
        /// `number`.
        fn nearest(&self, number: u64) -> Option<u64> {
            self.line
                .range(..=number)
                .next_back()
                .map(|(found, _)| *found)
        }

        fn pointer_key(
            &self,
            branch: BranchId,
            boundary: u64,
        ) -> (Hash, lore_base::types::KeyType) {
            branch::revision_step_key(
                repository::SALT_LORE,
                self.repository,
                branch,
                boundary,
                DEFAULT_HISTORY_STEP_SIZE,
            )
        }

        /// The skip pointer of `branch` at `boundary`, `None` where unsealed.
        async fn pointer_of(&self, branch: BranchId, boundary: u64) -> Option<Hash> {
            let (key, key_type) = self.pointer_key(branch, boundary);
            self.mutable_store
                .clone()
                .load(self.repository, key, key_type)
                .await
                .ok()
                .filter(|pointer| !pointer.is_zero())
        }

        async fn pointer(&self, boundary: u64) -> Option<Hash> {
            self.pointer_of(self.branch, boundary).await
        }

        async fn set_pointer_of(&self, branch: BranchId, boundary: u64, revision: Hash) {
            let (key, key_type) = self.pointer_key(branch, boundary);
            self.mutable_store
                .clone()
                .store(self.repository, key, revision, key_type)
                .await
                .expect("store skip pointer");
        }

        /// Storing zero deletes the mutable store entry.
        async fn remove_pointers(&self, branch: BranchId, boundaries: &[u64]) {
            for &boundary in boundaries {
                self.set_pointer_of(branch, boundary, Hash::default()).await;
            }
        }

        fn segment_key(&self, boundary: u64) -> (Hash, lore_base::types::KeyType) {
            branch::revision_list_step_key(
                repository::SALT_LORE,
                self.repository,
                self.branch,
                boundary,
                DEFAULT_HISTORY_STEP_SIZE,
            )
        }

        async fn segment_cached(&self, boundary: u64) -> bool {
            let (key, key_type) = self.segment_key(boundary);
            self.mutable_store
                .clone()
                .load(self.repository, key, key_type)
                .await
                .is_ok_and(|blob| !blob.is_zero())
        }

        async fn remove_segment(&self, boundary: u64) {
            let (key, key_type) = self.segment_key(boundary);
            self.mutable_store
                .clone()
                .store(self.repository, key, Hash::default(), key_type)
                .await
                .expect("evict cached segment");
        }

        /// Asserts that `page` lists a run of the line, newest first, with
        /// the cursors of the line's revisions on either side of it, and
        /// that it holds the line's nearest revision at or below `number`.
        /// Returns the numbers the page lists.
        fn assert_answers(&self, number: u64, page: &RevisionListResponse) -> Vec<u64> {
            let nearest = self
                .nearest(number)
                .unwrap_or_else(|| panic!("the line holds a revision at or below {number}"));
            let numbers: Vec<u64> = page.items.iter().map(|item| item.number).collect();
            assert!(
                numbers.contains(&nearest),
                "the page for {number} holds {nearest}: {numbers:?}"
            );
            for item in &page.items {
                assert_eq!(
                    Some(&Hash::from(item.signature.as_ref())),
                    self.line.get(&item.number),
                    "the page for {number} lists the line's revision {}",
                    item.number
                );
            }
            for pair in numbers.windows(2) {
                assert_eq!(
                    self.line
                        .range(..pair[0])
                        .next_back()
                        .map(|(below, _)| *below),
                    Some(pair[1]),
                    "the page for {number} follows the line: {numbers:?}"
                );
            }
            let cursor = |cursor: &Option<bytes::Bytes>| {
                cursor.as_ref().map(|cursor| Hash::from(cursor.as_ref()))
            };
            let bottom = *numbers.last().expect("the page lists revisions");
            assert_eq!(
                cursor(&page.signature_backward),
                self.line
                    .range(..bottom)
                    .next_back()
                    .map(|(_, revision)| *revision),
                "the page for {number} points back to the line below it"
            );
            assert_eq!(
                cursor(&page.signature_forward),
                self.line
                    .range(numbers[0] + 1..)
                    .next()
                    .map(|(_, revision)| *revision),
                "the page for {number} points forward to the line above it"
            );
            numbers
        }
    }

    fn run(top: u64, bottom: u64) -> Vec<u64> {
        (bottom..=top).rev().collect()
    }

    async fn with_stores<F, Fut>(body: F)
    where
        F: FnOnce(
            RepositoryId,
            Arc<RepositoryContext>,
            Arc<dyn lore_storage::ImmutableStore>,
            Arc<dyn lore_storage::MutableStore>,
        ) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            body(repository, context, immutable_store, mutable_store).await;
        }))
        .await;
    }

    /// A line of `linear_before` revisions, a merge whose other parent is
    /// numbered `jump_other_number`, then `linear_after` more, as
    /// [`create_branch_with_jump_history`] builds it.
    async fn with_jump<F, Fut>(
        linear_before: u64,
        jump_other_number: u64,
        linear_after: u64,
        body: F,
    ) where
        F: FnOnce(Line) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_stores(
            |repository, context, immutable_store, mutable_store| async move {
                let (branch, revisions) = create_branch_with_jump_history(
                    &context,
                    linear_before,
                    jump_other_number,
                    linear_after,
                )
                .await;
                body(Line {
                    repository,
                    immutable_store,
                    mutable_store,
                    branch,
                    line: revisions,
                })
                .await;
            },
        )
        .await;
    }

    /// Every number up to and past the branch's latest revision answers
    /// the page holding the line's nearest revision at or below it, or
    /// `NotFound` where the line holds none, under each acceleration
    /// setting, numbers the line holds and number 0 among them.
    #[tokio::test]
    async fn every_number_answers_the_page_of_the_nearest_revision_under_each_setting() {
        for (before, other, after) in [(150, 399, 5), (99, 104, 145), (120, 140, 70), (99, 104, 0)]
        {
            for acceleration in SETTINGS {
                with_jump(before, other, after, |line| async move {
                    let latest = *line.line.keys().next_back().expect("latest");
                    for number in 1..=latest + 2 {
                        match line.call(number, acceleration).await {
                            Ok(response) if number <= latest => {
                                line.assert_answers(number, &response.into_inner());
                            }
                            Ok(_) => panic!("{number} is above the latest revision {latest}"),
                            Err(status) => {
                                assert!(number > latest, "{number}: {status:?}");
                                assert_eq!(status.code(), tonic::Code::NotFound);
                            }
                        }
                    }
                    let (_, page) = line.page(0, acceleration).await;
                    assert_eq!(line.assert_answers(latest, &page)[0], latest);
                })
                .await;
            }
        }
    }

    /// A boundary the line skips is answered from its skip pointer, which
    /// holds the line's highest revision at or below it, with no walk from
    /// the branch's latest revision.
    #[tokio::test]
    async fn a_skipped_boundary_is_answered_from_its_skip_pointer() {
        with_jump(150, 399, 5, |line| async move {
            for number in [200, 300] {
                let (strategy, page) = line.page(number, LIST_CACHE_OFF).await;
                assert_eq!(strategy, "history-step", "{number}");
                assert_eq!(line.assert_answers(number, &page), run(150, 101));

                let (strategy, page) = line.page(number, RevisionListAcceleration::default()).await;
                assert_eq!(strategy, "list-cache", "{number}");
                assert_eq!(line.assert_answers(number, &page), run(150, 101));
            }
        })
        .await;
    }

    /// A skipped number between boundaries is answered from the skip pointer
    /// of its boundary where the pointer is numbered at or below it, as
    /// where its segment holds no revision, and from its segment or a walk
    /// down from the pointer where the segment holds revisions above it.
    #[tokio::test]
    async fn a_skipped_number_between_boundaries_is_answered_without_a_walk_from_latest() {
        with_jump(150, 399, 5, |line| async move {
            for number in [151, 250, 399] {
                let (strategy, page) = line.page(number, LIST_CACHE_OFF).await;
                assert_eq!(strategy, "history-step", "{number}");
                assert_eq!(line.assert_answers(number, &page), run(150, 101));
                let (strategy, page) = line.page(number, RevisionListAcceleration::default()).await;
                assert_eq!(strategy, "list-cache", "{number}");
                assert_eq!(line.assert_answers(number, &page), run(150, 101));
            }
        })
        .await;
        with_jump(99, 104, 145, |line| async move {
            for number in [100, 102, 104] {
                let (strategy, page) = line.page(number, LIST_CACHE_OFF).await;
                assert_eq!(strategy, "history-step", "{number}");
                assert_eq!(line.assert_answers(number, &page), run(99, 1));
                let (strategy, page) = line.page(number, RevisionListAcceleration::default()).await;
                assert_eq!(strategy, "list-cache", "{number}");
                assert_eq!(line.assert_answers(number, &page), run(99, 1));
            }
        })
        .await;
    }

    /// A gap inside one segment is answered from the cached segment, which
    /// holds the nearest revision below it, or from a walk down from the
    /// segment's skip pointer.
    #[tokio::test]
    async fn a_gap_inside_a_segment_is_answered_from_the_segment() {
        with_jump(120, 140, 70, |line| async move {
            let (strategy, page) = line.page(130, RevisionListAcceleration::default()).await;
            assert_eq!(strategy, "list-cache");
            let mut expected = run(200, 141);
            expected.extend(run(120, 101));
            assert_eq!(line.assert_answers(130, &page), expected);

            let (strategy, page) = line.page(130, LIST_CACHE_OFF).await;
            assert_eq!(strategy, "history-step");
            assert_eq!(line.assert_answers(130, &page), run(120, 101));
        })
        .await;
    }

    /// The band holding the branch's latest revision has no sealed boundary
    /// above it, so a skipped number there is answered by a walk from the
    /// latest revision, and one on the sealed boundary below by its pointer.
    #[tokio::test]
    async fn a_gap_in_the_latest_band_is_answered_by_a_walk_from_latest() {
        with_jump(99, 104, 0, |line| async move {
            assert_eq!(line.pointer(200).await, None);
            let (strategy, page) = line.page(100, LIST_CACHE_OFF).await;
            assert_eq!(strategy, "history-step");
            assert_eq!(line.assert_answers(100, &page), run(99, 1));
            for acceleration in [LIST_CACHE_OFF, RevisionListAcceleration::default()] {
                let (strategy, page) = line.page(102, acceleration).await;
                assert_eq!(strategy, "full-iteration");
                assert_eq!(line.assert_answers(102, &page), run(99, 1));
            }
        })
        .await;
    }

    /// With the skip pointers removed, the walk from the branch's latest
    /// revision answers, and seals each boundary it crossed with the line's
    /// highest revision at or below it, so the next request in the range
    /// reads the pointer.
    #[tokio::test]
    async fn the_walk_from_latest_answers_and_seals_the_boundaries_it_crossed() {
        with_jump(150, 399, 5, |line| async move {
            line.remove_pointers(line.branch, &[200, 300, 400]).await;

            let (strategy, page) = line.page(300, LIST_CACHE_OFF).await;
            assert_eq!(strategy, "full-iteration");
            assert_eq!(line.assert_answers(300, &page), run(150, 101));
            for boundary in [200, 300] {
                assert_eq!(line.pointer(boundary).await, Some(line.line[&150]));
            }
            assert_eq!(line.pointer(400).await, Some(line.line[&400]));

            let (strategy, page) = line.page(250, LIST_CACHE_OFF).await;
            assert_eq!(strategy, "history-step");
            assert_eq!(line.assert_answers(250, &page), run(150, 101));
        })
        .await;
    }

    /// A push of several revisions at once seals only the boundaries between
    /// the new latest revision and its first parent, so the line below is
    /// answered by the walk from latest, which seals the boundaries it
    /// crossed.
    #[tokio::test]
    async fn boundaries_a_push_of_several_revisions_never_sealed_are_sealed_by_the_walk() {
        with_stores(
            |repository, context, immutable_store, mutable_store| async move {
                let (branch, signatures) = create_branch_with_history(&context, 150).await;
                let mut revisions: BTreeMap<u64, Hash> = (1..=150)
                    .map(|number| (number, signatures[150 - number as usize]))
                    .collect();
                let other = serialize_detached_revision(&context, branch, 399).await;
                let mut parent = revisions[&150];
                let mut other_parent = other;
                for number in 400..=405 {
                    parent =
                        serialize_chained_revision(&context, branch, parent, other_parent, number)
                            .await;
                    other_parent = Hash::default();
                    revisions.insert(number, parent);
                }
                let (pushed, number) = push_revision(&context, branch, parent).await;
                assert_eq!((pushed, number), (parent, 405));
                let line = Line {
                    repository,
                    immutable_store,
                    mutable_store,
                    branch,
                    line: revisions,
                };
                for boundary in [200, 300, 400] {
                    assert_eq!(line.pointer(boundary).await, None, "{boundary}");
                }

                let (strategy, page) = line.page(300, LIST_CACHE_OFF).await;
                assert_eq!(strategy, "full-iteration");
                assert_eq!(line.assert_answers(300, &page), run(150, 101));
                for boundary in [200, 300] {
                    assert_eq!(line.pointer(boundary).await, Some(line.line[&150]));
                }
                assert_eq!(line.pointer(400).await, Some(line.line[&400]));

                let (strategy, page) = line.page(250, LIST_CACHE_OFF).await;
                assert_eq!(strategy, "history-step");
                assert_eq!(line.assert_answers(250, &page), run(150, 101));
            },
        )
        .await;
    }

    /// A segment that is not cached is backfilled from the skip pointer
    /// above it, and the backfill seals the boundaries its walk crossed.
    #[tokio::test]
    async fn an_evicted_segment_is_backfilled_and_its_walk_seals_the_boundaries_it_crossed() {
        with_jump(99, 104, 245, |line| async move {
            line.remove_segment(200).await;
            line.remove_pointers(line.branch, &[100]).await;

            let (_, page) = line.page(102, RevisionListAcceleration::default()).await;
            assert_eq!(line.assert_answers(102, &page), run(99, 1));
            assert!(line.segment_cached(200).await);
            assert_eq!(line.pointer(100).await, Some(line.line[&99]));
        })
        .await;
    }

    /// The walk down from a skip pointer above the requested number seals
    /// the boundaries it crossed.
    #[tokio::test]
    async fn the_walk_down_from_a_skip_pointer_seals_the_boundaries_it_crossed() {
        with_jump(99, 104, 145, |line| async move {
            line.remove_pointers(line.branch, &[100]).await;

            let (strategy, page) = line.page(102, LIST_CACHE_OFF).await;
            assert_eq!(strategy, "history-step");
            assert_eq!(line.assert_answers(102, &page), run(99, 1));
            assert_eq!(line.pointer(100).await, Some(line.line[&99]));
        })
        .await;
    }

    /// A skip pointer numbered above its boundary does not describe the line,
    /// so it is not followed, and the walk from latest answers and seals the
    /// boundary again.
    #[tokio::test]
    async fn a_skip_pointer_above_its_boundary_is_not_followed() {
        with_jump(150, 399, 5, |line| async move {
            line.set_pointer_of(line.branch, 200, line.line[&405]).await;

            let (strategy, page) = line.page(200, LIST_CACHE_OFF).await;
            assert_eq!(strategy, "full-iteration");
            assert_eq!(line.assert_answers(200, &page), run(150, 101));
            assert_eq!(line.pointer(200).await, Some(line.line[&150]));
        })
        .await;
    }

    /// With skip pointers off, no request writes one.
    #[tokio::test]
    async fn with_skip_pointers_off_no_walk_seals_a_boundary() {
        for acceleration in &SETTINGS[2..] {
            let acceleration = *acceleration;
            with_jump(150, 399, 5, |line| async move {
                line.remove_pointers(line.branch, &[200, 300]).await;
                let (_, page) = line.page(250, acceleration).await;
                line.assert_answers(250, &page);
                assert_eq!(line.pointer(200).await, None);
                assert_eq!(line.pointer(300).await, None);
            })
            .await;
        }
    }

    /// A number with no revision of the line at or below it answers `NotFound`,
    /// as does any number on a branch without revisions.
    #[tokio::test]
    async fn a_number_below_the_line_answers_not_found() {
        for acceleration in SETTINGS {
            with_jump(0, 99, 0, |line| async move {
                assert_eq!(line.line.keys().copied().collect::<Vec<_>>(), vec![100]);
                for number in [1, 50, 99] {
                    let status = line
                        .call(number, acceleration)
                        .await
                        .expect_err("no revision at or below");
                    assert_eq!(status.code(), tonic::Code::NotFound, "{number}");
                }
                let (_, page) = line.page(100, acceleration).await;
                assert_eq!(line.assert_answers(100, &page), vec![100]);
            })
            .await;
            with_stores(
                |repository, context, immutable_store, mutable_store| async move {
                    let (branch, _) = create_branch_with_history(&context, 0).await;
                    let line = Line {
                        repository,
                        immutable_store,
                        mutable_store,
                        branch,
                        line: BTreeMap::new(),
                    };
                    let status = line
                        .call(5, acceleration)
                        .await
                        .expect_err("an empty branch holds no revision");
                    assert_eq!(status.code(), tonic::Code::NotFound);
                },
            )
            .await;
        }
    }

    /// A number above the branch's latest revision answers `NotFound`.
    #[tokio::test]
    async fn a_number_above_the_latest_revision_answers_not_found() {
        for acceleration in SETTINGS {
            with_jump(150, 399, 5, |line| async move {
                for number in [406, 450, 500, 1000] {
                    let status = line
                        .call(number, acceleration)
                        .await
                        .expect_err("above the latest revision");
                    assert_eq!(status.code(), tonic::Code::NotFound, "{number}");
                }
            })
            .await;
        }
    }

    /// A store asking the caller to slow down, for the skip pointer, the
    /// cached segment, the skip pointer a backfill walks from, or the branch's
    /// latest revision a walk starts from, is reported as `RESOURCE_EXHAUSTED`,
    /// never as `NotFound` or another page.
    #[tokio::test]
    async fn a_store_asking_to_slow_down_is_reported_as_resource_exhausted() {
        let slow = |line: &Line, key: Hash| {
            FailingLoadStore::for_key(
                line.mutable_store.clone(),
                key,
                StoreError::from(lore_base::error::SlowDown),
            )
        };
        with_jump(150, 399, 5, |line| async move {
            let latest_key = branch::mutable_key(
                repository::SALT_LORE,
                branch::LATEST,
                line.repository,
                line.branch,
            )
            .0;
            for (case, acceleration, key) in [
                (
                    "skip pointer",
                    LIST_CACHE_OFF,
                    line.pointer_key(line.branch, 300).0,
                ),
                ("cached segment", SETTINGS[0], line.segment_key(300).0),
                ("branch latest", SETTINGS[3], latest_key),
            ] {
                let status = line
                    .call_on(
                        250,
                        acceleration,
                        line.immutable_store.clone(),
                        slow(&line, key),
                    )
                    .await
                    .expect_err(case);
                assert_eq!(status.code(), tonic::Code::ResourceExhausted, "{case}");
            }
        })
        .await;
        with_jump(99, 104, 245, |line| async move {
            line.remove_segment(200).await;
            let status = line
                .call_on(
                    102,
                    SETTINGS[0],
                    line.immutable_store.clone(),
                    slow(&line, line.pointer_key(line.branch, 300).0),
                )
                .await
                .expect_err("the backfill's skip pointer");
            assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        })
        .await;
    }

    /// A branch whose latest revision the store fails to read answers an
    /// internal error, never `NotFound`, for number 0 and for a number a
    /// walk from the latest revision answers, under each acceleration
    /// setting.
    #[tokio::test]
    async fn an_unreadable_latest_revision_answers_internal() {
        with_jump(150, 399, 5, |line| async move {
            let latest_key = branch::mutable_key(
                repository::SALT_LORE,
                branch::LATEST,
                line.repository,
                line.branch,
            )
            .0;
            let failing = FailingLoadStore::for_key(
                line.mutable_store.clone(),
                latest_key,
                StoreError::internal("failing the latest revision"),
            );
            for acceleration in SETTINGS {
                for number in [0, 250] {
                    let status = line
                        .call_on(
                            number,
                            acceleration,
                            line.immutable_store.clone(),
                            failing.clone(),
                        )
                        .await
                        .expect_err("the latest revision can't be read");
                    assert_eq!(
                        status.code(),
                        tonic::Code::Internal,
                        "{number} {acceleration:?}"
                    );
                }
            }
        })
        .await;
    }

    /// A state the immutable store keeps asking the caller to slow down for
    /// reads as missing on a server, where no remote is in reach. A walk
    /// that needs it fails the request as internal, and one that does not
    /// answers the page: never `NotFound` or another page.
    #[tokio::test]
    async fn a_state_the_store_slows_down_for_fails_only_the_walk_that_needs_it() {
        let slowed = |line: &Line, number: u64| -> Arc<dyn lore_storage::ImmutableStore> {
            let slowed = line.line[&number];
            HookedImmutableStore::wrap(line.immutable_store.clone(), move |call, _, address| {
                if call == ImmutableCall::Get && address.hash == slowed {
                    return Err(StoreError::from(lore_base::error::SlowDown));
                }
                Ok(())
            })
        };
        with_jump(150, 399, 5, |line| async move {
            for (case, acceleration, number) in [
                ("the skip pointer's revision", LIST_CACHE_OFF, 150),
                ("the walk from latest", SETTINGS[3], 402),
            ] {
                let status = line
                    .call_on(
                        250,
                        acceleration,
                        slowed(&line, number),
                        line.mutable_store.clone(),
                    )
                    .await
                    .expect_err(case);
                assert_eq!(status.code(), tonic::Code::Internal, "{case}");
            }
        })
        .await;
        with_jump(99, 104, 145, |line| async move {
            let status = line
                .call_on(
                    102,
                    LIST_CACHE_OFF,
                    slowed(&line, 150),
                    line.mutable_store.clone(),
                )
                .await
                .expect_err("the walk down from the skip pointer");
            assert_eq!(status.code(), tonic::Code::Internal);
        })
        .await;
        with_jump(99, 104, 245, |line| async move {
            line.remove_segment(200).await;
            let response = line
                .call_on(
                    102,
                    SETTINGS[0],
                    slowed(&line, 250),
                    line.mutable_store.clone(),
                )
                .await
                .expect("the walk down from the skip pointer needs no state of the backfill");
            assert_eq!(line.assert_answers(102, &response.into_inner()), run(99, 1));
            assert!(!line.segment_cached(200).await);
        })
        .await;
    }

    /// Sealing is best effort: a store that refuses every write still
    /// answers the page.
    #[tokio::test]
    async fn a_failed_seal_fails_no_request() {
        with_jump(150, 399, 5, |line| async move {
            line.remove_pointers(line.branch, &[200, 300]).await;
            let response = line
                .call_on(
                    300,
                    LIST_CACHE_OFF,
                    line.immutable_store.clone(),
                    HookedStore::wrap(
                        line.mutable_store.clone(),
                        |call, _, _| {
                            if call == StoreCall::Store {
                                return Err(StoreError::internal("refusing every store"));
                            }
                            Ok(())
                        },
                        None,
                    ),
                )
                .await
                .expect("a refused seal fails nothing");
            assert_eq!(strategy_header(&response), Some("full-iteration"));
            assert_eq!(
                line.assert_answers(300, &response.into_inner()),
                run(150, 101)
            );
            assert_eq!(line.pointer(300).await, None);
        })
        .await;
    }

    /// Two requests in the same gap at once answer the same page and leave
    /// the same pointers.
    #[tokio::test]
    async fn concurrent_requests_in_a_gap_answer_the_same_page() {
        with_jump(150, 399, 5, |line| async move {
            line.remove_pointers(line.branch, &[200, 300]).await;
            let (first, second) = tokio::join!(
                line.call(250, LIST_CACHE_OFF),
                line.call(300, LIST_CACHE_OFF)
            );
            let first = first.expect("first request").into_inner();
            let second = second.expect("second request").into_inner();
            assert_eq!(line.assert_answers(250, &first), run(150, 101));
            assert_eq!(line.assert_answers(300, &second), run(150, 101));
            assert_eq!(first.signature_backward, second.signature_backward);
            assert_eq!(first.signature_forward, second.signature_forward);
            for boundary in [200, 300] {
                assert_eq!(line.pointer(boundary).await, Some(line.line[&150]));
            }
        })
        .await;
    }

    /// Paging on from the page answered for a skipped number, through each
    /// cursor, reaches every revision of the line.
    #[tokio::test]
    async fn paging_from_an_answered_page_reaches_every_revision() {
        for (before, other, after, number) in [(150, 399, 5, 250), (99, 104, 145, 102)] {
            for acceleration in SETTINGS {
                with_jump(before, other, after, |line| async move {
                    let (_, page) = line.page(number, acceleration).await;
                    let mut reached: BTreeSet<u64> =
                        line.assert_answers(number, &page).into_iter().collect();
                    for backward in [true, false] {
                        let mut cursor = if backward {
                            page.signature_backward.clone()
                        } else {
                            page.signature_forward.clone()
                        };
                        while let Some(signature) = cursor {
                            let next = handler(
                                make_request_signature(
                                    line.repository,
                                    Hash::from(signature.as_ref()),
                                ),
                                line.immutable_store.clone(),
                                line.mutable_store.clone(),
                                DEFAULT_HISTORY_STEP_SIZE,
                                acceleration,
                                &make_instruments(),
                            )
                            .await
                            .expect("a cursor lists a page")
                            .into_inner();
                            reached.extend(next.items.iter().map(|item| item.number));
                            cursor = if backward {
                                next.signature_backward
                            } else {
                                next.signature_forward
                            };
                        }
                    }
                    assert_eq!(reached, line.line.keys().copied().collect());
                })
                .await;
            }
        }
    }

    /// A child branch made from the parent's revision 140, whose first
    /// revision is a merge numbered 400, so the child's line skips 141 to
    /// 399, then 401 to 405. The parent holds 1 to 250.
    async fn with_straddling_child<F, Fut>(body: F)
    where
        F: FnOnce(Line, BranchId) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_stores(
            |repository, context, immutable_store, mutable_store| async move {
                let (parent, parent_signatures) = create_branch_with_history(&context, 250).await;
                let point = parent_signatures[250 - 140];
                let (child, _) = create_child_branch_with_history(
                    &context,
                    vec![BranchPoint {
                        branch: parent,
                        revision: point,
                    }],
                    0,
                )
                .await;
                let mut line: BTreeMap<u64, Hash> = (1..=140)
                    .map(|number| (number, parent_signatures[250 - number as usize]))
                    .collect();
                let other = serialize_detached_revision(&context, child, 399).await;
                let (mut revision, mut number) =
                    push_chained_revision(&context, child, point, other, 0).await;
                line.insert(number, revision);
                while number < 405 {
                    (revision, number) = push_chained_revision(
                        &context,
                        child,
                        revision,
                        Hash::default(),
                        number + 1,
                    )
                    .await;
                    line.insert(number, revision);
                }
                assert_eq!(line.keys().next_back(), Some(&405));
                body(
                    Line {
                        repository,
                        immutable_store,
                        mutable_store,
                        branch: child,
                        line,
                    },
                    parent,
                )
                .await;
            },
        )
        .await;
    }

    /// A gap that straddles the branch point is answered with the parent's
    /// revision at the branch point, in a page that holds none of the
    /// parent's revisions above it, under each acceleration setting.
    #[tokio::test]
    async fn a_gap_straddling_the_branch_point_answers_the_branch_point() {
        for acceleration in SETTINGS {
            with_straddling_child(|line, _parent| async move {
                for number in [141, 199, 200, 250, 300, 399] {
                    let (_, page) = line.page(number, acceleration).await;
                    assert_eq!(
                        line.assert_answers(number, &page),
                        run(140, 101),
                        "{number}"
                    );
                }
                for number in [120, 140] {
                    let (_, page) = line.page(number, acceleration).await;
                    line.assert_answers(number, &page);
                }
                for number in 1..=406 {
                    match line.call(number, acceleration).await {
                        Ok(response) => {
                            line.assert_answers(number, &response.into_inner());
                        }
                        Err(status) => {
                            assert_eq!((number, status.code()), (406, tonic::Code::NotFound));
                        }
                    }
                }
            })
            .await;
        }
    }

    /// A skipped number at or below the branch point is answered from the
    /// parent branch's skip pointer.
    #[tokio::test]
    async fn a_skipped_number_below_the_branch_point_is_answered_from_the_parent() {
        with_stores(
            |repository, context, immutable_store, mutable_store| async move {
                let (parent, revisions) =
                    create_branch_with_jump_history(&context, 150, 399, 5).await;
                let mut line = revisions.clone();
                let (child, child_revisions) = create_child_branch_with_history(
                    &context,
                    vec![BranchPoint {
                        branch: parent,
                        revision: revisions[&405],
                    }],
                    3,
                )
                .await;
                line.extend(child_revisions);
                let line = Line {
                    repository,
                    immutable_store,
                    mutable_store,
                    branch: child,
                    line,
                };
                assert_eq!(line.pointer(300).await, None);
                for number in [200, 250, 300] {
                    let (strategy, page) = line.page(number, LIST_CACHE_OFF).await;
                    assert_eq!(strategy, "history-step", "{number}");
                    assert_eq!(line.assert_answers(number, &page), run(150, 101));
                }
            },
        )
        .await;
    }

    /// The walk from the child's latest revision passes the branch point, and
    /// the page walked on from where it stops crosses a boundary below it.
    /// Both seal each boundary they cross under the child, whose line they
    /// walk, and none under the parent.
    #[tokio::test]
    async fn a_walk_past_the_branch_point_seals_every_boundary_under_the_child() {
        with_straddling_child(|line, parent| async move {
            assert_eq!(line.pointer(200).await, Some(line.line[&140]));
            line.remove_pointers(line.branch, &[200, 300]).await;
            line.remove_pointers(parent, &[100, 200]).await;

            let (strategy, page) = line.page(120, LIST_CACHE_OFF).await;
            assert_eq!(strategy, "full-iteration");
            assert_eq!(line.assert_answers(120, &page), run(120, 101));

            for boundary in [200, 300] {
                assert_eq!(line.pointer(boundary).await, Some(line.line[&140]));
            }
            assert_eq!(line.pointer(100).await, Some(line.line[&100]));
            assert_eq!(line.pointer_of(parent, 100).await, None);
        })
        .await;
    }

    /// A parent force-pushed back to 150 that then grows a new line to 260
    /// keeps its skip pointer at 200 when the child, made from the parent's
    /// old 300, backfills a segment over the parent's old revisions. The
    /// backfill seals 200 under the child, whose line holds them, so the
    /// parent still answers from its own line.
    #[tokio::test]
    async fn a_backfill_on_the_child_seals_no_pointer_of_a_parent_force_pushed_back() {
        with_stores(
            |repository, context, immutable_store, mutable_store| async move {
                let (parent, old) = create_branch_with_history(&context, 300).await;
                let (child, child_revisions) = create_child_branch_with_history(
                    &context,
                    vec![BranchPoint {
                        branch: parent,
                        revision: old[0],
                    }],
                    5,
                )
                .await;
                let old_line: BTreeMap<u64, Hash> = (1..=300)
                    .map(|number| (number, old[300 - number as usize]))
                    .collect();
                let mut parent_line: BTreeMap<u64, Hash> =
                    old_line.range(..=150).map(|(n, r)| (*n, *r)).collect();
                // The merge makes the new line differ from the old one: states with the same
                // parents, number and metadata are the same revision.
                let other = serialize_detached_revision(&context, parent, 1).await;
                let (mut revision, mut number) =
                    push_chained_revision(&context, parent, old_line[&150], other, 0).await;
                parent_line.insert(number, revision);
                while number < 260 {
                    (revision, number) = push_chained_revision(
                        &context,
                        parent,
                        revision,
                        Hash::default(),
                        number + 1,
                    )
                    .await;
                    parent_line.insert(number, revision);
                }
                assert_ne!(parent_line[&200], old_line[&200]);
                let mut child_line = old_line;
                child_line.extend(child_revisions);
                let child = Line {
                    repository,
                    immutable_store: immutable_store.clone(),
                    mutable_store: mutable_store.clone(),
                    branch: child,
                    line: child_line,
                };
                let parent = Line {
                    repository,
                    immutable_store,
                    mutable_store,
                    branch: parent,
                    line: parent_line,
                };
                assert_eq!(parent.pointer(200).await, Some(parent.line[&200]));
                assert_eq!(child.pointer(200).await, None);

                let backfilled = lore_server::cache::revision::try_backfill_segment(
                    &context,
                    child.branch,
                    170,
                    DEFAULT_HISTORY_STEP_SIZE,
                    true,
                )
                .await
                .expect("backfill the child's segment")
                .expect("the child's pointer at 300 closes the segment");
                assert_eq!(
                    backfilled.items().first().map(|item| item.signature),
                    Some(child.line[&200])
                );

                assert_eq!(parent.pointer(200).await, Some(parent.line[&200]));
                let (_, page) = parent.page(199, LIST_CACHE_OFF).await;
                parent.assert_answers(199, &page);
                assert_eq!(child.pointer(200).await, Some(child.line[&200]));
            },
        )
        .await;
    }

    /// A child made from the parent's 305, on a line that jumps from 180 to
    /// 290. The walk down from the child's own pointer at 300 crosses 200
    /// between two of the parent's revisions, and seals it under the child,
    /// whose line it walks.
    #[tokio::test]
    async fn a_walk_down_from_the_child_pointer_seals_under_the_child() {
        with_stores(
            |repository, context, immutable_store, mutable_store| async move {
                let (parent, revisions) =
                    create_branch_with_jump_history(&context, 180, 289, 15).await;
                let mut line = revisions.clone();
                let (child, child_revisions) = create_child_branch_with_history(
                    &context,
                    vec![BranchPoint {
                        branch: parent,
                        revision: revisions[&305],
                    }],
                    3,
                )
                .await;
                line.extend(child_revisions);
                let line = Line {
                    repository,
                    immutable_store,
                    mutable_store,
                    branch: child,
                    line,
                };
                line.remove_pointers(parent, &[200]).await;
                line.set_pointer_of(child, 300, line.line[&300]).await;
                assert_eq!(line.pointer(200).await, None);

                let (strategy, page) = line.page(250, LIST_CACHE_OFF).await;
                assert_eq!(strategy, "history-step");
                assert_eq!(line.assert_answers(250, &page), run(180, 101));
                assert_eq!(line.pointer(200).await, Some(line.line[&180]));
            },
        )
        .await;
    }

    /// A child made from the parent's 120, whose line was reset to the
    /// parent's 350 before its own revisions 351 to 360. Its page for 200
    /// lists the parent's revisions, and with the child's pointer at 300
    /// unsealed, its forward cursor walks down the child's line from the
    /// child's latest revision. The walk crosses 300 between two of the
    /// parent's revisions, and seals it under the child, whose line it walks,
    /// and not under the parent.
    #[tokio::test]
    async fn a_forward_cursor_walk_seals_under_the_branch_whose_line_it_walks() {
        with_stores(
            |repository, context, immutable_store, mutable_store| async move {
                let (parent, signatures) = create_branch_with_history(&context, 400).await;
                let (child, _) = create_child_branch_with_history(
                    &context,
                    vec![BranchPoint {
                        branch: parent,
                        revision: signatures[400 - 120],
                    }],
                    0,
                )
                .await;
                let mut line: BTreeMap<u64, Hash> = (1..=350)
                    .map(|number| (number, signatures[400 - number as usize]))
                    .collect();
                let mut revision = line[&350];
                for number in 351..=360 {
                    let pushed;
                    (revision, pushed) =
                        push_chained_revision(&context, child, revision, Hash::default(), number)
                            .await;
                    line.insert(pushed, revision);
                }
                assert_eq!(line.keys().next_back(), Some(&360));
                let line = Line {
                    repository,
                    immutable_store,
                    mutable_store,
                    branch: child,
                    line,
                };
                line.remove_pointers(child, &[300]).await;
                line.remove_pointers(parent, &[300]).await;
                line.set_pointer_of(child, 200, line.line[&200]).await;

                let (strategy, page) = line.page(200, LIST_CACHE_OFF).await;
                assert_eq!(strategy, "history-step");
                assert_eq!(line.assert_answers(200, &page), run(200, 101));
                assert_eq!(line.pointer(300).await, Some(line.line[&300]));
                assert_eq!(line.pointer_of(parent, 300).await, None);
            },
        )
        .await;
    }
}

/// A lookup by number with no acceleration data walks down the line from the
/// branch's latest revision, and a cancelled request stops that walk.
#[tokio::test]
async fn cancelled_identifier_lookup_answers_cancelled() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
            allow_all_repositories(),
        ));
        let (branch_id, _) = create_branch_with_history(&repository_context, 3).await;

        let cancellation = lore_revision::runtime::Cancellation::new();
        cancellation.cancel();
        let status = lore_server::util::core_hop::with_request_cancellation(
            cancellation,
            handler(
                make_request_identifier(repository, branch_id, 2),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration {
                    step_keys: false,
                    list_cache: false,
                },
                &make_instruments(),
            ),
        )
        .await
        .expect_err("a cancelled lookup returns no page");
        assert_eq!(status.code(), tonic::Code::Cancelled, "{status:?}");
    }))
    .await;
}

/// An execution whose request has been cancelled.
fn cancelled_execution() -> Arc<lore_revision::interface::ExecutionContext> {
    let cancellation = lore_revision::runtime::Cancellation::new();
    cancellation.cancel();
    Arc::new(
        lore_revision::interface::ExecutionContext::new_server(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            String::default(),
        )
        .with_cancellation(cancellation),
    )
}

mod accelerated_start {
    use lore_server::grpc::revision::v1::branch_lineage::HistoryOwner;
    use lore_server::grpc::server::RevisionListAcceleration;

    use super::*;

    #[tokio::test]
    async fn a_cancelled_descent_from_a_step_pointer_answers_cancelled() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                repository,
                allow_all_repositories(),
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            // The pointer at boundary 100 names revision 100, above 50, so the
            // line is walked down from it.
            let Err(status) = LORE_CONTEXT
                .scope(
                    cancelled_execution(),
                    accelerated_start(
                        &repository_context,
                        &HistoryOwner::requested(branch_id),
                        50,
                        DEFAULT_HISTORY_STEP_SIZE,
                        RevisionListAcceleration {
                            step_keys: true,
                            list_cache: false,
                        },
                    ),
                )
                .await
            else {
                panic!("a cancelled descent is not a miss");
            };
            assert_eq!(status.code(), tonic::Code::Cancelled, "{status:?}");
        }))
        .await;
    }

    #[tokio::test]
    async fn a_cancelled_request_skips_the_backfill_and_answers_cancelled() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store.clone(),
                repository,
                allow_all_repositories(),
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            // Storing zero deletes the cached segment, leaving a backfill to rebuild it.
            let (key, key_type) = branch::revision_list_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict cache entry");

            let Err(status) = LORE_CONTEXT
                .scope(
                    cancelled_execution(),
                    accelerated_start(
                        &repository_context,
                        &HistoryOwner::requested(branch_id),
                        50,
                        DEFAULT_HISTORY_STEP_SIZE,
                        RevisionListAcceleration {
                            step_keys: false,
                            list_cache: true,
                        },
                    ),
                )
                .await
            else {
                panic!("a cancelled backfill is not a miss");
            };
            assert_eq!(status.code(), tonic::Code::Cancelled, "{status:?}");
        }))
        .await;
    }
}

mod forward_descent {
    use super::*;

    #[tokio::test]
    async fn a_cancelled_descent_answers_cancelled() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                repository,
                allow_all_repositories(),
            ));
            let (branch_id, signatures) = create_branch_with_history(&repository_context, 3).await;

            let status = LORE_CONTEXT
                .scope(
                    cancelled_execution(),
                    forward_descent(
                        &repository_context,
                        branch_id,
                        signatures[0],
                        1,
                        DEFAULT_HISTORY_STEP_SIZE,
                        false,
                    ),
                )
                .await
                .expect_err("a cancelled descent finds no successor");
            assert_eq!(status.code(), tonic::Code::Cancelled, "{status:?}");
        }))
        .await;
    }
}
