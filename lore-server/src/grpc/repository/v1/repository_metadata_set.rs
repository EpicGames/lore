// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_proto::lore::repository::v1::RepositoryMetadataSetRequest;
use lore_proto::lore::repository::v1::RepositoryMetadataSetResponse;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_revision::metadata::repository::READ_ONLY_KEYS;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_storage::hash;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::extract_authorization_header;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::no_repository_access_status;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

/// `lore.repository.v1.RepositoryService.RepositoryMetadataSet` handler.
///
/// Compare-and-swap update of the repository metadata pointer.
/// Validates that the proposed metadata blob (a) preserves all read-only
/// fields and (b) references only existing immutable blobs for any
/// Address-typed entries, then performs a CAS on the mutable store.
///
/// CAS hit / miss is signalled in-band by comparing
/// `response.metadata` to `request.updated`; the gRPC status is always
/// `Ok` unless an internal failure prevents the CAS from being attempted
/// at all.
#[tracing::instrument(name = "RepositoryMetadataSet::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryMetadataSetRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryMetadataSetResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let authorization = extract_authorization_header(&request);
    let req = request.into_inner();

    let repository_id: Context = req.id.into();
    if repository_id == Context::default() {
        return Err(Status::invalid_argument("Missing repository id"));
    }

    let expected: Hash = req.expected.into();
    let updated: Hash = req.updated.into();

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id.into(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            authorizer
                .check_repository_access(authorization, repository_id.into())
                .await
                .map_err(|_err| no_repository_access_status())?;

            let current_metadata = if !expected.is_zero() {
                Metadata::deserialize(repository.clone(), expected)
                    .await
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

            let proposed_metadata = Metadata::deserialize(repository.clone(), updated)
                .await
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::invalid_argument(format!(
                            "failed to deserialize proposed metadata: {err}"
                        ))
                    })
                })?;

            validate_read_only_fields(&current_metadata, &proposed_metadata)?;
            validate_binary_blobs(repository.clone(), &proposed_metadata).await?;

            let metadata_key = hash::hash_function_arg(
                repository::SALT_LORE,
                repository::METADATA,
                hex::encode(repository_id.data()).as_str(),
            );
            let write_token = get_write_token();
            let previous = repository
                .write_mutable_store(&write_token)
                .compare_and_swap(
                    repository_id.into(),
                    metadata_key,
                    expected,
                    updated,
                    KeyType::RepositoryMetadata,
                )
                .await
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::internal(format!("failed to update metadata: {err}"))
                    })
                })?;

            let metadata = if previous == expected {
                updated
            } else {
                previous
            };
            Ok(Response::new(RepositoryMetadataSetResponse {
                metadata: metadata.into(),
            }))
        })
        .await
}

/// Reject a proposed metadata blob that mutates a read-only field.
fn validate_read_only_fields(current: &Metadata, proposed: &Metadata) -> Result<(), Status> {
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

/// Reject a proposed metadata blob that references an Address that is not
/// currently addressable in CAS.
async fn validate_binary_blobs(
    repo: Arc<RepositoryContext>,
    proposed: &Metadata,
) -> Result<(), Status> {
    let mut addresses = vec![];
    proposed.walk(
        |_key_slice: &[u8], value_slice: &[u8], value_type: MetadataType| {
            if value_type == MetadataType::Address
                && value_slice.len() == std::mem::size_of::<Address>()
            {
                let address: Address = value_slice.into();
                addresses.push(address);
            }
        },
    );

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

#[cfg(test)]
mod tests {
    mod auth_guard {
        use lore_base::types::RepositoryId;
        use lore_revision::repository::RepositoryMetadata;
        use tonic::Code;

        use super::super::*;
        use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
        use crate::store::test_store_create;

        mockall::mock! {
            pub Authorizer {}

            #[async_trait::async_trait]
            impl RepositoryAuthorizer for Authorizer {
                async fn check_repository_access(
                    &self,
                    authorization: Option<String>,
                    repository_id: RepositoryId,
                ) -> Result<(), Status>;
            }
        }

        const REPOSITORY_ID: [u8; 16] = [1u8; 16];

        /// Writes a valid metadata blob to the immutable store and returns its
        /// hash. `metadata_set` can then use that hash as `updated`.
        async fn seed_metadata_blob(
            immutable: Arc<dyn lore_storage::ImmutableStore>,
            mutable: Arc<dyn lore_storage::MutableStore>,
        ) -> lore_base::types::Hash {
            let repo_ctx = Arc::new(RepositoryContext::new_server_context(
                immutable,
                mutable,
                Context::from(REPOSITORY_ID).into(),
            ));
            lore_revision::repository::metadata_store(
                repo_ctx,
                RepositoryMetadata {
                    name: "test".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn no_auth_configured_allows_operation() {
            let (immutable, mutable, execution) = test_store_create().await.unwrap();
            LORE_CONTEXT
                .scope(execution, async move {
                    let hash = seed_metadata_blob(immutable.clone(), mutable.clone()).await;
                    let request = Request::new(RepositoryMetadataSetRequest {
                        id: REPOSITORY_ID.to_vec().into(),
                        expected: vec![0u8; 32].into(),
                        updated: hash.into(),
                    });
                    handler(
                        request,
                        Arc::new(AllowAllRepositoryAuthorizer),
                        immutable,
                        mutable,
                    )
                    .await
                    .unwrap();
                })
                .await;
        }

        #[tokio::test]
        async fn auth_configured_no_access_returns_permission_denied() {
            let (immutable, mutable, _) = test_store_create().await.unwrap();
            let mut mock = MockAuthorizer::new();
            mock.expect_check_repository_access()
                .returning(|_, _| Err(Status::permission_denied("denied")));
            let request = Request::new(RepositoryMetadataSetRequest {
                id: REPOSITORY_ID.to_vec().into(),
                expected: vec![0u8; 32].into(),
                updated: vec![1u8; 32].into(),
            });
            let err = handler(request, Arc::new(mock), immutable, mutable)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied);
            assert_eq!(err.message(), "Unauthorized");
        }

        #[tokio::test]
        async fn auth_configured_with_access_allows_operation() {
            let (immutable, mutable, execution) = test_store_create().await.unwrap();
            LORE_CONTEXT
                .scope(execution, async move {
                    let hash = seed_metadata_blob(immutable.clone(), mutable.clone()).await;
                    let mut mock = MockAuthorizer::new();
                    mock.expect_check_repository_access()
                        .returning(|_, _| Ok(()));
                    let request = Request::new(RepositoryMetadataSetRequest {
                        id: REPOSITORY_ID.to_vec().into(),
                        expected: vec![0u8; 32].into(),
                        updated: hash.into(),
                    });
                    handler(request, Arc::new(mock), immutable, mutable)
                        .await
                        .unwrap();
                })
                .await;
        }
    }

    mod validate_read_only_fields {
        use lore_base::types::Context;
        use lore_revision::repository;

        use super::super::validate_read_only_fields;
        use super::super::*;

        /// Build a metadata blob with every read-only key populated to a
        /// known value so individual tests can mutate exactly one field
        /// and assert the rejection is attributable to that mutation.
        fn baseline() -> Metadata {
            let mut metadata = Metadata::new();
            metadata.set_string(repository::NAME, "repo").unwrap();
            metadata
                .set_context(repository::DEFAULT_BRANCH, Context::default())
                .unwrap();
            metadata
                .set_string(repository::DEFAULT_BRANCH_NAME, "main")
                .unwrap();
            metadata.set_string(repository::CREATOR, "alice").unwrap();
            metadata.set_u64(repository::CREATED, 100).unwrap();
            metadata
        }

        #[test]
        fn accepts_unchanged_read_only_fields_with_writable_change() {
            let current = baseline();
            let mut proposed = baseline();
            proposed
                .set_string(repository::DESCRIPTION, "edited description")
                .unwrap();
            validate_read_only_fields(&current, &proposed)
                .expect("description is writable, all read-only fields unchanged");
        }

        #[test]
        fn rejects_name_modification() {
            let current = baseline();
            let mut proposed = baseline();
            proposed.set_string(repository::NAME, "renamed").unwrap();
            let err = validate_read_only_fields(&current, &proposed)
                .expect_err("mutating name must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains(repository::NAME));
        }

        #[test]
        fn rejects_creator_modification() {
            let current = baseline();
            let mut proposed = baseline();
            proposed.set_string(repository::CREATOR, "mallory").unwrap();
            let err = validate_read_only_fields(&current, &proposed)
                .expect_err("mutating creator must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains(repository::CREATOR));
        }

        #[test]
        fn rejects_default_branch_modification() {
            let current = baseline();
            let mut proposed = baseline();
            proposed
                .set_context(repository::DEFAULT_BRANCH, Context::from([1u8; 16]))
                .unwrap();
            let err = validate_read_only_fields(&current, &proposed)
                .expect_err("mutating default-branch must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains(repository::DEFAULT_BRANCH));
        }

        #[test]
        fn rejects_default_branch_name_modification() {
            let current = baseline();
            let mut proposed = baseline();
            proposed
                .set_string(repository::DEFAULT_BRANCH_NAME, "trunk")
                .unwrap();
            let err = validate_read_only_fields(&current, &proposed)
                .expect_err("mutating default-branch-name must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains(repository::DEFAULT_BRANCH_NAME));
        }

        #[test]
        fn rejects_created_modification() {
            let current = baseline();
            let mut proposed = baseline();
            proposed.set_u64(repository::CREATED, 200).unwrap();
            let err = validate_read_only_fields(&current, &proposed)
                .expect_err("mutating created must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains(repository::CREATED));
        }

        #[test]
        fn rejects_read_only_key_removal() {
            let current = baseline();
            let mut proposed = baseline();
            assert!(proposed.remove_key(repository::CREATOR));
            let err = validate_read_only_fields(&current, &proposed)
                .expect_err("removing a read-only key must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains(repository::CREATOR));
            assert!(err.message().contains("remove"));
        }

        #[test]
        fn accepts_setting_read_only_keys_when_current_is_empty() {
            // CAS-from-zero path: `expected` was Hash::default(), so the
            // server passes an empty `current` Metadata. Every key in
            // proposed is being set for the first time and must be
            // allowed.
            let current = Metadata::new();
            let proposed = baseline();
            validate_read_only_fields(&current, &proposed)
                .expect("first-time write of read-only keys must be allowed");
        }

        #[test]
        fn ignores_read_only_keys_absent_from_both() {
            // No read-only key is present in either blob; the validator
            // must not invent rejections.
            let current = Metadata::new();
            let proposed = Metadata::new();
            validate_read_only_fields(&current, &proposed)
                .expect("absence on both sides is a no-op");
        }

        #[test]
        fn type_mismatch_on_read_only_key_is_rejected() {
            // Same key, same logical value, different MetadataType: the
            // validator compares (bytes, type) and must catch this.
            let mut current = Metadata::new();
            current.set_string(repository::CREATED, "100").unwrap();
            let mut proposed = Metadata::new();
            proposed.set_u64(repository::CREATED, 100).unwrap();
            let err = validate_read_only_fields(&current, &proposed)
                .expect_err("type change on a read-only key must be rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains(repository::CREATED));
        }
    }
}
