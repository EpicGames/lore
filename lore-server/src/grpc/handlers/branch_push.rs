// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::LazyLock;

use lore_base::error::AddressNotFound;
use lore_base::lore_drain_tasks;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_proto::BranchPushRequest;
use lore_proto::BranchPushResponse;
use lore_revision::branch;
use lore_revision::branch::BranchError;
use lore_revision::branch::LATEST;
use lore_revision::branch::PROTECT;
use lore_revision::branch::load_latest;
use lore_revision::branch::metadata;
use lore_revision::branch::push;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::notification::NotificationSender;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::State;
use lore_revision::util::request_tracker::StoreRequestTracker;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::REVISION;
use lore_transport::grpc::address_not_found_status;
use opentelemetry::metrics::Histogram;
use parking_lot::Mutex;
use tokio::task::JoinSet;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::Level;
use tracing::debug;
use tracing::instrument;
use tracing::span;
use tracing::warn;

use crate::cache::revision::store_history_step;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::grpc::warn_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::util::setup_execution;

struct BranchPushInstrumentProvider;

impl InstrumentProvider for BranchPushInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.branch_push"
    }
}

static PEAK_FRAGMENT_LOOKUPS_IN_FLIGHT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider
        .length_histogram("peak_fragment_lookups_in_flight", peak_boundaries())
});

static FRAGMENT_LOOKUPS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram("fragment_lookups", count_boundaries())
});

static CONCURRENT_QUERY_BATCHES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram("concurrent_query_batches", peak_boundaries())
});

static COLLECTED_ADDRESSES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram("collected_addresses", count_boundaries())
});

fn peak_boundaries() -> Vec<f64> {
    vec![
        1., 10., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000., 25_000., 50_000.,
        100_000., 250_000.,
    ]
}

fn count_boundaries() -> Vec<f64> {
    vec![
        10., 100., 1_000., 10_000., 50_000., 100_000., 500_000., 1_000_000., 5_000_000.,
    ]
}

/// The store requests one push makes to verify its fragments, recorded once when dropped, so a
/// push that fails or is cancelled part-way still reports what it made.
///
/// `lookups` counts the per-address fragment lookups the collection spawns, across every
/// verification the push runs. The address and batch counts are those of its latest
/// verification, since a push verifies again only for the same revision.
#[derive(Default)]
struct PushStoreRequests {
    lookups: Arc<StoreRequestTracker>,
    collected_addresses: Option<u64>,
    concurrent_query_batches: Option<u64>,
    /// Whether the push returned, with success or an error, rather than being dropped part-way
    /// as a cancelled request is.
    finished: bool,
}

impl Drop for PushStoreRequests {
    fn drop(&mut self) {
        let num_lookups = self.lookups.requests();
        // A push that never walks its fragments, such as one rejected as not a fast-forward,
        // would only skew the distributions.
        if num_lookups == 0 {
            return;
        }

        let peak_lookups_in_flight = self.lookups.peak_in_flight();
        PEAK_FRAGMENT_LOOKUPS_IN_FLIGHT.record(peak_lookups_in_flight, &[]);
        FRAGMENT_LOOKUPS.record(num_lookups, &[]);
        if let Some(collected_addresses) = self.collected_addresses {
            COLLECTED_ADDRESSES.record(collected_addresses, &[]);
        }
        if let Some(concurrent_query_batches) = self.concurrent_query_batches {
            CONCURRENT_QUERY_BATCHES.record(concurrent_query_batches, &[]);
        }
        debug!(
            peak_lookups_in_flight,
            num_lookups,
            num_collected_addresses = self.collected_addresses,
            num_concurrent_query_batches = self.concurrent_query_batches,
            finished = self.finished,
            "Branch push store requests"
        );
    }
}

#[lore_macro::test_pub]
pub(crate) fn extract_client_ip<T>(request: &Request<T>) -> Option<IpAddr> {
    // try to get the LAST entry from XFF metadata header (injected by ALB)
    if let Some(ip_str) = request
        .metadata()
        .get("x-forwarded-for")
        .and_then(|header_value| header_value.to_str().ok())
        .and_then(|ip_list| ip_list.rsplit(',').next()) // use the last value, if multiple are present
        .map(str::trim)
        && let Ok(ip) = ip_str.parse::<IpAddr>()
    {
        return Some(ip);
    }

    // if XFF is not available, fallback to using remote_addr()
    request.remote_addr().map(|socket_addr| socket_addr.ip())
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "BranchPush::handle", skip_all)]
pub async fn handler(
    request: Request<BranchPushRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchPushResponse>, Status> {
    let user_info = get_authorization(request.extensions());
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let repository = get_repository(request.metadata())?;

    // TODO(mjansson): Once we have authz permission model with read/write/admin
    // this should be upgraded to check for the correct permission rather than
    // hardwired to service accounts. For now used to protect while allowing mirroring
    let mut bypass_protection = false;
    if let Ok(user_info) = user_info
        && user_info.is_service_account.unwrap_or_default()
    {
        bypass_protection = true;
    }

    let client_ip: Option<String> = extract_client_ip(&request).map(|ip_addr| ip_addr.to_string());
    let req = request.into_inner();
    let branch = BranchId::from(req.branch);
    let revision = Hash::from(req.revision);
    let force = req.force;
    let fast_forward_merge = req.fast_forward_merge;

    if revision.is_zero() {
        warn!("Invalid branch push request, revision is zero");
        return Err(Status::invalid_argument("Invalid revision"));
    }

    debug!({REVISION} = %revision, bypass_protection, {BRANCH_ID} = %branch, force, fast_forward_merge,
        "Handling branch push request",
    );

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository,
    ));

    let repository_id: RepositoryId = repository.id;

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    LORE_CONTEXT
        .scope(execution, async move {
            let mut ctx_builder = HookContext::builder()
                .correlation_id(correlation_id.clone())
                .hook_point(HookPoint::BranchPush)
                .repository(repository_id)
                .user(user_id.clone())
                .branch(branch)
                .revision(revision);

            if let Some(ip) = client_ip {
                ctx_builder = ctx_builder.metadata("client_ip", ip);
            }

            let mut hook_ctx = ctx_builder.build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchPush, &hook_ctx)
                .map_err(hook_error_to_status)?;

            let PushResult {
                success,
                fast_forward_merged,
                revision,
                revision_number,
            } = push(
                repository.clone(),
                branch,
                revision,
                bypass_protection,
                force,
                fast_forward_merge,
                history_step_size,
                acceleration,
            )
            .await?;

            if success {
                lore_spawn!({
                    let user_id = user_id.clone();
                    async move {
                        notification
                            .branch_pushed(
                                repository_id,
                                branch,
                                &user_id,
                                revision,
                                revision_number,
                            )
                            .instrument(span!(Level::DEBUG, "publish_notification"))
                            .await;
                    }
                    .in_current_span()
                });

                // Post-hook dispatch (async, non-blocking)
                hook_ctx.set_revision_number(revision_number);
                hook_dispatcher.spawn_post(HookPoint::BranchPush, hook_ctx);
            }

            let num_branches_pushed = instrument_provider.counter("num_branches_pushed");
            num_branches_pushed.add(1, &[]);

            let message = if success {
                dispatch_response_message(
                    hook_dispatcher,
                    &correlation_id,
                    &user_id,
                    repository_id,
                    branch,
                    revision,
                    repository.clone(),
                )
                .await
            } else {
                None
            };

            Ok(Response::new(BranchPushResponse {
                success,
                fast_forward_merged,
                revision: revision.into(),
                revision_number,
                message,
            }))
        })
        .await
}

/// Pre-computes repository and branch metadata, then dispatches response hooks
/// to generate an optional message for the client.
///
/// Metadata lookup failures are silently ignored — absent metadata keys cause
/// response hooks to return an empty response.
pub(crate) async fn dispatch_response_message(
    hook_dispatcher: &HookDispatcher,
    correlation_id: &str,
    user_id: &str,
    repository_id: RepositoryId,
    branch: BranchId,
    revision: Hash,
    repository: Arc<RepositoryContext>,
) -> Option<String> {
    let mut builder = HookContext::builder()
        .correlation_id(correlation_id)
        .hook_point(HookPoint::BranchPush)
        .repository(repository_id)
        .user(user_id)
        .branch(branch)
        .revision(revision);

    // no filter_slow_down()? usage here: these reads only decorate the hook
    // context, and the push they describe has already succeeded.
    if let Ok(metadata_hash) = repository::metadata_hash(repository.clone()).await
        && let Ok(repository_metadata) =
            repository::metadata(repository.clone(), metadata_hash).await
    {
        builder = builder
            .metadata("repository_name", repository_metadata.name.clone())
            .metadata(
                "default_branch_name",
                &repository_metadata.default_branch_name,
            )
            .metadata(
                "is_default_branch",
                if repository_metadata.default_branch == branch {
                    "true"
                } else {
                    "false"
                },
            );
    }

    if let Ok(branch_meta) = branch::metadata(repository.clone(), branch).await
        && let Ok(branch_meta) =
            branch::branch_metadata(repository.clone(), branch, &branch_meta).await
    {
        builder = builder.metadata("branch_name", branch_meta.name);
    }

    let response_ctx = builder.build();
    hook_dispatcher
        .dispatch_response(HookPoint::BranchPush, &response_ctx)
        .message
}

pub struct PushResult {
    pub success: bool,
    pub fast_forward_merged: bool,
    pub revision: Hash,
    pub revision_number: u64,
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all, fields(branch))]
pub async fn push(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
    bypass_protection: bool,
    force: bool,
    fast_forward_merge: bool,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<PushResult, Status> {
    // Zero hash is never valid to push as latest pointer
    if latest.is_zero() {
        return Err(Status::failed_precondition("invalid revision signature"));
    }

    // Check if branch is protected
    let branch_metadata = metadata(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .warn_map_err(|err| Status::internal(format!("Failed to load branch metadata: {err}")))?;

    if branch_metadata.get_bool(PROTECT).unwrap_or_default() {
        if bypass_protection {
            debug!("Bypass branch protection and push to protected branch");
        } else {
            warn!("Branch push failed, branch is protected");
            return Err(Status::permission_denied("protected"));
        }
    }

    // Verify the branch has not been deleted by checking the name→id mapping
    if let Ok(branch_name) = branch::name(&branch_metadata)
        && !branch_name.is_empty()
    {
        let is_mapped = branch::load_name_to_id_local(repository.clone(), branch_name)
            .await
            .filter_slow_down()?
            .is_ok_and(|id| id == branch);
        if !is_mapped {
            debug!("Branch push rejected, name-to-id mapping missing for deleted branch");
            return Err(Status::not_found("Branch not found"));
        }
    }

    let current_head = load_latest(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .unwrap_or_default();

    // Verify the validity of the revision to push to latest
    let state = State::deserialize(repository.clone(), latest)
        .await
        .filter_slow_down()?
        .warn_map_err(|err| {
            if err.is_not_found() {
                Status::not_found(format!(
                    "Revision '{latest}' to push to latest is not found"
                ))
            } else {
                Status::internal(format!("failed to load current latest state: {err}"))
            }
        })?;

    // If the incoming revision is already the latest revision the push is a no-op
    if current_head == latest {
        return Ok(PushResult {
            success: true,
            fast_forward_merged: false,
            revision: current_head,
            revision_number: state.revision_number(),
        });
    }

    let mut store_requests = PushStoreRequests::default();
    let result = store_new_head(
        repository,
        branch,
        latest,
        state,
        current_head,
        force,
        fast_forward_merge,
        history_step_size,
        acceleration,
        &mut store_requests,
    )
    .await;
    store_requests.finished = true;
    result
}

/// Verifies the fragments `state` introduces and stores it as the latest revision of `branch`,
/// retrying while the head moves underneath it.
#[allow(clippy::too_many_arguments)]
async fn store_new_head(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
    state: Arc<State>,
    mut current_head: Hash,
    force: bool,
    fast_forward_merge: bool,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    store_requests: &mut PushStoreRequests,
) -> Result<PushResult, Status> {
    let mut new_head = latest;
    loop {
        // Verify the current latest revision is parent of the incoming revision unless the push is forced
        if current_head != state.parent_self() && !force {
            if current_head.is_zero() {
                warn!("Branch push failed, branch does not exist");
                return Err(Status::not_found("Branch not found"));
            }

            if !fast_forward_merge {
                return Ok(PushResult {
                    success: false,
                    fast_forward_merged: false,
                    revision: current_head,
                    revision_number: 0,
                });
            }

            // Fast-forward merge: the incoming revision's parent_self no longer matches
            // the branch head. Attempt to create a new merge revision with
            // parent_self=current_head and parent_other=incoming_revision.
            return try_fast_forward_merge(
                repository.clone(),
                branch,
                state.clone(),
                current_head,
                history_step_size,
                acceleration,
                store_requests,
            )
            .await;
        }

        let state_parent = State::deserialize(repository.clone(), state.parent_self())
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!("Failed to load incoming state: {err}"))
            })?;

        let state_other = load_other_parent_state(repository.clone(), &state).await?;

        // Verify that all new fragments exist
        verify_fragments(
            repository.clone(),
            state_parent.clone(),
            state_other.clone(),
            state.clone(),
            store_requests,
        )
        .await?;

        // Verify that the revision number is valid
        let revision_number = next_revision_number(
            state_parent.revision_number(),
            state_other.as_ref().map_or(0, |s| s.revision_number()),
        );

        if state.revision_number() != revision_number {
            // Rewrite the revision with a correct revision number
            state.set_revision_number(revision_number);
            let write_token = get_write_token();
            new_head = state
                .serialize(repository.clone(), &write_token)
                .await
                .filter_slow_down()?
                .warn_map_err(|err| {
                    Status::internal(format!("Failed to serialize state: {err}"))
                })?;
        }

        let previous_head = try_store_latest(repository.clone(), branch, current_head, new_head)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!("Failed to store new latest pointer: {err}"))
            })?;

        // Check if the compare-and-swap was successful by checking match with expected value
        if previous_head == current_head {
            // If equal it means the value was swapped, i.e the push was successful. Set the
            // new latest revision signature and break out of the loop to return success
            current_head = new_head;

            store_history_step(
                repository.clone(),
                branch,
                history_step_size,
                acceleration,
                state_parent,
                state.clone(),
            )
            .await;

            break;
        }

        // Latest pointer moved during the processing of this push call, loop and try again
        current_head = previous_head;
    }

    Ok(PushResult {
        success: true,
        fast_forward_merged: false,
        revision: current_head,
        revision_number: state.revision_number(),
    })
}

/// Attempts a server-side fast-forward merge when the target branch head has moved
/// since the client created the merge revision.
///
/// Creates a new merge revision with:
/// - `parent_self` = current branch head (target branch)
/// - `parent_other` = the incoming merge revision
///
/// Uses a three-way diff between the original merge base, the incoming revision,
/// and the current head. If conflicts are detected, returns failure so the client
/// can resolve locally.
///
/// Retries via CAS loop if the branch head moves again during processing.
#[instrument(level = "debug", skip_all)]
async fn try_fast_forward_merge(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    incoming_state: Arc<State>,
    mut current_head: Hash,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    store_requests: &mut PushStoreRequests,
) -> Result<PushResult, Status> {
    let incoming_revision = incoming_state.revision();
    let original_base = incoming_state.parent_self();

    debug!(
        %incoming_revision, %original_base, %current_head,
        "Attempting fast-forward merge"
    );

    // Verify that all new fragments from the incoming revision exist in the store,
    // matching the verification done in the normal push path. Without this check a
    // client could reference fragments that were never fully uploaded.
    let base_state = State::deserialize(repository.clone(), original_base)
        .await
        .filter_slow_down()?
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to load base state for fragment verification: {err}"
            ))
        })?;

    let other_parent_state = load_other_parent_state(repository.clone(), &incoming_state).await?;

    verify_fragments(
        repository.clone(),
        base_state,
        other_parent_state,
        incoming_state.clone(),
        store_requests,
    )
    .await?;

    loop {
        // Three-way diff: base=original merge target, source=incoming merge, target=current head
        debug!(
            %original_base, %incoming_revision, %current_head,
            "Computing diff3 for fast-forward merge"
        );
        let diff_result = lore_revision::revision::diff3_collect(
            repository.clone(),
            original_base,
            incoming_revision,
            current_head,
            None,
            false,
        )
        .await
        .filter_slow_down()?
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to compute diff3 for fast-forward merge: {err}"
            ))
        })?;
        debug!(
            changes = diff_result.changes.len(),
            conflicts = diff_result.conflicts.len(),
            "diff3 result for fast-forward merge"
        );

        if !diff_result.conflicts.is_empty() {
            debug!(
                conflicts = diff_result.conflicts.len(),
                "Fast-forward merge has conflicts, rejecting"
            );
            return Ok(PushResult {
                success: false,
                fast_forward_merged: false,
                revision: current_head,
                revision_number: 0,
            });
        }

        // Deserialize the current head state to use as base for the new merge revision
        let state_current = State::deserialize(repository.clone(), current_head)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!(
                    "Failed to deserialize current head for fast-forward merge: {err}"
                ))
            })?;

        // Apply the non-conflicting changes to the current head state
        state::apply_tree_changes(
            repository.clone(),
            state_current.clone(),
            &diff_result.changes,
        )
        .await
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to apply tree changes for fast-forward merge: {err}"
            ))
        })?;

        // Set parents: self=current head (target branch), other=incoming merge revision
        state_current.set_parent_self(current_head);
        state_current.set_parent_other(incoming_revision);

        // Compute revision number from both parents
        let parent_state = State::deserialize(repository.clone(), current_head)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!("Failed to load current head state: {err}"))
            })?;

        let revision_number = next_revision_number(
            parent_state.revision_number(),
            incoming_state.revision_number(),
        );
        state_current.set_revision_number(revision_number);

        // Copy metadata from the incoming revision and set merged-by to "server"
        let incoming_metadata_hash = incoming_state.metadata_hash();
        if !incoming_metadata_hash.is_zero() {
            let mut metadata = lore_revision::metadata::Metadata::deserialize(
                repository.clone(),
                incoming_metadata_hash,
            )
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!("Failed to load incoming revision metadata: {err}"))
            })?;

            metadata
                .set_branch(branch)
                .warn_map_err(|_| Status::internal("Failed to set branch in metadata"))?;
            // Preserve the existing merged-by field if set, otherwise fall back to "server"
            if metadata
                .get_string(lore_revision::metadata::MERGED_BY)
                .is_err()
            {
                metadata
                    .set_string(lore_revision::metadata::MERGED_BY, "server")
                    .warn_map_err(|_| Status::internal("Failed to set merged-by in metadata"))?;
            }
            metadata
                .set_u64(lore_revision::metadata::FAST_FORWARD_MERGE, 1)
                .warn_map_err(|_| {
                    Status::internal("Failed to set fast-forward-merge in metadata")
                })?;

            let metadata_hash = metadata
                .serialize(repository.clone())
                .await
                .filter_slow_down()?
                .warn_map_err(|_| Status::internal("Failed to serialize metadata"))?;
            state_current.set_metadata_hash(metadata_hash);
        }

        // Serialize the new merge state
        let write_token = get_write_token();
        let new_revision = state_current
            .serialize(repository.clone(), &write_token)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!(
                    "Failed to serialize fast-forward merge state: {err}"
                ))
            })?;

        // CAS: attempt to set the new revision as branch head
        let previous_head =
            try_store_latest(repository.clone(), branch, current_head, new_revision)
                .await
                .filter_slow_down()?
                .warn_map_err(|err| {
                    Status::internal(format!("Failed to store fast-forward merge latest: {err}"))
                })?;

        if previous_head == current_head {
            // CAS succeeded
            debug!(
                %new_revision, revision_number,
                "Fast-forward merge succeeded"
            );

            // Store acceleration index if needed
            store_history_step(
                repository.clone(),
                branch,
                history_step_size,
                acceleration,
                parent_state,
                state_current.clone(),
            )
            .await;

            return Ok(PushResult {
                success: true,
                fast_forward_merged: true,
                revision: new_revision,
                revision_number,
            });
        }

        // CAS failed — branch head moved again, retry with updated head
        debug!(
            %previous_head, %current_head,
            "Fast-forward merge CAS failed, retrying"
        );
        current_head = previous_head;
    }
}

/// Compute the next revision number from the parent revision numbers.
/// The revision number is one greater than the maximum of the two parents.
fn next_revision_number(parent_self_number: u64, parent_other_number: u64) -> u64 {
    std::cmp::max(parent_self_number, parent_other_number) + 1
}

/// The first address in `batch` the store did not answer with a full match,
/// warned where it is found.
fn first_missing_fragment(batch: &[Address], answers: &[StoreMatchResult]) -> Option<Address> {
    let address = batch
        .iter()
        .zip(answers.iter())
        .find(|(_, answer)| answer.match_made != StoreMatch::MatchFull)
        .map(|(address, _)| *address)?;

    warn!({ADDRESS} = %address, "Branch push failed, fragment not found");
    Some(address)
}

/// The state of the second parent `state` names, or `None` when it names none.
///
/// Only a merge revision names one, and both the revision number the merge takes and the
/// fragments it is verified against come from that parent. A parent the store cannot
/// answer for is reported as `FAILED_PRECONDITION` naming its address, as a missing
/// fragment is. The address is the one the load asked for, since a state the store cannot
/// answer for is reported as a plain absence carrying no address of its own.
async fn load_other_parent_state(
    repository: Arc<RepositoryContext>,
    state: &State,
) -> Result<Option<Arc<State>>, Status> {
    if state.parent_other().is_zero() {
        return Ok(None);
    }

    let address = Address::zero_context_hash(state.parent_other());
    let other_parent_state = State::deserialize(repository, state.parent_other())
        .await
        .filter_slow_down()?
        .warn_map_err(|err| {
            if err.is_not_found() {
                return address_not_found_status(
                    &AddressNotFound::from(address),
                    format!("Missing fragment '{address}'"),
                );
            }

            Status::internal(format!("Failed to load other parent state: {err}"))
        })?;

    Ok(Some(other_parent_state))
}

#[lore_macro::test_pub]
async fn collect_new_addresses(
    repository: Arc<RepositoryContext>,
    parent_state: Arc<State>,
    other_parent_state: Option<Arc<State>>,
    state: Arc<State>,
    store_requests: Arc<StoreRequestTracker>,
) -> Result<Vec<Address>, Status> {
    // Each walk appends what it found rather than returning it, so the drain below can
    // be the shared one: it joins every task before reporting a failure. A walk must
    // never be aborted part-way.
    let collected: Arc<Mutex<Vec<Address>>> = Default::default();

    let collect = async |parent_state: Arc<State>,
                         repository: Arc<RepositoryContext>,
                         to_state: Arc<State>,
                         collected: Arc<Mutex<Vec<Address>>>,
                         store_requests: Arc<StoreRequestTracker>| {
        let parent = parent_state.revision();
        let mut fragments = state::collect_new_fragments(
            repository,
            parent_state,
            to_state,
            true, /* Ignore already durably stored fragments */
            store_requests,
        )
            .instrument(span!(Level::DEBUG, "collect_new_addresses"))
            .await
            .warn_map_err(|err| {
                if let Some(converted_error) = err.as_address_not_found() {
                    return address_not_found_status(
                        converted_error,
                        format!(
                            "Failed to collect new fragments for verification. Missing address '{converted_error}'"
                        ),
                    );
                }

                Status::internal(format!(
                    "Failed to collect new fragments for verification: {err}"
                ))
            })?;
        debug!(%parent, num_addresses = fragments.len(), "Collected new fragments against parent");

        collected.lock().append(&mut fragments);
        Ok(())
    };

    let mut collect_tasks = JoinSet::default();
    lore_spawn!(
        collect_tasks,
        collect(
            parent_state,
            repository.clone(),
            state.clone(),
            collected.clone(),
            store_requests.clone(),
        )
        .in_current_span()
    );
    if let Some(other_parent_state) = other_parent_state {
        collected
            .lock()
            .push(Address::zero_context_hash(state.parent_other()));
        lore_spawn!(
            collect_tasks,
            collect(
                other_parent_state,
                repository,
                state,
                collected.clone(),
                store_requests
            )
            .in_current_span()
        );
    }
    lore_drain_tasks!(
        collect_tasks,
        Status::internal("Error with collect_new_addresses task")
    )?;

    let mut new_fragments = std::mem::take(&mut *collected.lock());

    // each address vec appended is already sorted, so `sort()` can detect the linear runs
    // better than `sort_unstable()`
    new_fragments.sort();
    new_fragments.dedup();

    debug!(
        num_addresses = new_fragments.len(),
        "Collected new addresses for verification"
    );

    Ok(new_fragments)
}

/// Verify that all new fragments between `parent_state` and `state` exist in the
/// immutable store. Also includes the other parent hash if the state is a merge.
/// Returns an error if any fragment is missing.
///
/// A merge revision joins two lines of history, and what it names is new to this
/// branch against either of them, so `other_parent_state` is collected against as well.
/// Reading that parent's own tree is what refuses a peer that pushed the merge without
/// the tip of the line it merged.
///
/// A missing fragment is reported as `FAILED_PRECONDITION` naming the address,
/// whether the walk cannot read it or the store answers that it is absent.
/// `NOT_FOUND` is left to name an absent branch, which a caller reinstates.
async fn verify_fragments(
    repository: Arc<RepositoryContext>,
    parent_state: Arc<State>,
    other_parent_state: Option<Arc<State>>,
    state: Arc<State>,
    store_requests: &mut PushStoreRequests,
) -> Result<(), Status> {
    let mut new_fragments = collect_new_addresses(
        repository.clone(),
        parent_state,
        other_parent_state,
        state,
        store_requests.lookups.clone(),
    )
    .await?;
    store_requests.collected_addresses = Some(new_fragments.len() as u64);
    store_requests.concurrent_query_batches = None;

    let mut retry = lore_revision::util::time::retry(
        push::RETRY_START_DURATION,
        push::RETRY_MAX_DURATION,
        push::RETRY_MAX_ATTEMPTS,
    );

    let max_batch_size = repository
        .immutable_store()
        .max_query_batch()
        .unwrap_or(1000)
        .clamp(100, 10000);

    let mut tasks = JoinSet::new();
    while !new_fragments.is_empty() || !tasks.is_empty() {
        let num_addresses = new_fragments.len();
        let batch_span = span!(
            Level::DEBUG,
            "exist_batch",
            items = num_addresses,
            batch_size = max_batch_size
        );

        batch_span.in_scope(|| {
            while !new_fragments.is_empty() {
                let repository = repository.clone();
                let batch =
                    new_fragments.split_off(new_fragments.len().saturating_sub(max_batch_size));
                lore_spawn!(
                    tasks,
                    async move {
                        let mut resolved = vec![StoreMatchResult::default(); batch.len()];
                        let result = repository
                            .immutable_store()
                            .query(repository.id, batch.as_slice(), &mut resolved)
                            .await
                            .map(|()| resolved);
                        (batch, result)
                    }
                    .in_current_span()
                );
            }
        });
        // A wave after a slow-down resends only the throttled batches, so the first wave is
        // the one that holds the peak.
        store_requests
            .concurrent_query_batches
            .get_or_insert(tasks.len() as u64);
        debug!(
            num_batches = tasks.len(),
            num_addresses, "Sent existence query batches"
        );

        let mut num_slow_downs = 0;
        while let Some(result) = tasks.join_next().await {
            let (mut batch, result) =
                result.warn_map_err(|err| Status::internal(format!("Query task failed: {err}")))?;
            match result {
                Ok(result) => {
                    if let Some(missing) = first_missing_fragment(&batch, &result) {
                        return Err(address_not_found_status(
                            &AddressNotFound::from(missing),
                            format!("Missing fragment '{missing}'"),
                        ));
                    }
                }
                Err(StoreError::SlowDown(_)) => {
                    new_fragments.append(&mut batch);
                    num_slow_downs += 1;
                }
                Err(err) => {
                    let response = warn_error_to_status(&err, |err| {
                        Status::internal(format!("Store query failed: {err}"))
                    });
                    return Err(response);
                }
            }
        }

        if !new_fragments.is_empty() && !retry.wait().await {
            warn!("Exhausted {num_slow_downs} fragment exist retries");
            return Err(Status::resource_exhausted("Slow down"));
        }
    }

    Ok(())
}

#[instrument(level = "debug", skip_all, fields(branch))]
pub async fn try_store_latest(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    current_expected_latest: Hash,
    new_latest: Hash,
) -> Result<Hash, BranchError> {
    lore_revision::branch::mutable_try_store(
        repository.clone(),
        LATEST,
        branch,
        current_expected_latest,
        new_latest,
    )
    .await
}
