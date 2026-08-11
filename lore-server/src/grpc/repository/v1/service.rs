// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lore_proto::lore::repository::v1::RepositoryCreateRequest;
use lore_proto::lore::repository::v1::RepositoryCreateResponse;
use lore_proto::lore::repository::v1::RepositoryDeleteRequest;
use lore_proto::lore::repository::v1::RepositoryDeleteResponse;
use lore_proto::lore::repository::v1::RepositoryGetRequest;
use lore_proto::lore::repository::v1::RepositoryGetResponse;
use lore_proto::lore::repository::v1::RepositoryListRequest;
use lore_proto::lore::repository::v1::RepositoryListResponse;
use lore_proto::lore::repository::v1::RepositoryMetadataGetRequest;
use lore_proto::lore::repository::v1::RepositoryMetadataGetResponse;
use lore_proto::lore::repository::v1::RepositoryMetadataSetRequest;
use lore_proto::lore::repository::v1::RepositoryMetadataSetResponse;
use lore_proto::lore::repository::v1::repository_service_server::RepositoryService;
use lore_revision::environment::EnvironmentConfig;
use lore_telemetry::InstrumentProvider;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::codegen::tokio_stream::Stream;

use super::repository_create;
use super::repository_delete;
use super::repository_get;
use super::repository_list;
use super::repository_metadata_get;
use super::repository_metadata_set;
use crate::authnz::repository_authorizer::repository_authorizer;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::timeout_grpc;
use crate::hooks::HookDispatcher;

type RepositoryListStream =
    Pin<Box<dyn Stream<Item = Result<RepositoryListResponse, Status>> + Send + 'static>>;

/// Zero-sized `InstrumentProvider` carrying the v1 service's metric
/// namespace. Standalone so the constructor can mint instruments before
/// `LoreRepositoryV1Service` exists.
#[derive(Clone)]
struct RepositoryServiceInstrumentProvider;

impl InstrumentProvider for RepositoryServiceInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.repository.v1.repository_service"
    }
}

/// Dispatch struct for `lore.repository.v1.RepositoryService`. Methods
/// delegate to the per-RPC handlers in sibling files; the struct itself
/// only holds the dependencies the handlers need.
#[derive(Clone)]
pub struct LoreRepositoryV1Service {
    environment: EnvironmentConfig,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    hook_dispatcher: Arc<HookDispatcher>,
    forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
    rpc_timeout: Duration,
    instrument_provider: RepositoryServiceInstrumentProvider,
}

impl LoreRepositoryV1Service {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        environment: EnvironmentConfig,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        hook_dispatcher: Arc<HookDispatcher>,
        forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
        rpc_timeout: Duration,
    ) -> Self {
        Self {
            environment,
            immutable_store,
            mutable_store,
            hook_dispatcher,
            forwarded_requests,
            rpc_timeout,
            instrument_provider: RepositoryServiceInstrumentProvider,
        }
    }

    /// Auth-service URL extracted from the environment, if configured.
    fn auth_url(&self) -> Option<String> {
        self.environment
            .endpoint
            .clone()
            .and_then(|endpoint| endpoint.auth_url)
    }
}

#[tonic::async_trait]
impl RepositoryService for LoreRepositoryV1Service {
    async fn repository_create(
        &self,
        request: Request<RepositoryCreateRequest>,
    ) -> Result<Response<RepositoryCreateResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            repository_create::handler(
                request,
                self.auth_url(),
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                &self.forwarded_requests,
                &self.hook_dispatcher,
                &self.instrument_provider,
            ),
        )
        .await
    }

    async fn repository_delete(
        &self,
        request: Request<RepositoryDeleteRequest>,
    ) -> Result<Response<RepositoryDeleteResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            repository_delete::handler(
                request,
                self.auth_url(),
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                &self.instrument_provider,
            ),
        )
        .await
    }

    async fn repository_get(
        &self,
        request: Request<RepositoryGetRequest>,
    ) -> Result<Response<RepositoryGetResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            repository_get::handler(
                request,
                self.auth_url(),
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    type RepositoryListStream = RepositoryListStream;

    async fn repository_list(
        &self,
        request: Request<RepositoryListRequest>,
    ) -> Result<Response<Self::RepositoryListStream>, Status> {
        repository_list::handler(
            request,
            self.auth_url(),
            self.immutable_store.clone(),
            self.mutable_store.clone(),
        )
        .await
    }

    async fn repository_metadata_get(
        &self,
        request: Request<RepositoryMetadataGetRequest>,
    ) -> Result<Response<RepositoryMetadataGetResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            repository_metadata_get::handler(
                request,
                repository_authorizer(self.auth_url()),
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn repository_metadata_set(
        &self,
        request: Request<RepositoryMetadataSetRequest>,
    ) -> Result<Response<RepositoryMetadataSetResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            repository_metadata_set::handler(
                request,
                repository_authorizer(self.auth_url()),
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use lore_proto::lore::repository::v1::repository_service_server::RepositoryServiceServer;

    use super::*;

    /// Compile-time check that `LoreRepositoryV1Service` fully implements
    /// the generated `RepositoryService` trait — wrapping it in
    /// `RepositoryServiceServer` requires the trait bound to hold.
    #[allow(dead_code)]
    fn assert_implements_trait(
        service: LoreRepositoryV1Service,
    ) -> RepositoryServiceServer<LoreRepositoryV1Service> {
        RepositoryServiceServer::new(service)
    }
}
