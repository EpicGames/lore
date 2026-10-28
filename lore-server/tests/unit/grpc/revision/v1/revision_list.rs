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
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use lore_server::grpc::revision::v1::revision_list::*;
use lore_server::grpc::revision::v1::service::RevisionListInstruments;
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
            branch_id,
            serialized,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
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
    let write_token = get_write_token();
    let mut metadata = Metadata::new();
    metadata.set_branch(branch_id).expect("set branch");
    let metadata_hash = metadata
        .serialize(repository.clone())
        .await
        .expect("serialize metadata");

    let state = State::new();
    state.set_revision_number(revision_number);
    state.set_metadata_hash(metadata_hash);
    state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize detached state")
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
    let serialized = state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize state");

    let result = branch_push::push(
        repository.clone(),
        branch_id,
        serialized,
        crate::util::test_support::PUSH_ANY,
        true,
        false,
        DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
    )
    .await
    .expect("push revision");
    (result.revision, result.revision_number)
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

/// A revision number a merge skipped does not exist. A sealed boundary
/// whose anchor is numbered below the request proves that absence.
#[tokio::test]
async fn revision_number_skipped_by_a_jump_is_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let (branch_id, revisions) =
            create_branch_with_jump_history(&repository_context, 150, 399, 5).await;
        assert!(!revisions.contains_key(&250));

        let err = handler(
            make_request_identifier(repository, branch_id, 250),
            immutable_store,
            mutable_store,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &make_instruments(),
        )
        .await
        .expect_err("skipped revision number should not resolve");
        assert_eq!(err.code(), tonic::Code::NotFound);
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
            make_request_identifier(repository, branch_id, 50),
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
            make_request_identifier(repository, branch_id, 50),
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

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), lore_storage::StoreError> {
        self.inner.clone().flush(sync_data).await
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
