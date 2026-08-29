// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::BranchDiffRequest;
use lore_proto::BranchDiffResponse;
use lore_proto::PathDiff;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::handlers::path_diff::link_pin_path_diffs;
use crate::grpc::handlers::path_diff::map_to_conflict;
use crate::grpc::handlers::path_diff::map_to_path_diff;
use crate::grpc::link_read_authorizer;
use crate::util::setup_execution;

#[tracing::instrument(name = "BranchDiff::handle", skip_all)]
pub async fn handler(
    request: Request<BranchDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<BranchDiffResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let authorization = get_authorization(request.extensions()).ok();
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner().clone();
    let branch_source = BranchId::from(req.branch_source);
    let branch_target = BranchId::from(req.branch_target);
    let revision_source = req.revision_source.map(Hash::from);
    let revision_target = req.revision_target.map(Hash::from);
    let auto_resolve = req.autoresolve;

    info!(
        "Handling branch diff in repository {repository_id} source: {branch_source} target {branch_target}"
    );

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(
        RepositoryContext::new_server_context(immutable_store, mutable_store, repository_id)
            .with_link_read(link_read_authorizer(authorization)),
    );
    LORE_CONTEXT
        .scope(execution, async move {
            branch_diff_handler(
                repository,
                branch_source,
                revision_source,
                branch_target,
                revision_target,
                auto_resolve,
            )
            .await
        })
        .await
}

/// Loads the two states a pin comparison needs. A failure fails the diff for
/// the same reason [`link_pin_path_diffs`] does.
async fn link_pin_diffs(
    repository: &Arc<RepositoryContext>,
    from: Hash,
    to: Hash,
    parent_repository_id: RepositoryId,
) -> Result<Vec<PathDiff>, Status> {
    if from.is_zero() || to.is_zero() || from == to {
        return Ok(Vec::new());
    }
    let states = async {
        let state_from = State::deserialize(repository.clone(), from).await?;
        let state_to = State::deserialize(repository.clone(), to).await?;
        Ok::<_, StateError>((state_from, state_to))
    }
    .await
    .filter_slow_down()?;
    let (state_from, state_to) = states.map_err(|err| {
        warn!(
            {REPOSITORY_ID} = %repository.id, %from, %to, ?err,
            "Failed to load states for link pin comparison",
        );
        Status::internal(err.to_string())
    })?;
    link_pin_path_diffs(repository, &state_from, &state_to, parent_repository_id).await
}

async fn branch_diff_handler(
    repository: Arc<RepositoryContext>,
    branch_source: BranchId,
    revision_source: Option<Hash>,
    branch_target: BranchId,
    revision_target: Option<Hash>,
    auto_resolve: bool,
) -> Result<Response<BranchDiffResponse>, Status> {
    let metadata = branch::metadata(repository.clone(), branch_source)
        .await
        .map_err(|err| {
            warn!("Failed to get source branch metadata: {branch_source}");
            Status::not_found(err.to_string())
        })?;
    let source = branch::branch_metadata(repository.clone(), branch_source, &metadata)
        .await
        .map_err(|e| {
            warn!("Failed to resolve source branch: {branch_source}");
            Status::not_found(e.to_string())
        })?;

    let metadata = branch::metadata(repository.clone(), branch_target)
        .await
        .map_err(|err| {
            warn!("Failed to get target branch metadata: {branch_target}");
            Status::not_found(err.to_string())
        })?;
    let target = branch::branch_metadata(repository.clone(), branch_target, &metadata)
        .await
        .map_err(|e| {
            warn!("Failed to resolve target branch: {branch_target}");
            Status::not_found(e.to_string())
        })?;

    let repository_id = repository.id;
    let link_repository = repository.clone();
    let result = branch::diff3_collect(
        repository,
        branch_source,
        revision_source.unwrap_or(source.latest),
        branch_target,
        revision_target.unwrap_or(target.latest),
        None,  /* No path */
        false, /* Do not include identical changes */
        auto_resolve,
    )
    .await;
    match result {
        Ok(result) => {
            debug!("Found {} changes", result.changes.len());
            // The changes are base -> source; compare the registries over the
            // same pair.
            let pin_diffs =
                link_pin_diffs(&link_repository, result.base, result.source, repository_id).await?;

            let mut diffs = Vec::with_capacity(result.changes.len() + pin_diffs.len());
            diffs.extend(pin_diffs);
            for change in &result.changes {
                if let Some(diff) = map_to_path_diff(change, repository_id).await {
                    diffs.push(diff);
                }
            }
            let mut conflicts = Vec::with_capacity(result.conflicts.len());
            for conflict in &result.conflicts {
                if let Some(conflict) = map_to_conflict(conflict, repository_id).await {
                    conflicts.push(conflict);
                }
            }
            Ok(Response::new(BranchDiffResponse {
                diffs,
                conflicts,
                branch_source: Some(source.into()),
                branch_target: Some(target.into()),
                revision_source: result.source.into(),
                revision_target: result.target.into(),
                revision_base: result.base.into(),
            }))
        }
        Err(err) => {
            warn!({REPOSITORY_ID} = %repository_id, %branch_source, %branch_target, ?err, "Failed to calculate diff");
            if err.is_divergent() || err.is_invalid_arguments() {
                Err(Status::invalid_argument(err.to_string()))
            } else if err.is_max_history_search_depth() {
                Err(Status::resource_exhausted(err.to_string()))
            } else {
                Err(Status::internal(err.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod test {
    use lore_base::types::BranchPoint;
    use lore_base::types::Context;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::branch::MAX_DIVERGENT_HISTORY_LENGTH;
    use lore_revision::metadata;
    use lore_revision::state;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::store::test_store_create;

    async fn commit_revision_on_branch(
        repository_context: Arc<RepositoryContext>,
        branch: BranchId,
        parent: Hash,
        revision_number: u64,
    ) -> Hash {
        let write_token = get_write_token();
        let state = Arc::new(state::State::new());
        state.set_parent_self(parent);
        state.set_revision_number(revision_number);

        let mut metadata = metadata::Metadata::new();
        metadata.set_branch(branch).expect("Failed to set branch");
        let metadata = metadata
            .serialize(repository_context.clone())
            .await
            .expect("Failed to serialize metadata");
        state.set_metadata_hash(metadata);

        state
            .serialize(repository_context.clone(), &write_token)
            .await
            .expect("Failed to serialize state")
    }

    async fn push_revision_on_branch(
        repository_context: Arc<RepositoryContext>,
        branch: BranchId,
        parent: Hash,
        revision_number: u64,
    ) -> Hash {
        let write_token = get_write_token();
        let state = Arc::new(state::State::new());
        state.set_parent_self(parent);
        state.set_revision_number(revision_number);

        let mut metadata = metadata::Metadata::new();
        metadata.set_branch(branch).expect("Failed to set branch");
        let metadata = metadata
            .serialize(repository_context.clone())
            .await
            .expect("Failed to serialize metadata");
        state.set_metadata_hash(metadata);

        let state_hash = state
            .serialize(repository_context.clone(), &write_token)
            .await
            .expect("Failed to serialize state");

        branch_push::push(
            repository_context.clone(),
            branch,
            state_hash,
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("Failed to push head revision")
        .revision
    }

    async fn create_test_main(repository_context: Arc<RepositoryContext>) -> (BranchId, Hash) {
        let write_token = get_write_token();
        let main = lore_revision::branch::create(
            repository_context.clone(),
            &write_token,
            BranchId::from(uuid::Uuid::now_v7()),
            branch::DEFAULT_DEFAULT_NAME,
            branch::default_category(),
            "test-creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("Could not create main branch");
        let head =
            push_revision_on_branch(repository_context.clone(), main, Hash::default(), 1).await;

        (main, head)
    }

    async fn create_branch(
        repository_context: Arc<RepositoryContext>,
        name: &str,
        branch_stack: Vec<BranchPoint>,
    ) -> BranchId {
        let write_token = get_write_token();
        let branch = BranchId::from(uuid::Uuid::now_v7());
        lore_revision::branch::create(
            repository_context.clone(),
            &write_token,
            branch,
            name,
            branch::personal_category(),
            "BranchCreator",
            12345,
            branch_stack,
            false,
            false,
        )
        .await
        .expect("Could not create test branch");
        branch
    }

    /*
       (main parent)  X             Y  (branch A latest)
                      |             |
                      |            / (branch A)
                      |           /
        (main branch) |      X---/ (diverged parent, branch point)
                      |      |
                      |     / (main branch)
                      |    /
    (common ancestor) X---/
                      |
                      .
    */
    #[tokio::test]
    async fn divergence_returns_ok_for_exceeded_max_search_depth() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let (response, expected) = Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));

            let (
                main_branch,
                main_latest_revision,
                _main_latest_revision_number,
                branch,
                branch_latest_revision,
                _branch_latest_revision_number,
                branch_point_revision,
                divergence_point_revision,
            ) = {
                let (main_branch, revision_1) = create_test_main(repository_context.clone()).await;

                let mut last_revision = revision_1;
                let mut last_revision_number = 0;

                // Initial revisions on main
                for revision_number in 2..4 {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        main_branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                let divergence_point_revision = last_revision;
                let divergence_point_revision_number = last_revision_number;

                // create main sizeable history
                for revision_number in (last_revision_number as usize + 1)
                    ..(last_revision_number as usize + MAX_DIVERGENT_HISTORY_LENGTH + 20)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        main_branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                let main_latest_revision = last_revision;
                let main_latest_revision_number = last_revision_number;

                last_revision = divergence_point_revision;

                // Go back and create divergent history. Divergence caused by induced revision
                // number offset causing signature hash difference
                for revision_number in (divergence_point_revision_number as usize + 10)
                    ..(divergence_point_revision_number as usize
                        + MAX_DIVERGENT_HISTORY_LENGTH
                        + 20)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = commit_revision_on_branch(
                        repository_context.clone(),
                        main_branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                // branch after this sizeable divergent history
                let branch_point_revision = last_revision;
                let _branch_point_revision_number = last_revision_number;

                // branch A is a child of main, branched from somewhere between the start and the end
                // on a divergent line of history compared to the pushed main branch latest
                let branch = create_branch(
                    repository_context.clone(),
                    "branch_a",
                    vec![BranchPoint {
                        branch: main_branch,
                        revision: branch_point_revision,
                    }],
                )
                .await;

                // then create some more history that we will diff against
                for revision_number in
                    (last_revision_number as usize + 1)..(last_revision_number as usize + 10)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                (
                    main_branch,
                    main_latest_revision,
                    main_latest_revision_number,
                    branch,
                    last_revision,
                    last_revision_number,
                    branch_point_revision,
                    divergence_point_revision,
                )
            };

            let mut request = Request::new(BranchDiffRequest {
                branch_target: main_branch.into(),
                branch_source: branch.into(),
                revision_target: Some(main_latest_revision.into()),
                revision_source: Some(branch_latest_revision.into()),
                autoresolve: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            let response = handler(request, immutable_store, mutable_store).await;
            let expected = BranchDiffResponse {
                diffs: vec![],
                conflicts: vec![],
                branch_target: Some(lore_proto::model::Branch {
                    id: main_branch.into(),
                    name: "main".to_string(),
                    parent_deprecated: Some(Context::default().into()),
                    latest: main_latest_revision.into(),
                    branch_point_deprecated: Some(Hash::default().into()),
                    creator: "test-creator".to_string(),
                    created: 1,
                    category: "".to_string(),
                    stack: vec![],
                }),
                branch_source: Some(lore_proto::model::Branch {
                    id: branch.into(),
                    name: "branch_a".to_string(),
                    parent_deprecated: Some(main_branch.into()),
                    latest: branch_latest_revision.into(),
                    branch_point_deprecated: Some(branch_point_revision.into()),
                    creator: "BranchCreator".to_string(),
                    created: 12345,
                    category: "personal".to_string(),
                    stack: vec![lore_proto::model::BranchPoint {
                        branch: main_branch.into(),
                        revision: branch_point_revision.into(),
                    }],
                }),
                revision_target: main_latest_revision.into(),
                revision_source: branch_latest_revision.into(),
                revision_base: divergence_point_revision.into(),
            };

            (response, expected)
        }))
        .await;

        let response = response.expect("Expected ok response").into_inner();

        assert_eq!(
            response, expected,
            "Branch diff identifies the divergence point as base revision when source and target are on divergent chains of the parent branch"
        );
    }

    /*
                       (main latest)  X
                                      |             X (branch A latest)
             (branch B latest) X      |             |
                                \     |             |
                      (branch B) \    |            / (branch A)
                                  \---X           /
                        (main branch) |      X---/ (diverged parent, branch point)
                                      |      |
                                      |     / (main branch)
                                      |    /
                    (common ancestor) X---/
                                      |
                                      .
    */
    #[tokio::test]
    async fn two_branch_divergence_returns_ok_for_exceeded_max_search_depth() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let (response, expected) = Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));

            let (
                main_branch,
                _main_latest_revision,
                _main_latest_revision_number,
                branch_a,
                branch_a_latest_revision,
                _branch_a_latest_revision_number,
                branch_a_point_revision,
                branch_b,
                branch_b_latest_revision,
                _branch_b_latest_revision_number,
                branch_b_point_revision,
                divergence_point_revision,
            ) = {
                let (main_branch, revision_1) = create_test_main(repository_context.clone()).await;

                let mut last_revision = revision_1;
                let mut last_revision_number = 0;

                // Initial revisions on main
                for revision_number in 2..4 {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        main_branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                let divergence_point_revision = last_revision;
                let divergence_point_revision_number = last_revision_number;

                // create main sizeable history
                for revision_number in (last_revision_number as usize + 1)
                    ..(last_revision_number as usize + MAX_DIVERGENT_HISTORY_LENGTH + 50)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        main_branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                let branch_b_point_revision = last_revision;
                let branch_b_point_revision_number = last_revision_number;

                // more main history
                for revision_number in (last_revision_number as usize + 1)
                    ..(last_revision_number as usize + MAX_DIVERGENT_HISTORY_LENGTH + 20)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        main_branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                let main_latest_revision = last_revision;
                let main_latest_revision_number = last_revision_number;

                last_revision = divergence_point_revision;

                // Go back and create divergent history. Divergence caused by induced revision
                // number offset causing signature hash difference
                for revision_number in (divergence_point_revision_number as usize + 10)
                    ..(divergence_point_revision_number as usize
                        + MAX_DIVERGENT_HISTORY_LENGTH
                        + 20)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = commit_revision_on_branch(
                        repository_context.clone(),
                        main_branch,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                // branch after this sizeable divergent history
                let branch_a_point_revision = last_revision;
                let _branch_a_point_revision_number = last_revision_number;

                // branch A is a child of main, branched from somewhere between the start and the end
                // on a divergent line of history compared to the pushed main branch latest
                let branch_a = create_branch(
                    repository_context.clone(),
                    "branch_a",
                    vec![BranchPoint {
                        branch: main_branch,
                        revision: branch_a_point_revision,
                    }],
                )
                .await;

                last_revision = branch_a_point_revision;

                // then create some more history on branch A that we will diff against
                for revision_number in
                    (last_revision_number as usize + 1)..(last_revision_number as usize + 10)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        branch_a,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                let branch_a_latest_revision = last_revision;
                let branch_a_latest_revision_number = last_revision_number;

                // branch B is a child of main, branched from convergent history on main
                let branch_b = create_branch(
                    repository_context.clone(),
                    "branch_b",
                    vec![BranchPoint {
                        branch: main_branch,
                        revision: branch_b_point_revision,
                    }],
                )
                .await;

                last_revision = branch_b_point_revision;

                // then create some more history on branch B that we will diff against
                for revision_number in (branch_b_point_revision_number as usize + 1)
                    ..(branch_b_point_revision_number as usize + 10)
                {
                    last_revision_number = revision_number as u64;
                    last_revision = push_revision_on_branch(
                        repository_context.clone(),
                        branch_b,
                        last_revision,
                        last_revision_number,
                    )
                    .await;
                }

                let branch_b_latest_revision = last_revision;
                let branch_b_latest_revision_number = last_revision_number;

                (
                    main_branch,
                    main_latest_revision,
                    main_latest_revision_number,
                    branch_a,
                    branch_a_latest_revision,
                    branch_a_latest_revision_number,
                    branch_a_point_revision,
                    branch_b,
                    branch_b_latest_revision,
                    branch_b_latest_revision_number,
                    branch_b_point_revision,
                    divergence_point_revision,
                )
            };

            let mut request = Request::new(BranchDiffRequest {
                branch_target: branch_b.into(),
                branch_source: branch_a.into(),
                revision_target: Some(branch_b_latest_revision.into()),
                revision_source: Some(branch_a_latest_revision.into()),
                autoresolve: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            let response = handler(request, immutable_store, mutable_store).await;
            let expected = BranchDiffResponse {
                diffs: vec![],
                conflicts: vec![],
                branch_target: Some(lore_proto::model::Branch {
                    id: branch_b.into(),
                    name: "branch_b".to_string(),
                    parent_deprecated: Some(main_branch.into()),
                    latest: branch_b_latest_revision.into(),
                    branch_point_deprecated: Some(branch_b_point_revision.into()),
                    creator: "BranchCreator".to_string(),
                    created: 12345,
                    category: "personal".to_string(),
                    stack: vec![lore_proto::model::BranchPoint {
                        branch: main_branch.into(),
                        revision: branch_b_point_revision.into(),
                    }],
                }),
                branch_source: Some(lore_proto::model::Branch {
                    id: branch_a.into(),
                    name: "branch_a".to_string(),
                    parent_deprecated: Some(main_branch.into()),
                    latest: branch_a_latest_revision.into(),
                    branch_point_deprecated: Some(branch_a_point_revision.into()),
                    creator: "BranchCreator".to_string(),
                    created: 12345,
                    category: "personal".to_string(),
                    stack: vec![lore_proto::model::BranchPoint {
                        branch: main_branch.into(),
                        revision: branch_a_point_revision.into(),
                    }],
                }),
                revision_target: branch_b_latest_revision.into(),
                revision_source: branch_a_latest_revision.into(),
                revision_base: divergence_point_revision.into(),
            };

            (response, expected)
        }))
        .await;

        let response = response.expect("Expected ok response").into_inner();

        assert_eq!(
            response, expected,
            "Branch diff identifies the divergence point as base revision when source and target are on divergent chains of the parent branch"
        );
    }

    /// Every branch is created from another branch, so two branch stacks always
    /// share an entry - the default branch at the latest. Stacks that share none
    /// are an invalid branch configuration, and the diff has to say so rather than
    /// resolve a base from a search it has no starting point for.
    #[tokio::test]
    async fn no_shared_branch_in_stacks_is_rejected() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let status = Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));

            let (_main_branch, main_revision) = create_test_main(repository_context.clone()).await;

            // Both stacks name a branch the other side does not carry.
            let source_branch = create_branch(
                repository_context.clone(),
                "branch_source",
                vec![BranchPoint {
                    branch: BranchId::from(uuid::Uuid::now_v7()),
                    revision: main_revision,
                }],
            )
            .await;
            let target_branch = create_branch(
                repository_context.clone(),
                "branch_target",
                vec![BranchPoint {
                    branch: BranchId::from(uuid::Uuid::now_v7()),
                    revision: main_revision,
                }],
            )
            .await;

            let source_revision = push_revision_on_branch(
                repository_context.clone(),
                source_branch,
                main_revision,
                2,
            )
            .await;
            let target_revision = push_revision_on_branch(
                repository_context.clone(),
                target_branch,
                main_revision,
                2,
            )
            .await;

            let mut request = Request::new(BranchDiffRequest {
                branch_target: target_branch.into(),
                branch_source: source_branch.into(),
                revision_target: Some(target_revision.into()),
                revision_source: Some(source_revision.into()),
                autoresolve: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            handler(request, immutable_store, mutable_store).await.err()
        }))
        .await;

        let status = status.expect("Branch diff must refuse stacks that share no branch");
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "An unusable branch configuration is the caller's argument, not a server fault, got {status:?}"
        );
        assert!(
            status
                .message()
                .contains("no common branch in their branch stacks"),
            "The status must name the branch configuration as the cause, got {:?}",
            status.message()
        );
    }

    /*
        (main latest)  X            X (branch B latest)
                       |            |
                       |           / (branch B)
                       |          /
         (main branch) |     X---/ (branch point, on an unrelated root)
                       |     |
                       X     X (no shared revision)
                       |     |
                       .     .
    */
    /// Two branch points on the same branch that share no revision leave both
    /// searches with nothing to find, so the older branch point is used. It is a
    /// guess rather than an ancestor, and it is still the answer: the branches
    /// were created from it, and the alternative - the zero revision - is the
    /// empty tree, which reports every path in both branches as an add.
    #[tokio::test]
    async fn disjoint_histories_fall_back_to_the_older_branch_point() {
        let repository = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let (base, branch_point) = Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));

            let (main_branch, revision_1) = create_test_main(repository_context.clone()).await;

            let mut main_latest_revision = revision_1;
            for revision_number in 2..4 {
                main_latest_revision = push_revision_on_branch(
                    repository_context.clone(),
                    main_branch,
                    main_latest_revision,
                    revision_number,
                )
                .await;
            }

            // A second root chain on the same branch. The offset revision numbers
            // keep every signature distinct from the pushed chain, so the two share
            // no revision at all.
            let mut orphan_revision = commit_revision_on_branch(
                repository_context.clone(),
                main_branch,
                Hash::default(),
                11,
            )
            .await;
            orphan_revision = commit_revision_on_branch(
                repository_context.clone(),
                main_branch,
                orphan_revision,
                12,
            )
            .await;

            let source_branch = create_branch(
                repository_context.clone(),
                "branch_source",
                vec![BranchPoint {
                    branch: main_branch,
                    revision: main_latest_revision,
                }],
            )
            .await;
            let target_branch = create_branch(
                repository_context.clone(),
                "branch_target",
                vec![BranchPoint {
                    branch: main_branch,
                    revision: orphan_revision,
                }],
            )
            .await;

            let source_revision = push_revision_on_branch(
                repository_context.clone(),
                source_branch,
                main_latest_revision,
                4,
            )
            .await;
            let target_revision = push_revision_on_branch(
                repository_context.clone(),
                target_branch,
                orphan_revision,
                13,
            )
            .await;

            let mut request = Request::new(BranchDiffRequest {
                branch_target: target_branch.into(),
                branch_source: source_branch.into(),
                revision_target: Some(target_revision.into()),
                revision_source: Some(source_revision.into()),
                autoresolve: false,
            });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let response = handler(request, immutable_store, mutable_store)
                .await
                .expect("Branch diff must resolve a base for histories that share no revision")
                .into_inner();

            (Hash::from(response.revision_base), main_latest_revision)
        }))
        .await;

        assert_eq!(
            base, branch_point,
            "The older of the two branch points is the best answer left once both searches come up empty"
        );
        assert!(
            !base.is_zero(),
            "A zero base is the empty tree, which conflicts on every path in both branches"
        );
    }
}
