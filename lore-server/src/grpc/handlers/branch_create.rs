// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_proto::BranchCreateRequest;
use lore_proto::BranchCreateResponse;
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

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::grpc::require_write_permission;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

// Reject oversized string fields early to prevent resource exhaustion.
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

// Branch existence checks and write ordering are implemented in
// urc-core/src/branch.rs::create(). See the documentation there for the
// full decision matrix. The query handler in branch_query.rs uses the
// same existence model.

#[tracing::instrument(name = "BranchCreate::handle", skip_all)]
pub async fn handler(
    request: Request<BranchCreateRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification_sender: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchCreateResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    require_write_permission(request.extensions(), repository_id)?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let branch = BranchId::from(req.branch);
    let category = req.category;
    let name = req.name;
    let creator = req.creator;
    let created = req.created;

    let mut stack = req.stack;
    if stack.is_empty()
        && let Some(branch) = req.parent_deprecated
        && let Some(revision) = req.revision_deprecated
    {
        stack.push(lore_proto::BranchPoint { branch, revision });
    }

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let hook_ctx = HookContext::builder()
                .correlation_id(correlation_id)
                .hook_point(HookPoint::BranchCreate)
                .repository(repository_id)
                .user(user_id)
                .branch(branch)
                .build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchCreate, &hook_ctx)
                .map_err(hook_error_to_status)?;

            validate_create_input(&name, &category, &creator)?;

            let stack: Vec<BranchPoint> = stack.into_iter().map(BranchPoint::from).collect();
            debug!({BRANCH_ID} = %branch, branch_name = name, stack = ?stack, "Creating branch");

            let write_token = get_write_token();
            let branch = lore_revision::branch::create(
                repository.clone(),
                &write_token,
                branch,
                name.as_str(),
                category.as_str(),
                creator.as_str(),
                created,
                stack,
                false,
                false, /* Do not create linked repository branches */
            )
            .await
            .map_err(|err| Status::invalid_argument(err.to_string()))?;

            notification_sender
                .branch_created(repository_id, branch)
                .await;

            hook_dispatcher.spawn_post(HookPoint::BranchCreate, hook_ctx);

            let revision = branch::load_latest(repository, branch)
                .await
                .unwrap_or_default();

            debug!("Created branch {name} ({branch}) at revision {revision}");
            let num_branches_created = instrument_provider.counter("num_branches_created");
            num_branches_created.add(1, &[]);

            Ok(Response::new(BranchCreateResponse {
                revision: revision.into(),
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
    use lore_revision::branch;
    use lore_revision::lore::RepositoryId;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use mockall::predicate::eq;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
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
    async fn sends_created_notification_for_created_branch() {
        let repository = random::<RepositoryId>();
        let branch_context = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let mut notification_sender = MockNotificationSender::new();
        notification_sender
            .expect_branch_created()
            .with(eq(repository), eq(branch_context))
            .return_once(|_, _| ());
        let notification_sender = Arc::new(notification_sender);
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let mut request = Request::new(BranchCreateRequest {
                branch: branch_context.into(),
                name: branch::DEFAULT_DEFAULT_NAME.into(),
                creator: "creator".into(),
                created: 1,
                category: "category".into(),
                stack: vec![],
                revision_deprecated: None,
                parent_deprecated: None,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let hook_dispatcher = HookDispatcher::empty();
            handler(
                request,
                immutable_store.clone(),
                mutable_store.clone(),
                notification_sender.clone(),
                &hook_dispatcher,
                &instrument_provider,
            )
            .await
            .expect("Request failed");
        }))
        .await;
    }

    #[tokio::test]
    async fn no_created_notification_for_branch_create_errors() {
        let repository = random::<RepositoryId>();
        let branch_context = BranchId::from(uuid::Uuid::now_v7());

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        // no notifications sent, so no expectations required
        let notification_sender = Arc::new(MockNotificationSender::new());
        let instrument_provider = TestInstrumentProvider {};

        Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let mut request = Request::new(BranchCreateRequest {
                branch: branch_context.into(),
                // an invalid branch name that will cause core branch_create
                // logic to not create the branch
                name: "".into(),
                creator: "creator".into(),
                created: 1,
                category: "category".into(),
                stack: vec![],
                revision_deprecated: None,
                parent_deprecated: None,
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
            .unwrap_err();

            assert_eq!(response.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }
}
