// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Atomic, server-authored branch merges for the revision v1 API.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::lore::revision::v1 as revision_v1;
use lore_proto::lore::revision::v1::branch_merge_resolution::Resolution;
use lore_revision::branch;
use lore_revision::branch::Diff3Options;
use lore_revision::merge_resolution::ConflictSide;
use lore_revision::merge_resolution::MergePlan;
use lore_revision::metadata;
use lore_revision::metadata::Metadata;
use lore_revision::notification::NotificationSender;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::State;
use lore_telemetry::InstrumentProvider;
use prost::Message;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::branch_push::publish_revision;
use super::idempotency;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::handlers::branch_push::extract_client_ip;
use crate::grpc::server::RevisionListAcceleration;
use crate::hooks::HookDispatcher;
use crate::util::setup_execution;

const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESOLUTIONS: usize = 100_000;
const MAX_SOURCE_CHANGES: usize = 100_000;

enum Attempt {
    Published(revision_v1::BranchMergeResponse),
    NotPublished(revision_v1::BranchMergeResponse),
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "BranchMerge::v1::handle", skip_all)]
pub async fn handler(
    request: Request<revision_v1::BranchMergeRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    history_step_size: u64,
    acceleration: RevisionListAcceleration,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<revision_v1::BranchMergeResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let user_id = get_user_id(request.extensions());
    let bypass_protection = get_authorization(request.extensions())
        .ok()
        .and_then(|token| token.is_service_account)
        .unwrap_or_default();
    let client_ip = extract_client_ip(&request).map(|value| value.to_string());
    let req = request.into_inner();

    if req.encoded_len() > MAX_REQUEST_BYTES {
        return Err(Status::resource_exhausted(format!(
            "BranchMerge encoded request byte limit exceeded: {} > {MAX_REQUEST_BYTES}",
            req.encoded_len()
        )));
    }
    if req.resolutions.len() > MAX_RESOLUTIONS {
        return Err(Status::resource_exhausted(format!(
            "BranchMerge resolution count limit exceeded: {} > {MAX_RESOLUTIONS}",
            req.resolutions.len()
        )));
    }

    let request_id = Context::from(uuid_v7(req.request_id.as_ref(), "request_id")?);
    let target_branch = branch_id(req.branch_id_target.as_ref(), "branch_id_target")?;
    let source_branch = branch_id(req.branch_id_source.as_ref(), "branch_id_source")?;
    if target_branch == source_branch {
        return Err(Status::invalid_argument(
            "branch_id_source and branch_id_target must differ",
        ));
    }
    let expected_target = nonzero_hash(
        req.revision_signature_target.as_ref(),
        "revision_signature_target",
    )?;
    let expected_source = nonzero_hash(
        req.revision_signature_source.as_ref(),
        "revision_signature_source",
    )?;
    let resolutions = parse_resolutions(&req.resolutions)?;
    let request_digest = Hash::hash_buffer(req.encode_to_vec().as_slice());

    let reservation = match idempotency::begin(
        "BranchMerge",
        immutable_store.clone(),
        mutable_store.clone(),
        repository_id,
        target_branch,
        request_id,
        request_digest,
    )
    .await?
    {
        idempotency::Start::Completed {
            revision,
            revision_number,
        } => {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                repository_id,
            ));
            let base = resolve_base(
                repository,
                source_branch,
                expected_source,
                target_branch,
                expected_target,
            )
            .await?;
            return Ok(Response::new(revision_v1::BranchMergeResponse {
                outcome: revision_v1::BranchMergeOutcome::Merged as i32,
                revision_signature: revision.into(),
                revision_number,
                revision_signature_base: base.into(),
                unresolved_conflicts: Vec::new(),
            }));
        }
        idempotency::Start::Reserved(reservation) => reservation,
    };

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store.clone(),
        mutable_store.clone(),
        repository_id,
    ));
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());
    let outcome = LORE_CONTEXT
        .scope(execution, async {
            ensure_tip(repository.clone(), target_branch, expected_target, "target").await?;
            ensure_tip(repository.clone(), source_branch, expected_source, "source").await?;

            let diff = branch::diff3_collect_with_options(
                repository.clone(),
                source_branch,
                expected_source,
                target_branch,
                expected_target,
                Diff3Options {
                    auto_resolve: false,
                    source_cap: Some(MAX_SOURCE_CHANGES),
                    ..Default::default()
                },
            )
            .await
            .map_err(branch_error)?;
            if diff.base.is_zero() {
                return Err(Status::failed_precondition(
                    "BranchMerge: source and target have no common ancestor",
                ));
            }

            let plan = MergePlan::new(diff);
            let resolved = plan.resolve(&resolutions);
            if !resolved.unknown_resolution_ids.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "resolution contains {} conflict id(s) that do not belong to this merge",
                    resolved.unknown_resolution_ids.len()
                )));
            }

            let target_state = load_state(repository.clone(), expected_target, "target").await?;
            if !resolved.unresolved.is_empty() {
                return Ok(Attempt::NotPublished(revision_v1::BranchMergeResponse {
                    outcome: revision_v1::BranchMergeOutcome::Conflicted as i32,
                    revision_signature: expected_target.into(),
                    revision_number: target_state.revision_number(),
                    revision_signature_base: plan.base.into(),
                    unresolved_conflicts: resolved
                        .unresolved
                        .iter()
                        .map(|conflict| revision_v1::BranchMergeConflict {
                            conflict_id: conflict.id.into(),
                            path_source: conflict.source.path.to_string(),
                            path_target: conflict.target.path.to_string(),
                        })
                        .collect(),
                }));
            }

            if plan.changes.is_empty() && plan.conflicts.is_empty() {
                return Ok(Attempt::NotPublished(revision_v1::BranchMergeResponse {
                    outcome: revision_v1::BranchMergeOutcome::AlreadyUpToDate as i32,
                    revision_signature: expected_target.into(),
                    revision_number: target_state.revision_number(),
                    revision_signature_base: plan.base.into(),
                    unresolved_conflicts: Vec::new(),
                }));
            }

            // A source branch can advance independently while the immutable
            // merge plan is being computed. Honor both optimistic guards at
            // the final publication boundary.
            ensure_tip(repository.clone(), source_branch, expected_source, "source").await?;
            ensure_tip(repository.clone(), target_branch, expected_target, "target").await?;

            let deleted = state::apply_tree_changes_for_commit(
                repository.clone(),
                target_state.clone(),
                &resolved.changes,
            )
            .await
            .map_err(|error| {
                Status::internal(format!("failed to apply merge tree changes: {error}"))
            })?;

            let mut pending_metadata = if target_state.metadata_hash().is_zero() {
                Metadata::new()
            } else {
                Metadata::deserialize(repository.clone(), target_state.metadata_hash())
                    .await
                    .map_err(|error| {
                        Status::internal(format!("failed to load target metadata: {error}"))
                    })?
            };
            pending_metadata
                .set_string(
                    metadata::MERGED_BY,
                    if user_id.is_empty() {
                        "server"
                    } else {
                        &user_id
                    },
                )
                .map_err(|error| {
                    Status::internal(format!("failed to set merge metadata: {error}"))
                })?;
            pending_metadata
                .set_string(metadata::MESSAGE, &req.commit_message)
                .map_err(|error| {
                    Status::internal(format!("failed to set merge message: {error}"))
                })?;
            let revision = lore_revision::commit::construct_merge_revision(
                repository.clone(),
                &get_write_token(),
                target_state,
                pending_metadata,
                target_branch,
                expected_source,
                deleted,
            )
            .await
            .map_err(|error| {
                Status::internal(format!("failed to construct merge revision: {error}"))
            })?;
            let published = publish_revision(
                repository,
                target_branch,
                revision,
                bypass_protection,
                false,
                false,
                client_ip,
                correlation_id,
                user_id,
                notification,
                hook_dispatcher,
                history_step_size,
                acceleration,
                instrument_provider,
            )
            .await?
            .into_inner();

            Ok(Attempt::Published(revision_v1::BranchMergeResponse {
                outcome: revision_v1::BranchMergeOutcome::Merged as i32,
                revision_signature: published.revision_signature,
                revision_number: published.revision_number,
                revision_signature_base: plan.base.into(),
                unresolved_conflicts: Vec::new(),
            }))
        })
        .await;

    match outcome {
        Ok(Attempt::Published(response)) => {
            idempotency::complete(
                immutable_store,
                mutable_store,
                repository_id,
                &reservation,
                nonzero_hash(response.revision_signature.as_ref(), "published revision")?,
                response.revision_number,
            )
            .await?;
            Ok(Response::new(response))
        }
        Ok(Attempt::NotPublished(response)) => {
            idempotency::release(mutable_store, repository_id, &reservation).await;
            Ok(Response::new(response))
        }
        Err(status) => {
            idempotency::release(mutable_store, repository_id, &reservation).await;
            Err(status)
        }
    }
}

fn parse_resolutions(
    values: &[revision_v1::BranchMergeResolution],
) -> Result<BTreeMap<Hash, ConflictSide>, Status> {
    let mut result = BTreeMap::new();
    let mut duplicate_ids = BTreeSet::new();
    for value in values {
        let id = nonzero_hash(value.conflict_id.as_ref(), "resolution.conflict_id")?;
        let side = match value.resolution {
            Some(Resolution::Side(value)) => match revision_v1::BranchMergeSide::try_from(value) {
                Ok(revision_v1::BranchMergeSide::Target) => ConflictSide::Target,
                Ok(revision_v1::BranchMergeSide::Source) => ConflictSide::Source,
                _ => {
                    return Err(Status::invalid_argument(
                        "resolution.side must be TARGET or SOURCE",
                    ));
                }
            },
            None => {
                return Err(Status::invalid_argument(
                    "BranchMergeResolution.resolution must be set",
                ));
            }
        };
        if result.insert(id, side).is_some() {
            duplicate_ids.insert(id);
        }
    }
    if !duplicate_ids.is_empty() {
        return Err(Status::invalid_argument(format!(
            "duplicate resolution for {} conflict id(s)",
            duplicate_ids.len()
        )));
    }
    Ok(result)
}

async fn ensure_tip(
    repository: Arc<RepositoryContext>,
    branch_id: BranchId,
    expected: Hash,
    label: &str,
) -> Result<(), Status> {
    let current = branch::load_latest(repository, branch_id)
        .await
        .map_err(|error| Status::not_found(format!("{label} branch was not found: {error}")))?;
    if current != expected {
        return Err(Status::failed_precondition(format!(
            "{label} branch advanced; current latest: {current}"
        )));
    }
    Ok(())
}

async fn resolve_base(
    repository: Arc<RepositoryContext>,
    source_branch: BranchId,
    source: Hash,
    target_branch: BranchId,
    target: Hash,
) -> Result<Hash, Status> {
    let base = branch::resolve_diff3_base(repository, source_branch, source, target_branch, target)
        .await
        .map_err(branch_error)?;
    if base.is_zero() {
        return Err(Status::failed_precondition(
            "BranchMerge: source and target have no common ancestor",
        ));
    }
    Ok(base)
}

async fn load_state(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    label: &str,
) -> Result<Arc<State>, Status> {
    State::deserialize(repository, revision)
        .await
        .map_err(|error| {
            if error.is_address_not_found() || error.is_payload_not_found() || error.is_not_found()
            {
                Status::not_found(format!("{label} revision was not found: {error}"))
            } else {
                Status::internal(format!("failed to load {label} revision: {error}"))
            }
        })
}

fn branch_error(error: branch::BranchError) -> Status {
    if error.is_divergent() {
        Status::failed_precondition(error.to_string())
    } else if error.is_max_history_search_depth() || error.is_oversized() {
        Status::resource_exhausted(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}

fn uuid_v7(bytes: &[u8], field: &str) -> Result<uuid::Uuid, Status> {
    let value = uuid::Uuid::from_slice(bytes)
        .map_err(|_| Status::invalid_argument(format!("{field} must be exactly 16 UUID bytes")))?;
    if value.get_version_num() != 7 {
        return Err(Status::invalid_argument(format!(
            "{field} must be a UUIDv7"
        )));
    }
    Ok(value)
}

fn branch_id(bytes: &[u8], field: &str) -> Result<BranchId, Status> {
    if bytes.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "{field} must be exactly 16 bytes"
        )));
    }
    let value = BranchId::from(bytes);
    if value.is_zero() {
        return Err(Status::invalid_argument(format!(
            "{field} must be non-zero"
        )));
    }
    Ok(value)
}

fn nonzero_hash(bytes: &[u8], field: &str) -> Result<Hash, Status> {
    if bytes.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "{field} must be exactly 32 bytes"
        )));
    }
    let value = Hash::from(bytes);
    if value.is_zero() {
        return Err(Status::invalid_argument(format!(
            "{field} must be non-zero"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use lore_base::types::Address;
    use lore_base::types::BranchPoint;
    use lore_base::types::Partition;
    use lore_proto::lore::revision::v1::revision_create_operation::Op;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_storage::options::WriteOptions;
    use opentelemetry::KeyValue;
    use tonic::metadata::BinaryMetadataValue;

    use super::*;
    use crate::grpc::revision::v1::revision_create;
    use crate::grpc::revision::v1::revision_create::RevisionCreateLimits;
    use crate::notification::testing::MockNotificationSender;
    use crate::store::test_store_create;

    struct TestInstruments;

    impl InstrumentProvider for TestInstruments {
        fn namespace(&self) -> &'static str {
            "test"
        }

        fn labels(&self) -> &[KeyValue] {
            &[]
        }
    }

    async fn create_branch(
        repository: &Arc<RepositoryContext>,
        branch_id: BranchId,
        name: &str,
        stack: Vec<BranchPoint>,
    ) {
        branch::create(
            repository.clone(),
            &get_write_token(),
            branch_id,
            name,
            branch::default_category(),
            "tester",
            1,
            stack,
            false,
            false,
        )
        .await
        .expect("create branch");
    }

    async fn content(
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        repository: Partition,
        file_id: Context,
        value: &'static [u8],
    ) -> Address {
        lore_storage::write_content(
            immutable,
            repository,
            file_id,
            Bytes::from_static(value),
            WriteOptions::default(),
            None,
            None,
            None,
        )
        .await
        .expect("write content")
        .0
    }

    fn put(path: &str, address: Address) -> revision_v1::RevisionCreateOperation {
        revision_v1::RevisionCreateOperation {
            op: Some(Op::PutFile(revision_v1::RevisionCreatePutFile {
                path: path.into(),
                mode: 0o644,
                address: Some(address.into()),
            })),
        }
    }

    fn delete(path: &str) -> revision_v1::RevisionCreateOperation {
        revision_v1::RevisionCreateOperation {
            op: Some(Op::DeletePath(revision_v1::RevisionCreateDeletePath {
                path: path.into(),
            })),
        }
    }

    fn revision_request(
        repository: Partition,
        branch_id: BranchId,
        base: Hash,
        operations: Vec<revision_v1::RevisionCreateOperation>,
    ) -> Request<revision_v1::RevisionCreateRequest> {
        let mut request = Request::new(revision_v1::RevisionCreateRequest {
            request_id: Bytes::copy_from_slice(uuid::Uuid::now_v7().as_bytes()),
            branch_id: branch_id.into(),
            revision_signature_base: base.into(),
            commit_message: "test edit".into(),
            metadata: Vec::new(),
            operations,
        });
        request.metadata_mut().insert_bin(
            lore_transport::grpc::REPOSITORY_ID_KEY,
            BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    async fn commit(
        repository: Partition,
        branch_id: BranchId,
        base: Hash,
        operations: Vec<revision_v1::RevisionCreateOperation>,
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        mutable: Arc<dyn lore_storage::MutableStore>,
        notification: Arc<dyn NotificationSender>,
        hooks: &HookDispatcher,
    ) -> Hash {
        let response = revision_create::handler(
            revision_request(repository, branch_id, base, operations),
            immutable,
            mutable,
            notification,
            hooks,
            DEFAULT_HISTORY_STEP_SIZE,
            RevisionListAcceleration::default(),
            RevisionCreateLimits::default(),
            &TestInstruments,
        )
        .await
        .expect("commit revision")
        .into_inner();
        Hash::from(response.revision_signature)
    }

    fn merge_request(
        repository: Partition,
        request_id: uuid::Uuid,
        target_branch: BranchId,
        target: Hash,
        source_branch: BranchId,
        source: Hash,
        resolutions: Vec<revision_v1::BranchMergeResolution>,
    ) -> Request<revision_v1::BranchMergeRequest> {
        let mut request = Request::new(revision_v1::BranchMergeRequest {
            request_id: Bytes::copy_from_slice(request_id.as_bytes()),
            branch_id_target: target_branch.into(),
            revision_signature_target: target.into(),
            branch_id_source: source_branch.into(),
            revision_signature_source: source.into(),
            commit_message: "merge feature".into(),
            resolutions,
        });
        request.metadata_mut().insert_bin(
            lore_transport::grpc::REPOSITORY_ID_KEY,
            BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    fn choose(id: Bytes, side: revision_v1::BranchMergeSide) -> revision_v1::BranchMergeResolution {
        revision_v1::BranchMergeResolution {
            conflict_id: id,
            resolution: Some(Resolution::Side(side as i32)),
        }
    }

    #[tokio::test]
    async fn conflicts_are_non_mutating_and_mixed_choices_publish_idempotently() {
        let repository_id = Partition::from(uuid::Uuid::now_v7());
        let main = BranchId::from(uuid::Uuid::now_v7());
        let feature = BranchId::from(uuid::Uuid::now_v7());
        let (immutable, mutable, execution) = test_store_create().await.expect("stores");
        let mut notifications = MockNotificationSender::new();
        notifications
            .expect_branch_pushed()
            .times(4)
            .returning(|_, _, _, _, _| ());
        let notifications = Arc::new(notifications);
        let hooks = HookDispatcher::empty();

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable.clone(),
                mutable.clone(),
                repository_id,
            ));
            create_branch(&repository, main, "main", Vec::new()).await;

            let file_a = Context::from(uuid::Uuid::now_v7());
            let file_b = Context::from(uuid::Uuid::now_v7());
            let file_c = Context::from(uuid::Uuid::now_v7());
            let base_a = content(immutable.clone(), repository_id, file_a, b"base-a").await;
            let base_b = content(immutable.clone(), repository_id, file_b, b"base-b").await;
            let base_c = content(immutable.clone(), repository_id, file_c, b"base-c").await;
            let base = commit(
                repository_id,
                main,
                Hash::default(),
                vec![
                    put("a.txt", base_a),
                    put("b.txt", base_b),
                    put("c.txt", base_c),
                ],
                immutable.clone(),
                mutable.clone(),
                notifications.clone(),
                &hooks,
            )
            .await;
            create_branch(
                &repository,
                feature,
                "feature",
                vec![BranchPoint {
                    branch: main,
                    revision: base,
                }],
            )
            .await;

            let source_a = content(immutable.clone(), repository_id, file_a, b"source-a").await;
            let source_b = content(immutable.clone(), repository_id, file_b, b"source-b").await;
            let source = commit(
                repository_id,
                feature,
                base,
                vec![
                    put("a.txt", source_a),
                    put("b.txt", source_b),
                    delete("c.txt"),
                ],
                immutable.clone(),
                mutable.clone(),
                notifications.clone(),
                &hooks,
            )
            .await;

            let target_a = content(immutable.clone(), repository_id, file_a, b"target-a").await;
            let target_b = content(immutable.clone(), repository_id, file_b, b"target-b").await;
            let target_c = content(immutable.clone(), repository_id, file_c, b"target-c").await;
            let target = commit(
                repository_id,
                main,
                base,
                vec![
                    put("a.txt", target_a),
                    put("b.txt", target_b),
                    put("c.txt", target_c),
                ],
                immutable.clone(),
                mutable.clone(),
                notifications.clone(),
                &hooks,
            )
            .await;

            let stale_error = handler(
                merge_request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    main,
                    base,
                    feature,
                    source,
                    Vec::new(),
                ),
                immutable.clone(),
                mutable.clone(),
                notifications.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &TestInstruments,
            )
            .await
            .expect_err("stale target must fail");
            assert_eq!(stale_error.code(), tonic::Code::FailedPrecondition);

            let request_id = uuid::Uuid::now_v7();
            let conflicted = handler(
                merge_request(
                    repository_id,
                    request_id,
                    main,
                    target,
                    feature,
                    source,
                    Vec::new(),
                ),
                immutable.clone(),
                mutable.clone(),
                notifications.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &TestInstruments,
            )
            .await
            .expect("conflict response")
            .into_inner();
            assert_eq!(
                conflicted.outcome,
                revision_v1::BranchMergeOutcome::Conflicted as i32
            );
            assert_eq!(conflicted.unresolved_conflicts.len(), 3);
            assert_eq!(
                branch::load_latest(repository.clone(), main).await.unwrap(),
                target
            );

            let resolutions = conflicted
                .unresolved_conflicts
                .iter()
                .map(|conflict| {
                    let side = if conflict.path_source == "a.txt" || conflict.path_source == "c.txt"
                    {
                        revision_v1::BranchMergeSide::Source
                    } else {
                        revision_v1::BranchMergeSide::Target
                    };
                    choose(conflict.conflict_id.clone(), side)
                })
                .collect::<Vec<_>>();
            let request = || {
                merge_request(
                    repository_id,
                    request_id,
                    main,
                    target,
                    feature,
                    source,
                    resolutions.clone(),
                )
            };
            let merged = handler(
                request(),
                immutable.clone(),
                mutable.clone(),
                notifications.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &TestInstruments,
            )
            .await
            .expect("resolved merge")
            .into_inner();
            assert_eq!(
                merged.outcome,
                revision_v1::BranchMergeOutcome::Merged as i32
            );
            let revision = Hash::from(&merged.revision_signature);
            let state = State::deserialize(repository.clone(), revision)
                .await
                .expect("merge state");
            assert_eq!(state.parent_self(), target);
            assert_eq!(state.parent_other(), source);
            let a = state
                .find_node_link(repository.clone(), "a.txt")
                .await
                .expect("a.txt");
            let b = state
                .find_node_link(repository.clone(), "b.txt")
                .await
                .expect("b.txt");
            assert_eq!(
                state
                    .node(repository.clone(), a.node)
                    .await
                    .unwrap()
                    .address,
                source_a
            );
            assert_eq!(
                state
                    .node(repository.clone(), b.node)
                    .await
                    .unwrap()
                    .address,
                target_b
            );
            assert!(
                state
                    .find_node_link(repository.clone(), "c.txt")
                    .await
                    .is_err()
            );

            let retry = handler(
                request(),
                immutable.clone(),
                mutable.clone(),
                notifications.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &TestInstruments,
            )
            .await
            .expect("idempotent retry")
            .into_inner();
            assert_eq!(retry, merged);

            let already = handler(
                merge_request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    main,
                    revision,
                    feature,
                    source,
                    Vec::new(),
                ),
                immutable.clone(),
                mutable.clone(),
                notifications,
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                &TestInstruments,
            )
            .await
            .expect("source is already merged")
            .into_inner();
            assert_eq!(
                already.outcome,
                revision_v1::BranchMergeOutcome::AlreadyUpToDate as i32
            );
            assert_eq!(
                branch::load_latest(repository, main).await.unwrap(),
                revision
            );
        }))
        .await;
    }
}
