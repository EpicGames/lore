// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::RevisionItem;
use lore_proto::RevisionListRequest;
use lore_proto::RevisionListResponse;
use lore_proto::revision_list_request::Start;
use lore_revision::branch;
use lore_revision::find::FindMatchResult;
use lore_revision::find::find_revision;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::revision::{self};
use lore_revision::state::State;
use lore_revision::state::{self};
use lore_revision::util;
use lore_telemetry::LabelArray;
use lore_telemetry::observe::Observe;
use lore_telemetry::observe::ObserveResult;
use lore_telemetry::observe::observe_result;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_transport::grpc::REVISION_LIST_STRATEGY_HEADER;
use opentelemetry::KeyValue;
use smallvec::smallvec;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::metadata::MetadataValue;
use tracing::debug;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::revision_service::RevisionListInstruments;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

const MAX_REVISION_LIST_RESPONSE_ITEMS: usize = 100;
const METRICS_START_KEY: &str = "start_type";
const METRICS_LIST_STRATEGY_KEY: &str = "list_strategy";

enum RevisionListStrategy {
    Direct,
    FullIteration,
    HistoryStep,
}

impl RevisionListStrategy {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::FullIteration => "full-iteration",
            Self::HistoryStep => "history-step",
        }
    }
}

impl std::fmt::Display for RevisionListStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

fn start_to_metric_value(value: &Start) -> &'static str {
    match value {
        Start::Identifier(_) => "identifier",
        Start::Signature(_) => "signature",
    }
}

#[tracing::instrument(name = "RevisionList::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    instruments: &RevisionListInstruments,
) -> Result<Response<RevisionListResponse>, Status> {
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let user_id = get_user_id(request.extensions());
    let repository_id = get_repository(request.metadata())?;

    let req = request.into_inner();

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let Some(start) = req.start else {
                return Err(Status::invalid_argument("invalid start revision"));
            };

            let (hash, strategy) = {
                let labels = smallvec![KeyValue::new(
                    METRICS_START_KEY,
                    start_to_metric_value(&start)
                )];
                resolve_start(start, &repository, history_step_size, acceleration)
                    .observe(
                        instruments.resolve_start_duration.clone(),
                        labels,
                        observe_resolve_start(),
                    )
                    .await
                    .output?
            };

            let items =
                walk_revisions(hash, &strategy, &repository, history_step_size, instruments)
                    .observe_result(
                        instruments.walk_duration.clone(),
                        smallvec![KeyValue::new(METRICS_LIST_STRATEGY_KEY, strategy.as_str())],
                    )
                    .await
                    .output?;

            let mut response = Response::new(RevisionListResponse {
                items,
                ..Default::default()
            });

            response.metadata_mut().insert(
                REVISION_LIST_STRATEGY_HEADER,
                MetadataValue::from_static(strategy.as_str()),
            );
            Ok(response)
        })
        .await
}

async fn resolve_start(
    start: Start,
    repository: &Arc<RepositoryContext>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<(Hash, RevisionListStrategy), Status> {
    let start_info = match start {
        Start::Identifier(identifier) => {
            let branch = BranchId::from(&identifier.branch);

            let step_key_hit = if acceleration.step_keys {
                let (key, key_type) = branch::revision_step_key(
                    repository::SALT_LORE,
                    repository.id,
                    branch,
                    identifier.number,
                    history_step_size,
                );
                // Ignore errors as this is just an acceleration construct
                // This will be recreated on the lookup
                repository
                    .read_mutable_store()
                    .load(repository.id, key, key_type)
                    .await
                    .ok()
                    .map(|revision| (key, revision))
            } else {
                None
            };

            if let Some((key, revision)) = step_key_hit {
                debug!(
                    number = %identifier.number,
                    key = %key,
                    "Found history step key"
                );
                // Now we have found the start revision of the containing HISTORY_STEP_SIZE
                // block of revisions. Now we search for the exact matching revision.
                let hash = find_revision(
                    repository.clone(),
                    branch,
                    revision,
                    false,
                    None,
                    |state, _metadata| {
                        let state_revision_number = state.revision_number();
                        match state_revision_number.cmp(&identifier.number) {
                            Ordering::Equal => FindMatchResult::Match,
                            Ordering::Less => FindMatchResult::Abort,
                            Ordering::Greater => FindMatchResult::Continue,
                        }
                    },
                )
                .await
                .map_err(|err| Status::invalid_argument(format!("invalid identifier {err}")))?;
                (hash, RevisionListStrategy::HistoryStep)
            } else {
                let signature = format!("{}@{}", branch, identifier.number);
                let hash = revision::resolve(
                    repository.clone(),
                    signature,
                    None,
                    ResolveSearchLocation::Local,
                )
                .await
                .map_err(|err| Status::invalid_argument(format!("invalid identifier {err}")))?;

                (hash, RevisionListStrategy::FullIteration)
            }
        }
        Start::Signature(signature) => (Hash::from(signature), RevisionListStrategy::Direct),
    };

    Ok(start_info)
}

fn observe_resolve_start()
-> impl Fn(&Result<(Hash, RevisionListStrategy), Status>, &Duration, &mut LabelArray) + Copy {
    move |result: &Result<(Hash, RevisionListStrategy), Status>,
          elapsed: &Duration,
          labels: &mut LabelArray| {
        // base observability
        observe_result(result, elapsed, labels);

        if let Ok(ok_result) = &result {
            labels.push(KeyValue::new(
                METRICS_LIST_STRATEGY_KEY,
                ok_result.1.as_str(),
            ));
        }
    }
}

async fn walk_revisions(
    mut revision: Hash,
    strategy: &RevisionListStrategy,
    repository: &Arc<RepositoryContext>,
    history_step_size: u64,
    instruments: &RevisionListInstruments,
) -> Result<Vec<RevisionItem>, Status> {
    let mut items = Vec::with_capacity(MAX_REVISION_LIST_RESPONSE_ITEMS);

    let mut base_revision = true;
    // Track previous revision for step key backfill during full iteration.
    // Stores (revision_number, revision_hash, metadata_hash) so the branch
    // can be read from the revision metadata at boundary crossings.
    let mut prev_step_state: Option<Arc<State>> = None;

    while items.len() < MAX_REVISION_LIST_RESPONSE_ITEMS {
        let state = {
            match state::State::deserialize(repository.clone(), revision)
                .await
                .filter_slow_down()?
            {
                Ok(state) => state,
                Err(ref err) if err.is_not_found() => {
                    if base_revision {
                        return Err(Status::not_found("Base revision not found"));
                    }
                    warn!(
                    {REPOSITORY_ID} = %repository.id, revision = %revision, error = "Failed reading state data from immutable store",
                        "Failed to deserialize revision state"
                    );
                    return Err(warn_error_to_status(err, |err| {
                        Status::internal(err.to_string())
                    }));
                }
                Err(err) => {
                    warn!(
                    {REPOSITORY_ID} = %repository.id, revision = %revision, error = "Failed reading state data from immutable store",
                        "Failed to deserialize revision state"
                    );
                    return Err(warn_error_to_status(&err, |err| {
                        Status::internal(err.to_string())
                    }));
                }
            }
        };

        if revision.is_zero() {
            break;
        }

        if base_revision
            && let Ok(metadata) =
                Metadata::deserialize(repository.clone(), state.metadata_hash()).await
            && let Ok(state_timestamp) = metadata.get_timestamp()
        {
            let current_timestamp = util::time::timestamp();
            let age_seconds = (current_timestamp - state_timestamp) / 1000;
            instruments.relative_age_seconds.record(
                age_seconds,
                &[KeyValue::new(METRICS_LIST_STRATEGY_KEY, strategy.as_str())],
            );
        }

        let current_number = state.revision_number();

        // Backfill missing history step keys during full iteration walks.
        // When we detect a step boundary crossing between consecutive
        // revisions, write the step key for the higher-numbered revision
        // so future lookups can use the HistoryStep strategy. The branch
        // is read from the revision metadata rather than carried from the
        // request identifier.
        if matches!(strategy, RevisionListStrategy::FullIteration)
            && let Some(previous_state) = &prev_step_state
            && let Some((lowest_b, highest_b)) = crate::cache::revision::sealed_boundaries(
                state.revision_number(),
                previous_state.revision_number(),
                history_step_size,
            )
            && let Ok(metadata) =
                Metadata::deserialize(repository.clone(), previous_state.metadata_hash()).await
            && let Ok(branch) = metadata.get_branch()
        {
            for boundary in (lowest_b..=highest_b).step_by(history_step_size as usize) {
                let _ = crate::cache::revision::seal_boundary_revision_number(
                    repository.clone(),
                    branch,
                    history_step_size,
                    boundary,
                    &state,
                    previous_state,
                )
                .await;
                debug!(boundary, "Backfilled history step key");
            }
        }
        if matches!(strategy, RevisionListStrategy::FullIteration) {
            prev_step_state = Some(state.clone());
        }

        let item = RevisionItem {
            number: current_number,
            signature: Bytes::from_owner(revision),
            metadata: Bytes::from_owner(state.metadata_hash()),
        };
        items.push(item);

        revision = state.parent_self();
        base_revision = false;
    }

    Ok(items)
}
