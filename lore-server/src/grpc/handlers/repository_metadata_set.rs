// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_proto::RepositoryMetadataSetRequest;
use lore_proto::RepositoryMetadataSetResponse;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_revision::metadata::repository::READ_ONLY_KEYS;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_storage::hash;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
use crate::grpc::get_write_token;
use crate::grpc::no_repository_access_status;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

/// Validate that read-only fields have not been changed between the current and proposed
/// metadata blobs.
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

/// Validate that all Address-typed values in the proposed metadata blob reference existing
/// blobs in the immutable store.
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

#[tracing::instrument(name = "RepositoryMetadataSet::handle", skip_all)]
pub async fn handler(
    request: Request<RepositoryMetadataSetRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RepositoryMetadataSetResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let (_, extensions, req) = request.into_parts();

    let repository_id: Context = req.repository_id.into();
    if repository_id == Context::default() {
        return Err(Status::invalid_argument("Missing repository ID"));
    }

    let expected_hash: Hash = req.expected_hash.into();
    let new_hash: Hash = req.new_hash.into();

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id.into(),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            authorizer
                .check_repository_access(
                    get_verified_token(&extensions).as_ref(),
                    repository_id.into(),
                    Action::Write,
                )
                .await
                .map_err(|_err| no_repository_access_status())?;

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

            // Validate binary blob references exist
            validate_binary_blobs(repository.clone(), &proposed_metadata).await?;

            // Perform compare-and-swap
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
                    expected_hash,
                    new_hash,
                    KeyType::RepositoryMetadata,
                )
                .await
                .filter_slow_down()?
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::internal(format!("failed to update metadata: {err}"))
                    })
                })?;

            if previous == expected_hash {
                Ok(Response::new(RepositoryMetadataSetResponse {
                    success: true,
                    current_hash: new_hash.into(),
                }))
            } else {
                Ok(Response::new(RepositoryMetadataSetResponse {
                    success: false,
                    current_hash: previous.into(),
                }))
            }
        })
        .await
}
