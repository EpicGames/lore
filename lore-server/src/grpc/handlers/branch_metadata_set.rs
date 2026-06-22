// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_proto::BranchMetadataSetRequest;
use lore_proto::BranchMetadataSetResponse;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_revision::metadata::branch::READ_ONLY_KEYS;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::require_write_permission;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

/// Validate that read-only fields have not been changed between the current and proposed
/// metadata blobs.
pub(crate) fn validate_read_only_fields(
    current: &Metadata,
    proposed: &Metadata,
) -> Result<(), Status> {
    for key in READ_ONLY_KEYS {
        let current_value = current.get_typed(key);
        let proposed_value = proposed.get_typed(key);

        match (current_value, proposed_value) {
            (Ok((current_bytes, current_type)), Ok((proposed_bytes, proposed_type))) => {
                if current_type != proposed_type || current_bytes != proposed_bytes {
                    return Err(Status::invalid_argument(format!(
                        "cannot modify read-only key '{key}'"
                    )));
                }
            }
            (Ok(_), Err(_)) => {
                return Err(Status::invalid_argument(format!(
                    "cannot remove read-only key '{key}'"
                )));
            }
            (Err(_), Ok(_) | Err(_)) => {}
        }
    }
    Ok(())
}

/// Validate that all Address-typed values in the proposed metadata blob reference existing
/// blobs in the immutable store.
pub(crate) async fn validate_binary_blobs(
    repo: Arc<RepositoryContext>,
    proposed: &Metadata,
) -> Result<(), Status> {
    let mut addresses = vec![];
    proposed
        .walk(
            |_key_slice: &[u8], value_slice: &[u8], value_type: MetadataType| {
                if value_type == MetadataType::Address
                    && value_slice.len() == std::mem::size_of::<Address>()
                {
                    let address: Address = value_slice.into();
                    addresses.push(address);
                }
            },
        )
        .map_err(|err| {
            warn_error_to_status(&err, |err| {
                Status::internal(format!("failed to walk proposed metadata: {err}"))
            })
        })?;

    for address in addresses {
        let options = lore_revision::immutable::read_options_from_repository(&repo).with_cache();
        if lore_revision::immutable::read(repo.clone(), address, None, options)
            .await
            .is_err()
        {
            return Err(Status::not_found(format!(
                "binary blob not found: {address}"
            )));
        }
    }
    Ok(())
}

#[tracing::instrument(name = "BranchMetadataSet::handle", skip_all)]
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

    let branch_id = BranchId::from(req.branch_id);
    if branch_id == BranchId::default() {
        return Err(Status::invalid_argument("Missing branch ID"));
    }

    let expected_hash: Hash = req.expected_hash.into();
    let new_hash: Hash = req.new_hash.into();

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // Deserialize current and proposed blobs for validation
            let current_metadata = if !expected_hash.is_zero() {
                Metadata::deserialize(repository.clone(), expected_hash)
                    .await
                    .filter_slow_down()?
                    .map_err(|err| {
                        warn_error_to_status(&err, |err| {
                            Status::invalid_argument(format!(
                                "failed to deserialize current metadata: {err}"
                            ))
                        })
                    })?
            } else {
                Metadata::new()
            };

            let proposed_metadata = Metadata::deserialize(repository.clone(), new_hash)
                .await
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::invalid_argument(format!(
                            "failed to deserialize proposed metadata: {err}"
                        ))
                    })
                })?;

            // Validate read-only fields are unchanged
            validate_read_only_fields(&current_metadata, &proposed_metadata)?;

            // Validate binary blob references exist
            validate_binary_blobs(repository.clone(), &proposed_metadata).await?;

            // Perform compare-and-swap
            let (metadata_key, key_type) = branch::mutable_key(
                repository::SALT_LORE,
                branch::METADATA,
                repository_id,
                branch_id,
            );
            let write_token = get_write_token();
            let previous = repository
                .write_mutable_store(&write_token)
                .compare_and_swap(
                    repository_id,
                    metadata_key,
                    expected_hash,
                    new_hash,
                    key_type,
                )
                .await
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::internal(format!("failed to update metadata: {err}"))
                    })
                })?;

            if previous == expected_hash {
                Ok(Response::new(BranchMetadataSetResponse {
                    success: true,
                    current_hash: new_hash.into(),
                }))
            } else {
                Ok(Response::new(BranchMetadataSetResponse {
                    success: false,
                    current_hash: previous.into(),
                }))
            }
        })
        .await
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_proto::BranchMetadataSetRequest;
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
        expected_hash: Hash,
        new_hash: Hash,
    ) -> Request<BranchMetadataSetRequest> {
        let mut request = Request::new(BranchMetadataSetRequest {
            branch_id: branch.into(),
            expected_hash: expected_hash.into(),
            new_hash: new_hash.into(),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    /// Create a branch and return its metadata hash.
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

    /// Serialize a metadata blob and return its hash.
    async fn serialize_metadata(repository: Arc<RepositoryContext>, metadata: &Metadata) -> Hash {
        metadata
            .serialize(repository)
            .await
            .expect("Failed to serialize metadata")
    }

    #[tokio::test]
    async fn set_custom_key_succeeds() {
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

            let current_hash = create_branch(repository.clone(), branch_id).await;

            // Build proposed metadata with the same read-only fields plus a custom key
            let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
                .await
                .expect("deserialize");
            proposed
                .set_string("custom-key", "custom-value")
                .expect("set custom key");
            let new_hash = serialize_metadata(repository.clone(), &proposed).await;

            let request = make_request(repository_id, branch_id, current_hash, new_hash);
            let response = handler(request, immutable_store, mutable_store)
                .await
                .expect("Handler failed");

            let inner = response.into_inner();
            assert!(inner.success);
            assert_eq!(Hash::from(inner.current_hash), new_hash);
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

            let current_hash = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
                .await
                .expect("deserialize");
            proposed
                .set_string(branch::NAME, "renamed-branch")
                .expect("set name");
            let new_hash = serialize_metadata(repository.clone(), &proposed).await;

            let request = make_request(repository_id, branch_id, current_hash, new_hash);
            let result = handler(request, immutable_store, mutable_store).await;

            assert!(result.is_err());
            let status = result.unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert!(status.message().contains("name"));
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

            let current_hash = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
                .await
                .expect("deserialize");
            proposed.remove_key(branch::CREATOR);
            let new_hash = serialize_metadata(repository.clone(), &proposed).await;

            let request = make_request(repository_id, branch_id, current_hash, new_hash);
            let result = handler(request, immutable_store, mutable_store).await;

            assert!(result.is_err());
            let status = result.unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert!(status.message().contains("creator"));
        }))
        .await;
    }

    #[tokio::test]
    async fn allows_modification_of_protect_field() {
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

            let current_hash = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
                .await
                .expect("deserialize");
            proposed
                .set_bool(branch::PROTECT, true)
                .expect("set protect");
            let new_hash = serialize_metadata(repository.clone(), &proposed).await;

            let request = make_request(repository_id, branch_id, current_hash, new_hash);
            let response = handler(request, immutable_store, mutable_store)
                .await
                .expect("Handler failed — protect should be writable");

            assert!(response.into_inner().success);
        }))
        .await;
    }

    #[tokio::test]
    async fn cas_fails_on_stale_expected_hash() {
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

            let current_hash = create_branch(repository.clone(), branch_id).await;

            // Build a valid proposed metadata with a custom key
            let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
                .await
                .expect("deserialize");
            proposed.set_string("key", "value").expect("set custom key");
            let new_hash = serialize_metadata(repository.clone(), &proposed).await;

            // Use a bogus expected hash
            let stale_hash = Hash::from(random::<[u8; 32]>());
            let request = make_request(repository_id, branch_id, stale_hash, new_hash);
            let result = handler(request, immutable_store, mutable_store).await;

            // CAS should fail because expected_hash doesn't match (it can't be deserialized)
            assert!(result.is_err());
        }))
        .await;
    }

    #[tokio::test]
    async fn rejects_missing_branch_id() {
        let repository_id = random::<RepositoryId>();

        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        let request = make_request(
            repository_id,
            BranchId::default(),
            Hash::default(),
            Hash::default(),
        );
        let result = handler(request, immutable_store, mutable_store).await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
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

            let current_hash = create_branch(repository.clone(), branch_id).await;

            let mut proposed = Metadata::deserialize(repository.clone(), current_hash)
                .await
                .expect("deserialize");
            proposed
                .set_string(branch::CATEGORY, "changed-category")
                .expect("set category");
            let new_hash = serialize_metadata(repository.clone(), &proposed).await;

            let request = make_request(repository_id, branch_id, current_hash, new_hash);
            let result = handler(request, immutable_store, mutable_store).await;

            assert!(result.is_err());
            let status = result.unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert!(status.message().contains("category"));
        }))
        .await;
    }
}
