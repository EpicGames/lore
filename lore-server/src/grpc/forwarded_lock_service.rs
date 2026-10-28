// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_proto::lock::QueryRequest;
use lore_proto::lock::QueryResponse;
use lore_proto::lock::forwarded_lock_service_server::ForwardedLockService;
use lore_revision::lock::LockStore;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::lock_service::query_implementation;
use super::timeout_grpc;
use crate::grpc::forwarded_requests::CallerContext;

/// Serves the lock RPCs a peer forwards on behalf of its client, from this server's lock store.
#[derive(Clone)]
pub struct LoreForwardedLockService {
    lock_store: Arc<dyn LockStore>,
    rpc_timeout: Duration,
}

impl LoreForwardedLockService {
    pub fn new(lock_store: Arc<dyn LockStore>, rpc_timeout: Duration) -> Self {
        Self {
            lock_store,
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
}
