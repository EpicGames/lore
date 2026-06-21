// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1::BranchMetadataSetRequest;
use lore_proto::lore::revision::v1::BranchMetadataSetResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::METADATA;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::handlers::branch_metadata_set::validate_binary_blobs;
use crate::grpc::handlers::branch_metadata_set::validate_read_only_fields;
use crate::grpc::require_write_permission;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

/// `lore.revision.v1.RevisionService.BranchMetadataSet` handler.
///
/// Compare-and-swap update of a branch's metadata pointer. CAS miss is
/// signalled in-band: the response always carries the current pointer
/// after the call. On hit, `response.metadata == request.updated`; on
/// miss, `response.metadata` is the unchanged prior value, and the
/// caller compares against `request.updated` to detect the miss.
///
/// `protect` remains writable through this RPC (it is not in the
/// read-only key set) so clients can continue to toggle branch
/// protection without dedicated protect/unprotect RPCs.
#[tracing::instrument(name = "BranchMetadataSet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<BranchMetadataSetRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchMetadataSetResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    require_write_permission(request.extensions(), repository_id)?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let branch_id = BranchId::from(req.id);
    if branch_id == BranchId::default() {
        return Err(Status::invalid_argument("Branch id must be non-zero"));
    }

    let expected: Hash = req.expected.into();
    let updated: Hash = req.updated.into();

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            debug!(
                {BRANCH_ID} = %branch_id,
                expected = %expected,
                updated = %updated,
                "Branch metadata CAS",
            );

            // Reject writes to branches that have no metadata pointer
            // at all (never created). Deleted branches keep their
            // metadata blob and pass this check.
            branch::metadata_hash(repository.clone(), branch_id)
                .await
                .map_err(|err| {
                    info!({BRANCH_ID} = %branch_id, ?err, "Branch metadata not found");
                    Status::not_found(format!("Branch {branch_id} not found"))
                })?;

            // Caller-supplied `expected` is treated as authoritative for
            // the validation pass: it lets the server check that the
            // proposed transformation is well-formed even if the
            // underlying state has moved. The CAS itself ensures
            // atomicity against the actual current pointer.
            let current_metadata = if expected.is_zero() {
                Metadata::new()
            } else {
                Metadata::deserialize(repository.clone(), expected)
                    .await
                    .map_err(|err| {
                        warn_error_to_status(&err, |err| {
                            Status::invalid_argument(format!(
                                "failed to deserialize expected metadata: {err}"
                            ))
                        })
                    })?
            };

            let proposed_metadata = Metadata::deserialize(repository.clone(), updated)
                .await
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::invalid_argument(format!(
                            "failed to deserialize updated metadata: {err}"
                        ))
                    })
                })?;

            validate_read_only_fields(&current_metadata, &proposed_metadata)?;
            validate_binary_blobs(repository.clone(), &proposed_metadata).await?;

            let (metadata_key, key_type) = branch::mutable_key(
                repository::SALT_LORE,
                branch::METADATA,
                repository_id,
                branch_id,
            );
            let write_token = get_write_token();
            let previous = repository
                .write_mutable_store(&write_token)
                .compare_and_swap(repository_id, metadata_key, expected, updated, key_type)
                .await
                .map_err(|err| {
                    warn!({BRANCH_ID} = %branch_id, ?err, "Branch metadata CAS failed");
                    warn_error_to_status(&err, |err| {
                        Status::internal(format!("failed to update branch metadata: {err}"))
                    })
                })?;

            let metadata = if previous == expected {
                updated
            } else {
                previous
            };

            debug!(
                {BRANCH_ID} = %branch_id,
                {METADATA} = %metadata,
                hit = previous == expected,
                "Branch metadata CAS response",
            );

            Ok(Response::new(BranchMetadataSetResponse {
                metadata: metadata.into(),
            }))
        })
        .await
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::repository::RepositoryContext;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::store::test_store_create;

    fn make_request(
        repository: RepositoryId,
        branch: BranchId,
        expected: Hash,
        updated: Hash,
    ) -> Request<BranchMetadataSetRequest> {
        let mut request = Request::new(BranchMetadataSetRequest {
            id: branch.into(),
            expected: expected.into(),
            updated: updated.into(),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    /// Create a branch and return its current metadata pointer.
    async fn create_branch(repository: Arc<RepositoryContext>, branch_id: BranchId) -> Hash {
        let write_token = get_write_token();
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
        .expect("Failed to create branch");

        branch::metadata_hash(repository, branch_id)
            .await
            .expect("Failed to load metadata hash")
    }

    async fn serialize(repository: Arc<RepositoryContext>, metadata: &Metadata) -> Hash {
        metadata
            .serialize(repository)
            .await
            .expect("Failed to serialize metadata")
    }

    #[tokio::test]
    async fn cas_hit_swaps_pointer_and_returns_updated() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize current");
            proposed
                .set_string("custom-key", "custom-value")
                .expect("set custom key");
            let updated = serialize(repository.clone(), &proposed).await;

            let response = handler(
                make_request(repository_id, branch_id, current, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect("CAS hit should succeed");
            assert_eq!(Hash::from(response.into_inner().metadata), updated);
        }))
        .await;
    }

    #[tokio::test]
    async fn cas_miss_returns_current_pointer_in_band() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            // Build a stale "expected" blob that exists in the store
            // (passes deserialize) but doesn't match the current pointer.
            let mut stale = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize current");
            stale.set_string("stale", "value").expect("set stale key");
            let stale_expected = serialize(repository.clone(), &stale).await;
            assert_ne!(stale_expected, current);

            // Build a proposed update derived from `stale` so read-only
            // validation passes between expected and updated.
            let mut proposed = Metadata::deserialize(repository.clone(), stale_expected)
                .await
                .expect("deserialize stale");
            proposed
                .set_string("intent", "value")
                .expect("set intent key");
            let updated = serialize(repository.clone(), &proposed).await;

            let response = handler(
                make_request(repository_id, branch_id, stale_expected, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect("CAS miss should still return Ok");
            // In-band miss signal: response.metadata == current (unchanged),
            // not the requested `updated`.
            let returned: Hash = response.into_inner().metadata.into();
            assert_eq!(returned, current);
            assert_ne!(returned, updated);
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_modification_of_read_only_name() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize");
            proposed
                .set_string(branch::NAME, "renamed")
                .expect("set name");
            let updated = serialize(repository.clone(), &proposed).await;

            let err = handler(
                make_request(repository_id, branch_id, current, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("read-only name modification must fail");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("name"));
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_modification_of_read_only_category() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize");
            proposed
                .set_string(branch::CATEGORY, "changed")
                .expect("set category");
            let updated = serialize(repository.clone(), &proposed).await;

            let err = handler(
                make_request(repository_id, branch_id, current, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("read-only category modification must fail");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("category"));
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_removal_of_read_only_creator() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize");
            proposed.remove_key(branch::CREATOR);
            let updated = serialize(repository.clone(), &proposed).await;

            let err = handler(
                make_request(repository_id, branch_id, current, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("removal of read-only creator must fail");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("creator"));
        }))
        .await;
    }

    #[tokio::test]
    async fn allows_protect_modification() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize");
            proposed
                .set_bool(branch::PROTECT, true)
                .expect("set protect");
            let updated = serialize(repository.clone(), &proposed).await;

            let response = handler(
                make_request(repository_id, branch_id, current, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect("protect toggle must succeed");
            assert_eq!(Hash::from(response.into_inner().metadata), updated);
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_unknown_branch_with_not_found() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let err = handler(
                make_request(repository_id, branch_id, Hash::default(), Hash::default()),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("unknown branch must fail");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn cas_succeeds_on_deleted_branch() {
        let repository_id = random::<RepositoryId>();
        let main_id = BranchId::from(uuid::Uuid::now_v7());
        let child_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let write_token = get_write_token();

            // main acts as a real parent so the child branch can carry a
            // non-empty stack and be deletable. Push one revision on
            // main first since `branch::create` rejects zero-revision
            // parents that aren't the repository's default.
            branch::create(
                repository.clone(),
                &write_token,
                main_id,
                "main",
                branch::default_category(),
                "creator",
                1,
                vec![],
                false,
                false,
            )
            .await
            .expect("create main");
            let state = lore_revision::state::State::new();
            state.set_parent_self(Hash::default());
            state.set_revision_number(1);
            let state_hash = state
                .serialize(repository.clone(), &write_token)
                .await
                .expect("serialize state");
            let main_latest = crate::grpc::handlers::branch_push::push(
                repository.clone(),
                main_id,
                state_hash,
                true,
                true,
                false,
                lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
            )
            .await
            .expect("seed main")
            .revision;

            branch::create(
                repository.clone(),
                &write_token,
                child_id,
                "feature",
                branch::personal_category(),
                "creator",
                1,
                vec![lore_base::types::BranchPoint {
                    branch: main_id,
                    revision: main_latest,
                }],
                false,
                false,
            )
            .await
            .expect("create child");
            let current = branch::metadata_hash(repository.clone(), child_id)
                .await
                .expect("child metadata");
            branch::delete(repository.clone(), child_id)
                .await
                .expect("delete child");

            // Delete preserves the metadata pointer; CAS must still
            // succeed against it.
            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize current");
            proposed
                .set_string("custom-key", "value")
                .expect("set custom");
            let updated = serialize(repository.clone(), &proposed).await;

            let response = handler(
                make_request(repository_id, child_id, current, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect("CAS on deleted branch must succeed");
            assert_eq!(Hash::from(response.into_inner().metadata), updated);
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_garbage_updated_hash() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            // Random hash that almost certainly doesn't deserialize to
            // a valid metadata blob.
            let garbage = Hash::from(random::<[u8; 32]>());

            let err = handler(
                make_request(repository_id, branch_id, current, garbage),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("garbage updated hash must fail to deserialize");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_garbage_expected_hash() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            // Build a real proposed blob so deserialization of `updated` succeeds.
            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize current");
            proposed.set_string("k", "v").expect("set custom");
            let updated = serialize(repository.clone(), &proposed).await;

            // Random hash that almost certainly doesn't deserialize to a valid metadata.
            let garbage = Hash::from(random::<[u8; 32]>());

            let err = handler(
                make_request(repository_id, branch_id, garbage, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("garbage expected hash must fail to deserialize");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_dangling_address_reference() {
        let repository_id = random::<RepositoryId>();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            let current = create_branch(repository.clone(), branch_id).await;

            // Add an Address-typed field whose hash points to nothing
            // in the immutable store. validate_binary_blobs walks the
            // metadata, sees the Address, fails to read it.
            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize current");
            let dangling =
                lore_storage::Address::zero_context_hash(Hash::from(random::<[u8; 32]>()));
            proposed
                .set_address("payload", dangling)
                .expect("set address");
            let updated = serialize(repository.clone(), &proposed).await;

            let err = handler(
                make_request(repository_id, branch_id, current, updated),
                immutable_store,
                mutable_store,
            )
            .await
            .expect_err("dangling address reference must fail");
            assert_eq!(err.code(), tonic::Code::NotFound);
            assert!(err.message().contains("binary blob not found"));
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_zero_branch_id() {
        let repository_id = random::<RepositoryId>();
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        let err = handler(
            make_request(
                repository_id,
                BranchId::default(),
                Hash::default(),
                Hash::default(),
            ),
            immutable_store,
            mutable_store,
        )
        .await
        .expect_err("zero branch id must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn rejects_missing_repository_id() {
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        let request = Request::new(BranchMetadataSetRequest {
            id: branch_id.into(),
            expected: Hash::default().into(),
            updated: Hash::default().into(),
        });
        let err = handler(request, immutable_store, mutable_store)
            .await
            .expect_err("missing repository must fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
