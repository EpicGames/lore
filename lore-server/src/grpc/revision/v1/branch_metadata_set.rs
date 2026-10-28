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

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::handlers::branch_metadata_set::changes_protection;
use crate::grpc::handlers::branch_metadata_set::validate_binary_blobs;
use crate::grpc::handlers::branch_metadata_set::validate_read_only_fields;
use crate::grpc::require_action;
use crate::grpc::require_repository_administration;
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
/// protection without dedicated protect/unprotect RPCs. Changing it needs
/// the same rights as those RPCs; comparing against the caller's `expected`
/// is sound because the CAS succeeds only when `expected` is the stored
/// pointer.
#[tracing::instrument(name = "BranchMetadataSet::v1::handle", skip_all)]
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

    let branch_id = BranchId::from(req.id);
    if branch_id == BranchId::default() {
        return Err(Status::invalid_argument("Branch id must be non-zero"));
    }

    let expected: Hash = req.expected.into();
    let updated: Hash = req.updated.into();

    let execution = setup_execution(module_path!(), correlation_id, user_id.clone());
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
                .filter_slow_down()?
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
                    .filter_slow_down()?
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
                .filter_slow_down()?
                .map_err(|err| {
                    warn_error_to_status(&err, |err| {
                        Status::invalid_argument(format!(
                            "failed to deserialize updated metadata: {err}"
                        ))
                    })
                })?;

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
                .filter_slow_down()?
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
