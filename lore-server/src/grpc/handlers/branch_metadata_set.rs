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

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::require_action;
use crate::grpc::require_repository_administration;
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

/// Whether `proposed` turns the branch's protection on or off relative to
/// `current`. Such a change is repository administration rather than an
/// ordinary write.
pub(crate) fn changes_protection(current: &Metadata, proposed: &Metadata) -> bool {
    current.get_bool(branch::PROTECT).unwrap_or_default()
        != proposed.get_bool(branch::PROTECT).unwrap_or_default()
}

/// Validate that all Address-typed values in the proposed metadata blob reference existing
/// blobs in the immutable store.
pub(crate) async fn validate_binary_blobs(
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
            .filter_slow_down()?
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
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchMetadataSetResponse>, Status> {
    require_action(authorizer, &request, Action::Write).await?;
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let extensions = request.extensions().clone();
    let req = request.into_inner();

    let branch_id = BranchId::from(req.branch_id);
    if branch_id == BranchId::default() {
        return Err(Status::invalid_argument("Missing branch ID"));
    }

    let expected_hash: Hash = req.expected_hash.into();
    let new_hash: Hash = req.new_hash.into();

    let execution = setup_execution(module_path!(), correlation_id, user_id.clone());
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
                .filter_slow_down()?
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::invalid_argument(format!(
                            "failed to deserialize proposed metadata: {err}"
                        ))
                    })
                })?;

            // Validate read-only fields are unchanged
            validate_read_only_fields(&current_metadata, &proposed_metadata)?;
            if changes_protection(&current_metadata, &proposed_metadata) {
                require_repository_administration(
                    authorizer,
                    &extensions,
                    repository.clone(),
                    &user_id,
                )
                .await?;
            }

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
                .filter_slow_down()?
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
