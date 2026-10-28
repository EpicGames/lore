// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1::BranchPushRequest;
use lore_revision::branch;
use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::grpc::get_write_token;
use lore_server::grpc::revision::v1::branch_push::*;
use lore_server::hooks::HookDispatcher;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use opentelemetry::KeyValue;
use rand::random;
use tonic::Request;

use super::hook_recorder::Seen;
use super::hook_recorder::recording_dispatcher;
use crate::grpc::push_protected::PUSH_PROTECTED;
use crate::grpc::push_protected::TIERS;
use crate::grpc::push_protected::Tier;
use crate::grpc::push_protected::allow_all;
use crate::notification::testing::MockNotificationSender;
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

async fn create_root_branch(repository_context: &Arc<RepositoryContext>, name: &str) -> BranchId {
    let write_token = get_write_token();
    lore_revision::branch::create(
        repository_context.clone(),
        &write_token,
        BranchId::from(uuid::Uuid::now_v7()),
        name,
        branch::default_category(),
        "test-creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("Could not create root branch")
}

/// Builds a state revision rooted at `parent_self` with `revision_number`,
/// serializes it, and returns the new revision hash.
async fn build_revision(
    repository_context: &Arc<RepositoryContext>,
    parent_self: Hash,
    revision_number: u64,
) -> Hash {
    build_state_revision(
        repository_context,
        parent_self,
        Hash::default(),
        revision_number,
    )
    .await
}

/// Like `build_revision` but lets the caller set `parent_other`, which
/// distinguishes the resulting state hash from a sibling revision
/// with the same `parent_self` / `revision_number`.
async fn build_state_revision(
    repository_context: &Arc<RepositoryContext>,
    parent_self: Hash,
    parent_other: Hash,
    revision_number: u64,
) -> Hash {
    let write_token = get_write_token();
    let state = state::State::new();
    state.set_parent_self(parent_self);
    if !parent_other.is_zero() {
        state.set_parent_other(parent_other);
    }
    state.set_revision_number(revision_number);
    state
        .serialize(repository_context.clone(), &write_token)
        .await
        .expect("Failed to serialize state")
}

fn make_request(
    repository: RepositoryId,
    branch: BranchId,
    revision: Hash,
    force: bool,
    fast_forward_merge: bool,
) -> Request<BranchPushRequest> {
    let mut request = Request::new(BranchPushRequest {
        id: branch.into(),
        revision_signature: revision.into(),
        force,
        fast_forward_merge,
    });
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    request
}

#[tokio::test]
async fn push_advances_branch_latest() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .return_once(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let revision = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        let response = handler(
            make_request(repository, main, revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("Request failed");

        let inner = response.into_inner();
        assert_eq!(inner.revision_signature, bytes::Bytes::from(revision));
        assert_eq!(inner.revision_number, 1);
        assert!(!inner.fast_forward_merged);
    }))
    .await;
}

#[tokio::test]
async fn push_zero_revision_returns_invalid_argument() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let notification_sender = Arc::new(MockNotificationSender::new());
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;

        let hook_dispatcher = HookDispatcher::empty();
        let err = handler(
            make_request(repository, main, Hash::default(), false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect_err("zero revision should fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }))
    .await;
}

#[tokio::test]
async fn push_with_stale_parent_returns_failed_precondition() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    // First push fires; second is rejected before notification.
    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .return_once(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let first = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        handler(
            make_request(repository, main, first, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("first push should succeed");

        // Build a revision whose parent_self is still Hash::default() —
        // doesn't descend from the branch's current latest.
        let stale = build_revision(&repository_context, Hash::default(), 2).await;
        let err = handler(
            make_request(repository, main, stale, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect_err("stale parent push should fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }))
    .await;
}

#[tokio::test]
async fn force_push_overrides_stale_parent() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .times(2)
        .returning(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let first = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        handler(
            make_request(repository, main, first, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("first push should succeed");

        let stale = build_revision(&repository_context, Hash::default(), 2).await;
        let response = handler(
            make_request(repository, main, stale, true, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("force push should succeed");
        let inner = response.into_inner();
        // push() may rewrite the revision number, so the returned
        // latest can differ from the supplied hash; verify the push
        // landed by reading the new branch latest.
        assert!(!inner.revision_signature.is_empty());
        assert!(!inner.fast_forward_merged);
        let new_latest = branch::load_latest(repository_context.clone(), main)
            .await
            .expect("load_latest after force push");
        assert_eq!(inner.revision_signature, bytes::Bytes::from(new_latest));
    }))
    .await;
}

#[tokio::test]
async fn push_to_protected_branch_returns_permission_denied() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let notification_sender = Arc::new(MockNotificationSender::new());
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        // Protect the branch — non-service-account pushes must be denied.
        branch::protect(repository_context.clone(), main)
            .await
            .expect("should protect");

        let revision = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        let err = handler(
            make_request(repository, main, revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect_err("protected push should fail");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }))
    .await;
}

#[tokio::test]
async fn push_idempotent_on_current_latest() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    // Notification fires once on the actual push; the no-op repush
    // hits the early-return path inside `push()` (branch latest == incoming)
    // which still reports success but doesn't re-publish the
    // notification — see push() body.
    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .times(2)
        .returning(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let revision = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        handler(
            make_request(repository, main, revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("first push should succeed");

        // Re-pushing the same revision returns success with the same latest.
        let response = handler(
            make_request(repository, main, revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("idempotent re-push should succeed");
        let inner = response.into_inner();
        assert_eq!(inner.revision_signature, bytes::Bytes::from(revision));
        assert!(!inner.fast_forward_merged);
    }))
    .await;
}

#[tokio::test]
async fn unknown_branch_returns_not_found() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let notification_sender = Arc::new(MockNotificationSender::new());
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let unknown = BranchId::from(uuid::Uuid::now_v7());
        let revision = Hash::from([1u8; 32].as_slice());

        let hook_dispatcher = HookDispatcher::empty();
        let err = handler(
            make_request(repository, unknown, revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect_err("unknown branch should fail");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }))
    .await;
}

/// Pushes to a protected branch as a caller holding `actions`, on `tier`.
async fn push_to_protected_branch(
    tier: Tier,
    actions: &'static [&'static str],
) -> Result<(), tonic::Status> {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .returning(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        branch::protect(repository_context.clone(), main)
            .await
            .expect("should protect");
        let revision = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        handler(
            tier.with_token(
                make_request(repository, main, revision, false, false),
                repository,
                actions,
            ),
            tier.authorizer(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .map(|_| ())
    }))
    .await
}

#[tokio::test]
async fn push_protected_action_bypasses_protection() {
    for tier in TIERS {
        push_to_protected_branch(tier, &[PUSH_PROTECTED])
            .await
            .unwrap_or_else(|err| panic!("{tier:?}: holder of the action should push: {err}"));
    }
}

#[tokio::test]
async fn push_to_protected_branch_without_the_action_is_denied() {
    for tier in TIERS {
        let err = push_to_protected_branch(tier, &["read", "write"])
            .await
            .expect_err("a caller without the action must not push");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{tier:?}");
    }
}

#[tokio::test]
async fn service_account_claim_no_longer_bypasses_protection() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let notification_sender = Arc::new(MockNotificationSender::new());
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        branch::protect(repository_context.clone(), main)
            .await
            .expect("should protect");
        let revision = build_revision(&repository_context, Hash::default(), 1).await;

        let mut request = Tier::Global.with_token(
            make_request(repository, main, revision, false, false),
            repository,
            &[],
        );
        request
            .extensions_mut()
            .insert(lore_server::auth::jwt::AuthorizationToken {
                user_id: "service-bot".into(),
                is_service_account: Some(true),
                ..Default::default()
            });

        let hook_dispatcher = HookDispatcher::empty();
        let err = handler(
            request,
            Tier::Global.authorizer(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect_err("the claim alone must not waive protection");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }))
    .await;
}

#[tokio::test]
async fn push_to_deleted_branch_reinstates_name() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .times(2)
        .returning(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let main_latest = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        handler(
            make_request(repository, main, main_latest, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("seed push should succeed");

        // Create a child of main, then delete it. Pushing to the
        // deleted id should reinstate the name → id mapping.
        let child = create_child_branch(&repository_context, "feature", main, main_latest).await;
        branch::delete(repository_context.clone(), child)
            .await
            .expect("delete should succeed");

        // Confirm deleted: name lookup fails (the mutable store
        // treats zero-valued entries as missing).
        assert!(
            branch::load_name_to_id_local(repository_context.clone(), "feature")
                .await
                .is_err(),
            "feature name should be deleted",
        );

        let child_revision = build_revision(&repository_context, main_latest, 2).await;
        let response = handler(
            make_request(repository, child, child_revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("push to deleted branch should reinstate and succeed");
        assert!(!response.into_inner().revision_signature.is_empty());

        // Name now points back to the original branch id.
        let restored = branch::load_name_to_id_local(repository_context.clone(), "feature")
            .await
            .expect("name lookup after reinstate");
        assert_eq!(BranchId::from(restored), child);
    }))
    .await;
}

#[tokio::test]
async fn push_to_deleted_branch_fails_when_name_taken() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .return_once(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let main_latest = build_revision(&repository_context, Hash::default(), 1).await;

        let hook_dispatcher = HookDispatcher::empty();
        handler(
            make_request(repository, main, main_latest, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("seed push should succeed");

        let original = create_child_branch(&repository_context, "feature", main, main_latest).await;
        branch::delete(repository_context.clone(), original)
            .await
            .expect("delete should succeed");

        // Recycle the name with a new branch id.
        create_child_branch(&repository_context, "feature", main, main_latest).await;

        let stale_revision = build_revision(&repository_context, main_latest, 2).await;
        let err = handler(
            make_request(repository, original, stale_revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect_err("push to deleted branch with claimed name should fail");
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
    }))
    .await;
}

#[tokio::test]
async fn fast_forward_merge_succeeds_for_clean_diff() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .times(3)
        .returning(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;

        // Push r1 then r2 to advance main's latest.
        let r1 = build_revision(&repository_context, Hash::default(), 1).await;
        let r2 = build_revision(&repository_context, r1, 2).await;

        let hook_dispatcher = HookDispatcher::empty();
        for rev in [r1, r2] {
            handler(
                make_request(repository, main, rev, false, false),
                allow_all(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("seed push should succeed");
        }

        // Build a divergent revision rooted at r1 (parent_self=r1)
        // — main is now at r2, so parent_self != current latest and
        // the server must fast-forward merge. Set parent_other to
        // r1 to differentiate the state hash from r2 (which has the
        // same parent_self / revision_number).
        let divergent = build_state_revision(&repository_context, r1, r1, 2).await;

        let response = handler(
            make_request(repository, main, divergent, false, true),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("fast-forward merge with clean diff should succeed");
        let inner = response.into_inner();
        assert!(inner.fast_forward_merged);
        // Resulting latest is the new server-created merge revision,
        // not the supplied divergent revision.
        assert_ne!(inner.revision_signature, bytes::Bytes::from(divergent));
        assert!(!inner.revision_signature.is_empty());
    }))
    .await;
}

/// Helper for fast-forward-merge tests: creates a child branch with
/// `parent` at `parent_revision` in its stack so the parent
/// validator inside `branch::create` accepts it.
async fn create_child_branch(
    repository_context: &Arc<RepositoryContext>,
    name: &str,
    parent: BranchId,
    parent_revision: Hash,
) -> BranchId {
    let write_token = get_write_token();
    lore_revision::branch::create(
        repository_context.clone(),
        &write_token,
        BranchId::from(uuid::Uuid::now_v7()),
        name,
        branch::personal_category(),
        "test-creator",
        1,
        vec![lore_base::types::BranchPoint {
            branch: parent,
            revision: parent_revision,
        }],
        false,
        false,
    )
    .await
    .expect("Could not create child branch")
}

#[tokio::test]
async fn pre_hook_sees_branch_name_and_service_account() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .return_once(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let revision = build_revision(&repository_context, Hash::default(), 1).await;

        let mut request = make_request(repository, main, revision, false, false);
        request.extensions_mut().insert(AuthorizationToken {
            user_id: "ci-bot".into(),
            is_service_account: Some(true),
            ..AuthorizationToken::default()
        });
        let (hook_dispatcher, seen) = recording_dispatcher(false);
        handler(
            request,
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("push should succeed");

        assert_eq!(
            *seen.lock(),
            vec![Seen {
                branch_name: Some("main".into()),
                is_service_account: Some("true".into()),
            }]
        );
    }))
    .await;
}

#[tokio::test]
async fn rejected_push_to_deleted_branch_does_not_reinstate_name() {
    let repository = random::<RepositoryId>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    let mut notification_sender = MockNotificationSender::new();
    notification_sender
        .expect_branch_pushed()
        .times(1)
        .returning(|_, _, _, _, _| ());
    let notification_sender = Arc::new(notification_sender);
    let instrument_provider = TestInstrumentProvider {};

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository_context = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository,
        ));
        let main = create_root_branch(&repository_context, "main").await;
        let main_latest = build_revision(&repository_context, Hash::default(), 1).await;
        handler(
            make_request(repository, main, main_latest, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &HookDispatcher::empty(),
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect("seed push should succeed");

        let child = create_child_branch(&repository_context, "feature", main, main_latest).await;
        branch::delete(repository_context.clone(), child)
            .await
            .expect("delete should succeed");

        let (hook_dispatcher, seen) = recording_dispatcher(true);
        let child_revision = build_revision(&repository_context, main_latest, 2).await;
        let err = handler(
            make_request(repository, child, child_revision, false, false),
            allow_all(),
            immutable_store.clone(),
            mutable_store.clone(),
            notification_sender.clone(),
            &hook_dispatcher,
            DEFAULT_HISTORY_STEP_SIZE,
            lore_server::grpc::server::RevisionListAcceleration::default(),
            &instrument_provider,
        )
        .await
        .expect_err("a rejected push should fail");

        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert_eq!(seen.lock()[0].branch_name.as_deref(), Some("feature"));
        assert!(
            branch::load_name_to_id_local(repository_context.clone(), "feature")
                .await
                .is_err(),
            "a rejected push must leave the deleted name unmapped",
        );
    }))
    .await;
}

mod handler {
    use std::collections::HashSet;

    use async_trait::async_trait;
    use lore_revision::repository::RepositoryMetadata;
    use lore_server::hooks::HookContext;
    use lore_server::hooks::HookError;
    use lore_server::hooks::HookPoint;
    use lore_server::hooks::traits::Hook;
    use lore_server::hooks::traits::HookResponse;

    use super::*;
    use crate::store::test_support::RecordingLoadStore;

    /// Answers a push with the response context keys it was given, joined by `|`.
    struct EchoResponseContext;

    #[async_trait]
    impl Hook for EchoResponseContext {
        fn name(&self) -> &'static str {
            "echo_response_context"
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            &[HookPoint::BranchPush]
        }

        fn response_handler(&self, ctx: &HookContext) -> Result<HookResponse, HookError> {
            let keys = [
                "branch_name",
                "repository_name",
                "default_branch_name",
                "is_default_branch",
            ];
            let message = keys
                .map(|key| ctx.get_metadata(key).unwrap_or("<absent>"))
                .join("|");
            Ok(HookResponse::with_message(message))
        }
    }

    fn echo_dispatcher() -> HookDispatcher {
        HookDispatcher::from_hooks_default(vec![(
            "echo_response_context".to_string(),
            Box::new(EchoResponseContext),
        )])
    }

    async fn store_repository_metadata(
        repository_context: &Arc<RepositoryContext>,
        default_branch: BranchId,
    ) {
        let hash = lore_revision::repository::metadata_store(
            repository_context.clone(),
            RepositoryMetadata {
                name: "game".to_string(),
                description: String::new(),
                default_branch,
                default_branch_name: "main".to_string(),
                creator: "test-creator".to_string(),
                created: 1,
            },
        )
        .await
        .expect("store repository metadata");
        lore_revision::repository::metadata_store_hash(repository_context.clone(), hash)
            .await
            .expect("store repository metadata hash");
    }

    #[tokio::test]
    async fn the_response_hooks_read_the_branch_and_repository_names() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_pushed()
            .times(2)
            .returning(|_, _, _, _, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_root_branch(&repository_context, "main").await;
            store_repository_metadata(&repository_context, main).await;
            let main_latest = build_revision(&repository_context, Hash::default(), 1).await;
            let feature =
                create_child_branch(&repository_context, "feature", main, main_latest).await;
            let feature_revision = build_revision(&repository_context, main_latest, 2).await;

            let hook_dispatcher = echo_dispatcher();
            let mut messages = Vec::new();
            for (branch, revision) in [(main, main_latest), (feature, feature_revision)] {
                let response = handler(
                    make_request(repository, branch, revision, false, false),
                    allow_all(),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    notification_sender.clone(),
                    &hook_dispatcher,
                    DEFAULT_HISTORY_STEP_SIZE,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                    &instrument_provider,
                )
                .await
                .expect("push should succeed");
                messages.push(response.into_inner().message);
            }

            assert_eq!(
                messages,
                [
                    Some("main|game|main|true".to_string()),
                    Some("feature|game|main|false".to_string()),
                ]
            );
        }))
        .await;
    }

    /// A push reads each mutable-store key once, including the reads its response hooks need,
    /// since every read is a store round trip. The first push to a branch has no latest
    /// revision to read, and the next one does.
    #[tokio::test]
    async fn a_push_reads_each_mutable_store_key_once() {
        let repository = random::<RepositoryId>();
        let (immutable_store, inner_mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let recording = RecordingLoadStore::new(inner_mutable_store);
        let mutable_store: Arc<dyn lore_storage::MutableStore> = recording.clone();

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_pushed()
            .times(2)
            .returning(|_, _, _, _, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_root_branch(&repository_context, "main").await;
            store_repository_metadata(&repository_context, main).await;
            let first = build_revision(&repository_context, Hash::default(), 1).await;
            let second = build_revision(&repository_context, first, 2).await;

            let hook_dispatcher = echo_dispatcher();
            let push = |revision| {
                handler(
                    make_request(repository, main, revision, false, false),
                    allow_all(),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    notification_sender.clone(),
                    &hook_dispatcher,
                    DEFAULT_HISTORY_STEP_SIZE,
                    lore_server::grpc::server::RevisionListAcceleration::default(),
                    &instrument_provider,
                )
            };
            for (revision, which) in [(first, "first"), (second, "second")] {
                recording.take_loaded();
                push(revision)
                    .await
                    .unwrap_or_else(|err| panic!("{which} push should succeed: {err}"));
                let loaded = recording.take_loaded();

                let distinct: HashSet<_> = loaded.iter().collect();
                assert_eq!(
                    distinct.len(),
                    loaded.len(),
                    "{which} push loaded keys: {loaded:?}"
                );
            }
        }))
        .await;
    }

    /// Protection is checked before the name of a deleted branch is mapped back, so a refused
    /// push leaves the name unmapped.
    #[tokio::test]
    async fn a_push_refused_as_protected_leaves_a_deleted_name_unmapped() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_pushed()
            .return_once(|_, _, _, _, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let main = create_root_branch(&repository_context, "main").await;
            let main_latest = build_revision(&repository_context, Hash::default(), 1).await;

            let hook_dispatcher = HookDispatcher::empty();
            handler(
                make_request(repository, main, main_latest, false, false),
                allow_all(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("seed push should succeed");

            let feature =
                create_child_branch(&repository_context, "feature", main, main_latest).await;
            branch::delete(repository_context.clone(), feature)
                .await
                .expect("delete should succeed");
            branch::protect(repository_context.clone(), feature)
                .await
                .expect("should protect");

            let feature_revision = build_revision(&repository_context, main_latest, 2).await;
            let err = handler(
                make_request(repository, feature, feature_revision, false, false),
                allow_all(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                lore_server::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect_err("a protected branch must not be pushed");
            assert_eq!(err.code(), tonic::Code::PermissionDenied);

            let err = branch::load_name_to_id_local(repository_context.clone(), "feature")
                .await
                .expect_err("the name must stay unmapped");
            assert!(err.is_branch_not_found(), "{err:?}");
        }))
        .await;
    }
}
