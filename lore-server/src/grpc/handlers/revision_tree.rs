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

use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::util::setup_execution;

#[tracing::instrument(name = "RevisionTree::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionTreeRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RevisionTreeResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let authorization = get_authorization(request.extensions()).ok();
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
    let can_read = link_read_authorizer(authorization);

    LORE_CONTEXT
        .scope(execution, async move {
            tree(repository.clone(), revision, path, max_depth, can_read)
                .await
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

#[cfg(test)]
mod tests {
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_proto::PathType;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::link::LinkFlags;
    use lore_revision::lore::BranchId;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::state;
    use lore_storage::hash::hash_string;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::store::test_store_create;

    #[tokio::test]
    async fn tree_on_file_returns_invalid_argument() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let write_token = get_write_token();
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository.into(),
                ));

                let main = lore_revision::branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    lore_revision::branch::DEFAULT_DEFAULT_NAME,
                    lore_revision::branch::default_category(),
                    "TestCreator",
                    12345,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create main branch");

                // Create a state with a file node at the root
                let state = state::State::new();
                state.set_parent_self(Hash::default());
                state.set_revision_number(1);

                let file_node = Node {
                    flags: NodeFlags::File.bits(),
                    name_hash: hash_string("file.txt"),
                    ..Default::default()
                };
                state
                    .node_add(repository.clone(), ROOT_NODE, file_node, "file.txt")
                    .await
                    .expect("Failed to add file node");

                let revision_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize state");

                branch_push::push(
                    repository.clone(),
                    main,
                    revision_hash,
                    true,
                    true,
                    false,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .expect("Failed to push revision");

                let mut request = Request::new(RevisionTreeRequest {
                    revision: revision_hash.into(),
                    path: "file.txt".to_string(),
                    max_depth: 10,
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let err = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect_err("Expected error for tree on non-directory path");
                assert_eq!(err.code(), tonic::Code::InvalidArgument);
                assert_eq!(
                    err.message(),
                    "Cannot calculate tree for path that is not a directory"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn tree_on_missing_path_returns_not_found() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let write_token = get_write_token();
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository.into(),
                ));

                let main = lore_revision::branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    lore_revision::branch::DEFAULT_DEFAULT_NAME,
                    lore_revision::branch::default_category(),
                    "TestCreator",
                    12345,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create main branch");

                // Create a state with only the root directory (no children)
                let state = state::State::new();
                state.set_parent_self(Hash::default());
                state.set_revision_number(1);

                let revision_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize state");

                branch_push::push(
                    repository.clone(),
                    main,
                    revision_hash,
                    true,
                    true,
                    false,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .expect("Failed to push revision");

                // Request tree for a path that doesn't exist in the state
                let mut request = Request::new(RevisionTreeRequest {
                    revision: revision_hash.into(),
                    path: "nonexistent".to_string(),
                    max_depth: 10,
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let err = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect_err("Expected NotFound for non-existent path");
                assert_eq!(err.code(), tonic::Code::NotFound);
                assert_eq!(err.message(), "A node in the tree could not be found");
            })
            .await;
    }

    #[tokio::test]
    async fn tree_emits_link_node_with_target_repository_context() {
        use lore_base::types::Address;
        use lore_proto::PathType;

        let repository_id = random::<Context>();
        let target_repo = random::<Context>();
        let target_revision = Hash::from(random::<[u8; 32]>());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let write_token = get_write_token();
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository_id.into(),
                ));

                let main = lore_revision::branch::create(
                    repository.clone(),
                    &write_token,
                    Context::from(uuid::Uuid::now_v7()),
                    lore_revision::branch::DEFAULT_DEFAULT_NAME,
                    lore_revision::branch::default_category(),
                    "TestCreator",
                    12345,
                    vec![],
                    false,
                    false,
                )
                .await
                .expect("Could not create main branch");

                let state = state::State::new();
                state.set_parent_self(Hash::default());
                state.set_revision_number(1);

                let link_node = Node {
                    flags: NodeFlags::Link.bits(),
                    child: ROOT_NODE,
                    address: Address {
                        hash: target_revision,
                        context: target_repo,
                    },
                    name_hash: hash_string("linked"),
                    ..Default::default()
                };
                state
                    .node_add(repository.clone(), ROOT_NODE, link_node, "linked")
                    .await
                    .expect("Failed to add link node");

                let revision_hash = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize state");

                branch_push::push(
                    repository.clone(),
                    main,
                    revision_hash,
                    true,
                    true,
                    false,
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                )
                .await
                .expect("Failed to push revision");

                let mut request = Request::new(RevisionTreeRequest {
                    revision: revision_hash.into(),
                    path: String::new(),
                    max_depth: 10,
                });
                request.metadata_mut().insert_bin(
                    REPOSITORY_ID_KEY,
                    tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
                );
                let response = handler(request, immutable_store.clone(), mutable_store.clone())
                    .await
                    .expect("handler ok");
                let paths = response.into_inner().paths;

                assert_eq!(
                    paths.len(),
                    1,
                    "expected exactly the link entry, got {paths:?}"
                );
                let link_path = &paths[0];
                assert_eq!(link_path.path, "linked");
                assert_eq!(link_path.r#type, PathType::Link as i32);
                let address: Address = (&link_path.address).into();
                assert_eq!(
                    address.hash, target_revision,
                    "link.address.hash should be the linked revision signature",
                );
                assert_eq!(
                    address.context, target_repo,
                    "link.address.context should be the target repository id",
                );
            })
            .await;
    }

    /// Push a tree with a single link recording `link_branch` and return the
    /// emitted `Path`.
    async fn link_tree_path(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        repository_id: Context,
        link_branch: BranchId,
    ) -> Path {
        let target_repo = random::<Context>();
        let target_revision = Hash::from(random::<[u8; 32]>());
        let write_token = get_write_token();
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository_id.into(),
        ));

        let main = lore_revision::branch::create(
            repository.clone(),
            &write_token,
            Context::from(uuid::Uuid::now_v7()),
            lore_revision::branch::DEFAULT_DEFAULT_NAME,
            lore_revision::branch::default_category(),
            "TestCreator",
            12345,
            vec![],
            false,
            false,
        )
        .await
        .expect("Could not create main branch");

        let state = state::State::new();
        state.set_parent_self(Hash::default());
        state.set_revision_number(1);

        let link_node = Node {
            flags: NodeFlags::Link.bits(),
            child: ROOT_NODE,
            address: Address {
                hash: target_revision,
                context: target_repo,
            },
            name_hash: hash_string("linked"),
            ..Default::default()
        };
        let link_node_id = state
            .node_add(repository.clone(), ROOT_NODE, link_node, "linked")
            .await
            .expect("Failed to add link node");

        state
            .link_add(
                repository.clone(),
                target_repo.into(),
                link_branch,
                target_revision,
                link_node_id,
                LinkFlags::NoFlags,
            )
            .await
            .expect("Failed to register link reference");

        let revision_hash = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize state");

        branch_push::push(
            repository.clone(),
            main,
            revision_hash,
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push revision");

        let mut request = Request::new(RevisionTreeRequest {
            revision: revision_hash.into(),
            path: String::new(),
            max_depth: 10,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        let response = handler(request, immutable_store, mutable_store)
            .await
            .expect("handler ok");
        let mut paths = response.into_inner().paths;
        assert_eq!(paths.len(), 1, "expected exactly the link entry");
        let link_path = paths.pop().expect("one path");
        assert_eq!(link_path.path, "linked");
        assert_eq!(link_path.r#type, PathType::Link as i32);
        link_path
    }

    /// A zero-branch link reports `tracking = true`.
    #[tokio::test]
    async fn tree_marks_zero_branch_link_as_tracking() {
        let repository_id = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let link_path = link_tree_path(
                    immutable_store,
                    mutable_store,
                    repository_id,
                    BranchId::default(),
                )
                .await;
                assert!(
                    link_path.tracking,
                    "a zero-branch link must be reported as tracking",
                );
            })
            .await;
    }

    /// A link pinned to an explicit branch reports `tracking = false`.
    #[tokio::test]
    async fn tree_marks_pinned_link_as_not_tracking() {
        let repository_id = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let link_path = link_tree_path(
                    immutable_store,
                    mutable_store,
                    repository_id,
                    BranchId::from(uuid::Uuid::now_v7()),
                )
                .await;
                assert!(
                    !link_path.tracking,
                    "a pinned (non-zero branch) link must not be reported as tracking",
                );
            })
            .await;
    }
}
