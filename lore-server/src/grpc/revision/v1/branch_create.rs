// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_proto::lore::revision::v1::BranchCreateRequest;
use lore_proto::lore::revision::v1::BranchCreateResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use super::branch_record::build_branch;
use crate::grpc::ServerResultExt;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

/// Reject oversized string fields early to prevent resource exhaustion.
fn validate_create_input(name: &str, category: &str, creator: &str) -> Result<(), Status> {
    if name.len() > branch::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Branch name exceeds maximum length of {} bytes",
            branch::MAX_NAME_LEN,
        )));
    }
    if category.len() > branch::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Branch category exceeds maximum length of {} bytes",
            branch::MAX_NAME_LEN,
        )));
    }
    if creator.len() > repository::MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "Creator exceeds maximum length of {} bytes",
            repository::MAX_NAME_LEN,
        )));
    }
    Ok(())
}

/// `lore.revision.v1.RevisionService.BranchCreate` handler.
///
/// The caller pre-generates `Branch.id` (retry idempotency). The
/// server assigns `created` and the response's full `Branch` record.
/// `creator` is hybrid: caller-set if permitted, otherwise the
/// authenticated JWT identity.
#[tracing::instrument(name = "BranchCreate::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchCreateRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    let caller_context = CallerContext::from_original_request(&request)?;
    let req = request.into_inner();
    branch_create_implementation(
        req,
        caller_context,
        immutable_store,
        mutable_store,
        notification_sender,
        hook_dispatcher,
        instrument_provider,
    )
    .await
}

/// This `BranchCreateRequest` should be fulfilled by this server.
pub async fn branch_create_implementation(
    req: BranchCreateRequest,
    context: CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    let name = req.name;
    let category = req.category;
    let creator = req.creator.unwrap_or_else(|| context.user_id.clone());
    let stack: Vec<BranchPoint> = req.stack.into_iter().map(BranchPoint::from).collect();

    let branch = BranchId::from(req.id);

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();

    let execution = setup_execution(
        module_path!(),
        context.correlation_id.clone(),
        context.user_id.clone(),
    );
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        context.repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let hook_ctx = HookContext::builder()
                .correlation_id(&context.correlation_id)
                .hook_point(HookPoint::BranchCreate)
                .repository(context.repository_id)
                .user(&context.user_id)
                .branch(branch)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchCreate, &hook_ctx)
                .map_err(hook_error_to_status)?;

            validate_create_input(&name, &category, &creator)?;

            debug!({BRANCH_ID} = %branch, branch_name = %name, stack = ?stack, "Creating branch");

            let write_token = get_write_token();
            let branch_id = branch::create(
                repository.clone(),
                &write_token,
                branch,
                name.as_str(),
                category.as_str(),
                creator.as_str(),
                created,
                stack.clone(),
                false,
                false,
            )
            .await
            .map_err(|err| {
                if err.is_branch_already_exists() {
                    Status::already_exists(err.to_string())
                } else {
                    Status::invalid_argument(err.to_string())
                }
            })?;

            notification_sender
                .branch_created(context.repository_id, branch_id)
                .await;
            hook_dispatcher.spawn_post(HookPoint::BranchCreate, hook_ctx);

            let metadata_hash = branch::metadata_hash(repository.clone(), branch_id)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;
            let metadata = branch::load_metadata(repository.clone(), metadata_hash)
                .await
                .warn_map_err(|err| Status::internal(err.to_string()))?;

            debug!({BRANCH_ID} = %branch_id, %name, "Created branch");

            let response_branch =
                build_branch(repository, branch_id, &metadata, metadata_hash, false).await?;

            instrument_provider
                .counter("num_branches_created")
                .add(1, &[]);

            Ok(Response::new(BranchCreateResponse {
                branch: Some(response_branch),
            }))
        })
        .await
}

#[cfg(test)]
mod test {
    mod input_length_validation {
        use lore_revision::branch;
        use lore_revision::repository;

        use super::super::*;

        #[test]
        fn accepts_valid_input() {
            validate_create_input("my-branch", "feature", "alice")
                .expect("valid input should pass");
        }

        #[test]
        fn accepts_name_at_max_length() {
            let name = "a".repeat(branch::MAX_NAME_LEN);
            validate_create_input(&name, "feature", "alice")
                .expect("name at exactly MAX_NAME_LEN should pass");
        }

        #[test]
        fn rejects_oversized_branch_name() {
            let long_name = "a".repeat(branch::MAX_NAME_LEN + 1);
            let err = validate_create_input(&long_name, "feature", "alice")
                .expect_err("should reject oversized name");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("Branch name exceeds maximum length"));
        }

        #[test]
        fn rejects_oversized_category() {
            let long_cat = "a".repeat(branch::MAX_NAME_LEN + 1);
            let err = validate_create_input("my-branch", &long_cat, "alice")
                .expect_err("should reject oversized category");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(
                err.message()
                    .contains("Branch category exceeds maximum length")
            );
        }

        #[test]
        fn rejects_oversized_creator() {
            let long_creator = "a".repeat(repository::MAX_NAME_LEN + 1);
            let err = validate_create_input("my-branch", "feature", &long_creator)
                .expect_err("should reject oversized creator");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("Creator exceeds maximum length"));
        }
    }

    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::lore::RepositoryId;
    use lore_telemetry::InstrumentProvider;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;
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

    #[tokio::test]
    async fn create_returns_full_branch_record() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_created()
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let mut request = Request::new(BranchCreateRequest {
                id: branch_id.into(),
                name: "main".into(),
                creator: Some("alice".into()),
                category: "default".into(),
                stack: vec![],
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let hook_dispatcher = HookDispatcher::empty();
            let response = handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("Request failed");

            let branch = response
                .into_inner()
                .branch
                .expect("response should include Branch");
            assert_eq!(branch.name, "main");
            assert_eq!(branch.creator, "alice");
            assert_eq!(branch.category, "default");
            assert!(!branch.deleted);
            assert!(branch.created > 0);
            assert!(!branch.id.is_empty());
            assert!(!branch.metadata.is_empty());
        }))
        .await;
    }

    #[tokio::test]
    async fn empty_name_returns_invalid_argument() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let mut request = Request::new(BranchCreateRequest {
                id: branch_id.into(),
                name: String::new(),
                creator: Some("alice".into()),
                category: "default".into(),
                stack: vec![],
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let hook_dispatcher = HookDispatcher::empty();
            let err = handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("empty name should fail");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn unset_creator_falls_back_to_jwt_identity() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_created()
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let mut request = Request::new(BranchCreateRequest {
                id: branch_id.into(),
                name: "main".into(),
                creator: None,
                category: "default".into(),
                stack: vec![],
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            request.extensions_mut().insert(AuthorizationToken {
                user_id: "jwt-user".into(),
                ..AuthorizationToken::default()
            });

            let hook_dispatcher = HookDispatcher::empty();
            let response = handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("Request failed");

            let branch = response
                .into_inner()
                .branch
                .expect("response should include Branch");
            assert_eq!(branch.creator, "jwt-user");
        }))
        .await;
    }

    #[tokio::test]
    async fn duplicate_id_returns_already_exists() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_created()
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            let make_request = || {
                let mut request = Request::new(BranchCreateRequest {
                    id: branch_id.into(),
                    name: "main".into(),
                    creator: Some("alice".into()),
                    category: "default".into(),
                    stack: vec![],
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
                );
                request
            };

            let hook_dispatcher = HookDispatcher::empty();
            handler(
                make_request(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("first create should succeed");

            let err = handler(
                make_request(),
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect_err("duplicate id should fail");
            assert_eq!(err.code(), tonic::Code::AlreadyExists);
        }))
        .await;
    }
}
