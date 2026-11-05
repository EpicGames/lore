// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::Path;
use lore_proto::RevisionTreeRequest;
use lore_proto::RevisionTreeResponse;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::tree;
use lore_revision::util::path::RelativePath;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::REVISION;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::util::setup_execution;

#[tracing::instrument(name = "RevisionTree::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionTreeRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
) -> Result<Response<RevisionTreeResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let can_read = link_read_authorizer(&repository_authorizer, request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let revision = req.revision.into();
    let max_depth = req.max_depth as usize;
    let path = RelativePath::new_from_initial_path(req.path.as_str())
        .map_err(|_err| Status::invalid_argument("path"))?;

    info!(
        { REPOSITORY_ID} = %repository,
        { REVISION } = %revision,
        path = %path,
        max_depth,
        "Handling revision tree",
    );

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // No attribution: this legacy message has no field to carry it.
            tree(
                repository.clone(),
                revision,
                path,
                max_depth,
                can_read,
                false,
            )
            .await
            .filter_slow_down()?
            .map(|result| {
                debug!("Got tree");
                Response::new(RevisionTreeResponse {
                    paths: result
                        .paths
                        .iter()
                        .map(|tree_path| Path {
                            address: tree_path
                                .address
                                .map(|address| address.into())
                                .unwrap_or_default(),
                            path: tree_path.path.to_string(),
                            r#type: super::path_diff::node_flags_to_type(tree_path.flags),
                            tracking: tree_path.tracking,
                        })
                        .collect(),
                })
            })
            .warn_map_err(|e| {
                if e.is_invalid_path() {
                    return Status::invalid_argument(
                        "Cannot calculate tree for path that is not a directory",
                    );
                } else if e.is_node_not_found() {
                    return Status::not_found("A node in the tree could not be found");
                }
                Status::internal(e.to_string())
            })
        })
        .await
}
