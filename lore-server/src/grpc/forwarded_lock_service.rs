// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_proto::lock::ForwardedUnlockRequest;
use lore_proto::lock::LockRequest;
use lore_proto::lock::LockResponse;
use lore_proto::lock::QueryRequest;
use lore_proto::lock::QueryResponse;
use lore_proto::lock::StatusRequest;
use lore_proto::lock::StatusResponse;
use lore_proto::lock::UnlockResponse;
use lore_proto::lock::forwarded_lock_service_server::ForwardedLockService;
use lore_revision::lock::LockStore;
use lore_revision::notification::NotificationSender;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::lock_service::lock_implementation;
use super::lock_service::query_implementation;
use super::lock_service::status_implementation;
use super::lock_service::unlock_implementation;
use super::timeout_grpc;
use crate::grpc::forwarded_requests::CallerContext;

/// Serves the lock RPCs a peer forwards on behalf of its client, from this server's lock store.
#[derive(Clone)]
pub struct LoreForwardedLockService {
    lock_store: Arc<dyn LockStore>,
    notification: Arc<dyn NotificationSender>,
    rpc_timeout: Duration,
}

impl LoreForwardedLockService {
    pub fn new(
        lock_store: Arc<dyn LockStore>,
        notification: Arc<dyn NotificationSender>,
        rpc_timeout: Duration,
    ) -> Self {
        Self {
            lock_store,
            notification,
            rpc_timeout,
        }
    }
}

/// Handler that takes a `Query` request forwarded on from a peer's `LockService`
/// and executes it, returning the result to the other server for forwarding on to its
/// client
#[tracing::instrument(name = "ForwardedLock::Query::Handler", skip_all)]
pub async fn query_handler(
    request: Request<QueryRequest>,
    lock_store: &dyn LockStore,
) -> Result<Response<QueryResponse>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;

    query_implementation(request.into_inner(), caller_context, lock_store).await
}

/// Handler that takes a `Lock` request forwarded on from a peer's `LockService`,
/// which has already authorized the caller, and locks on the caller's behalf.
#[tracing::instrument(name = "ForwardedLock::Lock::Handler", skip_all)]
pub async fn lock_handler(
    request: Request<LockRequest>,
    lock_store: &dyn LockStore,
    notification: &dyn NotificationSender,
) -> Result<Response<LockResponse>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;

    lock_implementation(
        request.into_inner(),
        caller_context,
        lock_store,
        notification,
    )
    .await
}

/// Handler that takes a `Status` request forwarded on from a peer's `LockService`
/// and answers it from this server's lock store.
#[tracing::instrument(name = "ForwardedLock::Status::Handler", skip_all)]
pub async fn status_handler(
    request: Request<StatusRequest>,
    lock_store: &dyn LockStore,
) -> Result<Response<StatusResponse>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;

    status_implementation(request.into_inner(), caller_context, lock_store).await
}

/// Handler that takes an `Unlock` request forwarded on from a peer's `LockService`,
/// which has already authorized the caller and decided whether they may release
/// other users' locks.
#[tracing::instrument(name = "ForwardedLock::Unlock::Handler", skip_all)]
pub async fn unlock_handler(
    request: Request<ForwardedUnlockRequest>,
    lock_store: &dyn LockStore,
    notification: &dyn NotificationSender,
) -> Result<Response<UnlockResponse>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;
    let request = request.into_inner();

    unlock_implementation(
        request.resources,
        request.elevated_caller,
        caller_context,
        lock_store,
        notification,
    )
    .await
}

#[tonic::async_trait]
impl ForwardedLockService for LoreForwardedLockService {
    async fn query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            query_handler(request, self.lock_store.as_ref()),
        )
        .await
    }

    async fn lock(&self, request: Request<LockRequest>) -> Result<Response<LockResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            lock_handler(
                request,
                self.lock_store.as_ref(),
                self.notification.as_ref(),
            ),
        )
        .await
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            status_handler(request, self.lock_store.as_ref()),
        )
        .await
    }

    async fn unlock(
        &self,
        request: Request<ForwardedUnlockRequest>,
    ) -> Result<Response<UnlockResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            unlock_handler(
                request,
                self.lock_store.as_ref(),
                self.notification.as_ref(),
            ),
        )
        .await
    }
}
