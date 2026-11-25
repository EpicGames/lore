// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::InvalidArguments;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::LockResource;
use lore_proto::LockService;
use lore_proto::lock::AdminLockRequest;
use lore_proto::lock::AdminLockResponse;
use lore_proto::lock::ForwardedUnlockRequest;
use lore_proto::lock::LockRequest;
use lore_proto::lock::LockResponse;
use lore_proto::lock::QueryRequest;
use lore_proto::lock::QueryResponse;
use lore_proto::lock::StatusRequest;
use lore_proto::lock::StatusResponse;
use lore_proto::lock::UnlockRequest;
use lore_proto::lock::UnlockResponse;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use lore_revision::lock::util::store_lock_batch_size;
use lore_revision::lore::RepositoryId;
use lore_revision::notification::NotificationSender;
use lore_telemetry::InstrumentProvider;
use opentelemetry::metrics::Histogram;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::warn;

use super::ServerResultExt;
use super::extract_correlation_id;
use super::get_repository;
use super::get_user_id;
use super::timeout_grpc;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::check_request_cancelled;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::no_repository_access_status;
use crate::grpc::require_action;
use crate::util::setup_execution;

#[derive(Clone)]
struct LoreLockServiceInstrumentProvider {}

fn lock_query_from_request(
    repository: RepositoryId,
    request: &QueryRequest,
) -> Result<LockQuery, LockError> {
    match (&request.branch, &request.owner, &request.description) {
        // Repository
        (None, None, None) => Ok(LockQuery::Repository(repository)),
        // RepositoryBranch
        (Some(branch), None, None) => Ok(LockQuery::RepositoryBranch(repository, branch.into())),
        // RepositoryBranchDescription
        (Some(branch), None, Some(description)) => Ok(LockQuery::RepositoryBranchDescription(
            repository,
            branch.into(),
            description.clone(),
        )),
        // OwnerRepository
        (None, Some(owner), None) => Ok(LockQuery::OwnerRepository(owner.clone(), repository)),
        // OwnerRepositoryBranch
        (Some(branch), Some(owner), None) => Ok(LockQuery::OwnerRepositoryBranch(
            owner.clone(),
            repository,
            branch.into(),
        )),
        _ => Err(InvalidArguments {
            reason: "unsupported lock query combination".into(),
        }
        .into()),
    }
}

/// Answers a lock query from this server's own lock store.
pub async fn query_implementation(
    request: QueryRequest,
    caller_context: CallerContext,
    lock_store: &dyn LockStore,
) -> Result<Response<QueryResponse>, Status> {
    let query = lock_query_from_request(caller_context.repository_id, &request)
        .map_err(handle_lock_error)?;

    let execution = setup_execution(
        module_path!(),
        caller_context.correlation_id,
        caller_context.user_id,
    );

    LORE_CONTEXT
        .scope(execution, async move {
            lock_store
                .query_locks(query)
                .await
                .map(|result| {
                    Response::new(QueryResponse {
                        result: result.into_iter().map(Into::into).collect(),
                    })
                })
                .map_err(handle_lock_error)
        })
        .await
}

/// Has the peer named by `forwarded_requests` answer the query, and relays its
/// answer to the client.
async fn forward_query(
    request: QueryRequest,
    caller_context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<QueryResponse>, Status> {
    let mut client = forwarded_requests.forwarded_lock_service();
    let request = caller_context.to_forwarded_request(request)?;

    let query_result = client
        .query(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = query_result?;
    Ok(response)
}

/// Refuse a request `lock_store` cannot apply as one operation.
///
/// **Too many resources.** A request naming more resources than [`store_lock_batch_size`] gives
/// for the store is refused with `OUT_OF_RANGE`.
///
/// **The same resource twice.** A resource is keyed by its branch and hash as the store reads
/// them; the description is not part of that key, so two resources differing only there name
/// one lock. A request naming one key more than once is refused with `INVALID_ARGUMENT`.
///
/// The count is checked before the keys. A server forwarding a request checks it first, against
/// the store whose limit it advertises, and the peer checks it again against its own store, whose
/// limit may be lower.
fn check_resources(
    lock_store: &dyn LockStore,
    resources: &[lore_proto::lock::Resource],
) -> Result<(), Status> {
    let limit = store_lock_batch_size(lock_store);

    if resources.len() > limit {
        return Err(Status::out_of_range(format!(
            "Resource count {} exceeds limit {limit}",
            resources.len()
        )));
    }

    let mut seen = HashSet::with_capacity(resources.len());
    if let Some(duplicate) = resources.iter().find(|resource| {
        !seen.insert((
            BranchId::from(resource.branch.as_ref()),
            Hash::from(resource.hash.as_ref()),
        ))
    }) {
        return Err(Status::invalid_argument(format!(
            "Resource {} is named more than once",
            duplicate.description
        )));
    }

    Ok(())
}

/// Locks resources for the caller in this server's own lock store, and
/// announces the new locks.
pub async fn lock_implementation(
    request: LockRequest,
    caller_context: CallerContext,
    lock_store: &dyn LockStore,
    notification: &dyn NotificationSender,
) -> Result<Response<LockResponse>, Status> {
    if request.resources.is_empty() {
        return Ok(Response::new(LockResponse { locks: vec![] }));
    }

    check_resources(lock_store, &request.resources)?;

    let execution = setup_execution(
        module_path!(),
        caller_context.correlation_id,
        caller_context.user_id.clone(),
    );

    LORE_CONTEXT
        .scope(execution, async move {
            lock_as_user(
                lock_store,
                notification,
                caller_context.repository_id,
                request.resources,
                &caller_context.user_id,
            )
            .await
            .map(|locks| Response::new(LockResponse { locks }))
        })
        .await
}

/// Has the peer named by `forwarded_requests` take the locks, and relays its
/// answer to the client.
async fn forward_lock(
    request: LockRequest,
    caller_context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<LockResponse>, Status> {
    let mut client = forwarded_requests.forwarded_lock_service();
    let request = caller_context.to_forwarded_request(request)?;

    let lock_result = client
        .lock(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = lock_result?;
    Ok(response)
}

/// Answers a lock status request from this server's own lock store.
pub async fn status_implementation(
    request: StatusRequest,
    caller_context: CallerContext,
    lock_store: &dyn LockStore,
) -> Result<Response<StatusResponse>, Status> {
    if request.resources.is_empty() {
        return Ok(Response::new(StatusResponse { locks: vec![] }));
    }

    check_resources(lock_store, &request.resources)?;

    info!(
        num_items = request.resources.len(),
        "Handling LockService::Status request"
    );

    let resources: Vec<LockResource> = request.resources.into_iter().map(Into::into).collect();

    let execution = setup_execution(
        module_path!(),
        caller_context.correlation_id,
        caller_context.user_id,
    );

    LORE_CONTEXT
        .scope(execution, async move {
            let locks = lock_store
                .check_locks_status(caller_context.repository_id, &resources)
                .await
                .map_err(handle_lock_error)?;

            Ok(Response::new(StatusResponse {
                locks: locks.into_iter().map(Into::into).collect(),
            }))
        })
        .await
}

/// Has the peer named by `forwarded_requests` answer the status request, and
/// relays its answer to the client.
async fn forward_status(
    request: StatusRequest,
    caller_context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<StatusResponse>, Status> {
    let mut client = forwarded_requests.forwarded_lock_service();
    let request = caller_context.to_forwarded_request(request)?;

    let status_result = client
        .status(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = status_result?;
    Ok(response)
}

/// Releases locks in this server's own lock store, and announces the released
/// resources. Unless `elevated_caller` is set, each lock must be held by the caller.
pub async fn unlock_implementation(
    resources: Vec<lore_proto::lock::Resource>,
    elevated_caller: bool,
    caller_context: CallerContext,
    lock_store: &dyn LockStore,
    notification: &dyn NotificationSender,
) -> Result<Response<UnlockResponse>, Status> {
    if resources.is_empty() {
        return Ok(Response::new(UnlockResponse { resources: vec![] }));
    }

    check_resources(lock_store, &resources)?;

    let resources: Vec<LockResource> = resources.iter().map(Into::into).collect();
    let repository = caller_context.repository_id;
    let user_id = caller_context.user_id;

    let execution = setup_execution(
        module_path!(),
        caller_context.correlation_id,
        user_id.clone(),
    );

    LORE_CONTEXT
        .scope(execution, async move {
            check_request_cancelled()?;
            let resources = lock_store
                .unlock_resources(user_id.as_str(), !elevated_caller, repository, &resources)
                .await
                .map_err(handle_lock_error)?;

            // TODO: UCS-13626 move branch out of individual resources into the main message
            // All resources are on the same branch and the lock call has to be made with at least 1 resource
            if !resources.is_empty() {
                notification
                    .resource_unlocked(repository, resources[0].branch, &user_id, &resources)
                    .await;
            }

            Ok(Response::new(UnlockResponse {
                resources: resources.into_iter().map(Into::into).collect(),
            }))
        })
        .await
}

/// Has the peer named by `forwarded_requests` release the locks, and relays
/// its answer to the client.
async fn forward_unlock(
    request: ForwardedUnlockRequest,
    caller_context: CallerContext,
    forwarded_requests: &Arc<dyn ForwardedRequests>,
) -> Result<Response<UnlockResponse>, Status> {
    let mut client = forwarded_requests.forwarded_lock_service();
    let request = caller_context.to_forwarded_request(request)?;

    let unlock_result = client
        .unlock(request)
        .await
        .warn_map_err(|_err| Status::internal("Error making forwarded request"))?;

    // the Error arm of this result is for the client
    let response = unlock_result?;
    Ok(response)
}

async fn lock_as_user(
    lock_store: &dyn LockStore,
    notification: &dyn NotificationSender,
    repository: RepositoryId,
    resources: Vec<lore_proto::lock::Resource>,
    owner_id: &str,
) -> Result<Vec<lore_proto::lock::Lock>, Status> {
    if resources.is_empty() {
        return Err(Status::invalid_argument("At least one resource needed"));
    }

    let lock_resources: Vec<LockResource> = resources.into_iter().map(Into::into).collect();

    check_request_cancelled()?;
    let locks = lock_store
        .lock_resources(owner_id, repository, &lock_resources)
        .await
        .map_err(handle_lock_error)?;

    // TODO: UCS-13626 move branch out of individual resources into the main message
    // All resources are on the same branch and the lock call has to be made with at least 1 resource
    let branch = lock_resources[0].branch;
    let locked_resources: Vec<LockResource> =
        locks.iter().map(|lock| lock.resource.clone()).collect();

    notification
        .resource_locked(repository, branch, owner_id, &locked_resources)
        .await;

    Ok(locks.into_iter().map(Into::into).collect())
}

fn handle_lock_error(error: LockError) -> Status {
    match error {
        LockError::LockNotFound(_) => Status::not_found(error.to_string()),
        LockError::LockNotOwned(_) => Status::failed_precondition(error.to_string()),
        LockError::SlowDown(_) => Status::resource_exhausted(error.to_string()),
        LockError::InvalidArguments(_) => Status::invalid_argument(error.to_string()),
        LockError::Internal(_) => {
            warn!(error = ?error, "LockData operation failed");
            Status::internal(error.to_string())
        }
    }
}

#[derive(Clone)]
pub struct LoreLockService {
    lock_store: Arc<dyn LockStore>,
    notification: Arc<dyn NotificationSender>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
    rpc_timeout: Duration,

    instrument_provider: LoreLockServiceInstrumentProvider,
    locking_histogram: Histogram<u64>,
    status_histogram: Histogram<u64>,
}

impl LoreLockService {
    pub fn new(
        lock_store: Arc<dyn LockStore>,
        notification: Arc<dyn NotificationSender>,
        authorizer: Arc<dyn RepositoryAuthorizer>,
        forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
        rpc_timeout: Duration,
    ) -> Self {
        let instrument_provider = LoreLockServiceInstrumentProvider {};

        Self {
            lock_store,
            notification,
            authorizer,
            forwarded_requests,
            rpc_timeout,
            locking_histogram: instrument_provider.length_histogram(
                "locking.request.resources.length",
                vec![1., 5., 10., 25., 50., 75., 100., 200.],
            ),
            status_histogram: instrument_provider.length_histogram(
                "status.request.resources.length",
                vec![
                    1., 5., 10., 50., 100., 200., 300., 500., 2_500., 5_000., 10_000., 20_000.,
                    40_000., 60_000., 80_000.,
                ],
            ),
            instrument_provider,
        }
    }
}

impl InstrumentProvider for LoreLockServiceInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.lock_service"
    }
}

impl LoreLockService {
    /// `owner` or `admin` on `repository` waives the lock-ownership check.
    async fn is_elevated(
        &self,
        extensions: &tonic::Extensions,
        repository: RepositoryId,
    ) -> Result<bool, Status> {
        Ok(self
            .authorizer
            .permits(extensions, repository, Action::Owner)
            .await?
            || self
                .authorizer
                .permits(extensions, repository, Action::Admin)
                .await?)
    }
}

impl LoreLockService {
    async fn handle_lock(
        &self,
        request: Request<LockRequest>,
    ) -> Result<Response<LockResponse>, Status> {
        require_action(&self.authorizer, &request, Action::Write).await?;
        let caller_context = CallerContext::from_original_request(&request)?;
        let lock_request = request.into_inner();

        self.locking_histogram.record(
            lock_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("lock"),
        );

        // Answered here so an empty request never waits on, or fails with, the peer.
        if lock_request.resources.is_empty() {
            return Ok(Response::new(LockResponse { locks: vec![] }));
        }

        if let Some(forwarded_requests) = &self.forwarded_requests
            && forwarded_requests.rpc_flags().lock_lock
        {
            check_resources(self.lock_store.as_ref(), &lock_request.resources)?;
            forward_lock(lock_request, caller_context, forwarded_requests).await
        } else {
            lock_implementation(
                lock_request,
                caller_context,
                self.lock_store.as_ref(),
                self.notification.as_ref(),
            )
            .await
        }
    }

    async fn handle_query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let caller_context = CallerContext::from_original_request(&request)?;
        let query_request = request.into_inner();

        if let Some(forwarded_requests) = &self.forwarded_requests
            && forwarded_requests.rpc_flags().lock_query
        {
            forward_query(query_request, caller_context, forwarded_requests).await
        } else {
            query_implementation(query_request, caller_context, self.lock_store.as_ref()).await
        }
    }

    async fn handle_status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let caller_context = CallerContext::from_original_request(&request)?;
        let status_request = request.into_inner();

        self.status_histogram.record(
            status_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("status"),
        );

        // Answered here so an empty request never waits on, or fails with, the peer.
        if status_request.resources.is_empty() {
            return Ok(Response::new(StatusResponse { locks: vec![] }));
        }

        if let Some(forwarded_requests) = &self.forwarded_requests
            && forwarded_requests.rpc_flags().lock_status
        {
            check_resources(self.lock_store.as_ref(), &status_request.resources)?;
            forward_status(status_request, caller_context, forwarded_requests).await
        } else {
            status_implementation(status_request, caller_context, self.lock_store.as_ref()).await
        }
    }

    async fn handle_unlock(
        &self,
        request: Request<UnlockRequest>,
    ) -> Result<Response<UnlockResponse>, Status> {
        let repository = get_repository(request.metadata())?;
        let elevated_caller = self.is_elevated(request.extensions(), repository).await?;
        if !elevated_caller
            && !self
                .authorizer
                .permits(request.extensions(), repository, Action::Write)
                .await?
        {
            return Err(no_repository_access_status());
        }
        let caller_context = CallerContext::from_original_request(&request)?;
        let unlock_request = request.into_inner();

        self.locking_histogram.record(
            unlock_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("unlock"),
        );

        // Answered here so an empty request never waits on, or fails with, the peer.
        if unlock_request.resources.is_empty() {
            return Ok(Response::new(UnlockResponse { resources: vec![] }));
        }

        if let Some(forwarded_requests) = &self.forwarded_requests
            && forwarded_requests.rpc_flags().lock_unlock
        {
            check_resources(self.lock_store.as_ref(), &unlock_request.resources)?;
            let request = ForwardedUnlockRequest {
                resources: unlock_request.resources,
                elevated_caller,
            };
            forward_unlock(request, caller_context, forwarded_requests).await
        } else {
            unlock_implementation(
                unlock_request.resources,
                elevated_caller,
                caller_context,
                self.lock_store.as_ref(),
                self.notification.as_ref(),
            )
            .await
        }
    }

    async fn handle_admin_lock(
        &self,
        request: Request<AdminLockRequest>,
    ) -> Result<Response<AdminLockResponse>, Status> {
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let repository = get_repository(request.metadata())?;
        if !self
            .authorizer
            .permits(request.extensions(), repository, Action::Migrate)
            .await?
        {
            warn!("Attempt to apply admin locks, but user does not have the correct permissions");
            return Err(Status::permission_denied("Permission denied"));
        }

        let user_id = get_user_id(request.extensions());
        let lock_request = request.into_inner();

        self.locking_histogram.record(
            lock_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("admin_lock"),
        );

        check_resources(self.lock_store.as_ref(), &lock_request.resources)?;

        if lock_request.resources.is_empty() {
            return Ok(Response::new(AdminLockResponse { locks: vec![] }));
        }

        let resources = lock_request.resources;
        let owner = lock_request.owner;

        let execution = setup_execution(module_path!(), correlation_id, user_id.clone());

        LORE_CONTEXT
            .scope(execution, async move {
                lock_as_user(
                    self.lock_store.as_ref(),
                    self.notification.as_ref(),
                    repository,
                    resources,
                    &owner,
                )
                .await
                .map(|locks| Response::new(AdminLockResponse { locks }))
            })
            .await
    }
}

#[tonic::async_trait]
impl LockService for LoreLockService {
    #[tracing::instrument(name = "LoreLockService::lock", skip_all)]
    async fn lock(&self, request: Request<LockRequest>) -> Result<Response<LockResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_lock(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::query", skip_all)]
    async fn query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_query(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::status", skip_all)]
    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_status(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::unlock", skip_all)]
    async fn unlock(
        &self,
        request: Request<UnlockRequest>,
    ) -> Result<Response<UnlockResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_unlock(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::admin_lock", skip_all)]
    async fn admin_lock(
        &self,
        request: Request<AdminLockRequest>,
    ) -> Result<Response<AdminLockResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_admin_lock(request)).await
    }
}
