// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1::BranchPushRequest;
use lore_proto::lore::revision::v1::BranchPushResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::REVISION;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::Level;
use tracing::debug;
use tracing::info;
use tracing::span;

use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::handlers::branch_push::PushResult;
use crate::grpc::handlers::branch_push::dispatch_response_message;
use crate::grpc::handlers::branch_push::extract_client_ip;
use crate::grpc::handlers::branch_push::push;
use crate::grpc::hook_error_to_status;
use crate::grpc::require_write_permission;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

/// `lore.revision.v1.RevisionService.BranchPush` handler.
///
/// Soft rejection (non-fast-forward without `force`/`fast_forward_merge`,
/// or fast-forward merge with conflicts) is conveyed via
/// `FailedPrecondition` with a detail message that distinguishes the
/// two cases and embeds the current branch latest. Pushing to a branch
/// id whose metadata is missing returns `NotFound`. Pushing to a
/// deleted branch reinstates the name → id mapping if the name is
/// still free, or returns `AlreadyExists` if claimed by a different
/// live branch.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "BranchPush::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchPushRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchPushResponse>, Status> {
    let user_info = get_authorization(request.extensions());
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let repository_id = get_repository(request.metadata())?;
    require_write_permission(request.extensions(), repository_id)?;

    // Service accounts bypass branch-protection (mirroring path).
    let mut bypass_protection = false;
    if let Ok(user_info) = user_info
        && user_info.is_service_account.unwrap_or_default()
    {
        bypass_protection = true;
    }

    let client_ip: Option<String> = extract_client_ip(&request).map(|ip| ip.to_string());
    let req = request.into_inner();
    let branch_id = BranchId::from(req.id);
    let revision = Hash::from(req.revision_signature);
    let force = req.force;
    let fast_forward_merge = req.fast_forward_merge;

    if revision.is_zero() {
        info!("Invalid branch push request, revision_signature is zero");
        return Err(Status::invalid_argument(
            "revision_signature must be non-zero",
        ));
    }

    debug!(
        {REVISION} = %revision,
        bypass_protection,
        {BRANCH_ID} = %branch_id,
        force,
        fast_forward_merge,
        "Handling branch push request",
    );

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));
    let repository_id: RepositoryId = repository.id;
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(execution, async move {
            let mut ctx_builder = HookContext::builder()
                .correlation_id(correlation_id.clone())
                .hook_point(HookPoint::BranchPush)
                .repository(repository_id)
                .user(user_id.clone())
                .branch(branch_id)
                .revision(revision);

            if let Some(ip) = client_ip {
                ctx_builder = ctx_builder.metadata("client_ip", ip);
            }

            let mut hook_ctx = ctx_builder.build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchPush, &hook_ctx)
                .map_err(hook_error_to_status)?;

            ensure_branch_pushable(repository.clone(), branch_id).await?;

            let PushResult {
                success,
                fast_forward_merged,
                revision: resulting_revision,
                revision_number,
            } = push(
                repository.clone(),
                branch_id,
                revision,
                bypass_protection,
                force,
                fast_forward_merge,
                history_step_size,
                acceleration,
            )
            .await?;

            instrument_provider
                .counter("num_branches_pushed")
                .add(1, &[]);

            if !success {
                let detail = if fast_forward_merge {
                    format!("Fast-forward merge has conflicts; branch latest: {resulting_revision}")
                } else {
                    format!(
                        "Branch push is not a fast-forward; branch latest: {resulting_revision}"
                    )
                };
                debug!(
                    {BRANCH_ID} = %branch_id,
                    branch_latest = %resulting_revision,
                    fast_forward_merge,
                    "Branch push rejected",
                );
                return Err(Status::failed_precondition(detail));
            }

            lore_spawn!({
                let user_id = user_id.clone();
                async move {
                    notification
                        .branch_pushed(
                            repository_id,
                            branch_id,
                            &user_id,
                            resulting_revision,
                            revision_number,
                        )
                        .instrument(span!(Level::DEBUG, "publish_notification"))
                        .await;
                }
                .in_current_span()
            });

            hook_ctx.set_revision_number(revision_number);
            hook_dispatcher.spawn_post(HookPoint::BranchPush, hook_ctx);

            let message = dispatch_response_message(
                hook_dispatcher,
                &correlation_id,
                &user_id,
                repository_id,
                branch_id,
                resulting_revision,
                repository.clone(),
            )
            .await;

            debug!(
                {BRANCH_ID} = %branch_id,
                {REVISION} = %resulting_revision,
                revision_number,
                fast_forward_merged,
                "Branch push response",
            );

            Ok(Response::new(BranchPushResponse {
                revision_signature: resulting_revision.into(),
                revision_number,
                fast_forward_merged,
                message,
            }))
        })
        .await
}

/// Returns `NotFound` for branch ids without metadata, and reinstates
/// the name → id mapping for deleted branches whose name is still
/// free. If the name has been claimed by a different live branch,
/// returns `AlreadyExists`.
async fn ensure_branch_pushable(
    repository: Arc<RepositoryContext>,
    branch_id: BranchId,
) -> Result<(), Status> {
    let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
        .await
        .map_err(|_err| Status::not_found(format!("Branch {branch_id} not found")))?;
    let metadata = branch::load_metadata(repository.clone(), metadata_hash)
        .await
        .warn_map_err(|err| Status::internal(err.to_string()))?;

    let Ok(name) = branch::name(&metadata) else {
        return Ok(());
    };
    if name.is_empty() {
        return Ok(());
    }

    match branch::load_name_to_id_local(repository.clone(), name).await {
        Ok(mapped) if BranchId::from(mapped) == branch_id => Ok(()),
        Ok(other) => {
            let other_id = BranchId::from(other);
            if other_branch_still_claims_name(&repository, other_id, name).await {
                info!(
                    {BRANCH_ID} = %branch_id,
                    %name,
                    claimed_by = %other_id,
                    "Cannot reinstate deleted branch: name claimed by another branch",
                );
                Err(Status::already_exists(format!(
                    "Branch name '{name}' is in use by a different branch"
                )))
            } else {
                debug!(
                    {BRANCH_ID} = %branch_id,
                    %name,
                    stale = %other_id,
                    "Stale name → id mapping, reinstating to current branch",
                );
                branch::store_name_to_id(repository, branch_id, name)
                    .await
                    .warn_map_err(|err| {
                        Status::internal(format!("Failed to reinstate name → id mapping: {err}"))
                    })
            }
        }
        Err(_) => {
            // No mapping (deleted — the underlying store treats zero
            // values as missing — or never written). Reinstate the
            // mapping so the subsequent push sees a live branch.
            debug!({BRANCH_ID} = %branch_id, %name, "Reinstating name → id mapping for push");
            branch::store_name_to_id(repository, branch_id, name)
                .await
                .warn_map_err(|err| {
                    Status::internal(format!("Failed to reinstate name → id mapping: {err}"))
                })
        }
    }
}

/// True iff `other_id` exists and its metadata still names it `name`. A
/// mismatch (dead branch, missing metadata, or a rename that left an
/// orphan name pointer) is treated as a stale mapping the caller can
/// safely overwrite.
async fn other_branch_still_claims_name(
    repository: &Arc<RepositoryContext>,
    other_id: BranchId,
    name: &str,
) -> bool {
    let Ok(metadata_hash) = branch::metadata_hash(repository.clone(), other_id).await else {
        return false;
    };
    let Ok(metadata) = branch::load_metadata(repository.clone(), metadata_hash).await else {
        return false;
    };
    branch::name(&metadata).unwrap_or("") == name
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::hooks::HookDispatcher;
    use crate::notification::testing::MockNotificationSender;
    use crate::store::test_store_create;

    struct TestInstrumentProvider {}

    impl InstrumentProvider for TestInstrumentProvider {
        fn namespace(&self) -> &'static str {
            "test"
        }
        fn labels(&self) -> &[KeyValue] {
            &[]
        }
    }

    async fn create_root_branch(
        repository_context: &Arc<RepositoryContext>,
        name: &str,
    ) -> BranchId {
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

    fn make_service_account_request(
        repository: RepositoryId,
        branch: BranchId,
        revision: Hash,
    ) -> Request<BranchPushRequest> {
        let mut request = make_request(repository, branch, revision, false, false);
        request
            .extensions_mut()
            .insert(crate::auth::jwt::AuthorizationToken {
                user_id: "service-bot".into(),
                is_service_account: Some(true),
                resources: Some(vec![crate::auth::jwt::ResourcePermission {
                    permission: vec![crate::auth::jwt::PERMISSION_WRITE.to_string()],
                    resource_id: format!("urc-{repository}"),
                }]),
                ..crate::auth::jwt::AuthorizationToken::default()
            });
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("first push should succeed");

            // Build a revision whose parent_self is still Hash::default() —
            // doesn't descend from the branch's current latest.
            let stale = build_revision(&repository_context, Hash::default(), 2).await;
            let err = handler(
                make_request(repository, main, stale, false, false),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("first push should succeed");

            let stale = build_revision(&repository_context, Hash::default(), 2).await;
            let response = handler(
                make_request(repository, main, stale, true, false),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("first push should succeed");

            // Re-pushing the same revision returns success with the same latest.
            let response = handler(
                make_request(repository, main, revision, false, false),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect_err("unknown branch should fail");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn service_account_bypasses_protection() {
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
            branch::protect(repository_context.clone(), main)
                .await
                .expect("should protect");
            let revision = build_revision(&repository_context, Hash::default(), 1).await;

            let hook_dispatcher = HookDispatcher::empty();
            let response = handler(
                make_service_account_request(repository, main, revision),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("service account should bypass protection");
            assert_eq!(
                response.into_inner().revision_signature,
                bytes::Bytes::from(revision)
            );
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("seed push should succeed");

            // Create a child of main, then delete it. Pushing to the
            // deleted id should reinstate the name → id mapping.
            let child =
                create_child_branch(&repository_context, "feature", main, main_latest).await;
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &instrument_provider,
            )
            .await
            .expect("seed push should succeed");

            let original =
                create_child_branch(&repository_context, "feature", main, main_latest).await;
            branch::delete(repository_context.clone(), original)
                .await
                .expect("delete should succeed");

            // Recycle the name with a new branch id.
            create_child_branch(&repository_context, "feature", main, main_latest).await;

            let stale_revision = build_revision(&repository_context, main_latest, 2).await;
            let err = handler(
                make_request(repository, original, stale_revision, false, false),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
                    immutable_store.clone(),
                    mutable_store.clone(),
                    notification_sender.clone(),
                    &hook_dispatcher,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
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
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
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
}
