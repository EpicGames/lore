// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::time::Duration;

use lore_proto::lore::repository::v1::RepositoryCreateRequest;
use lore_proto::lore::repository::v1::RepositoryCreateResponse;
use lore_proto::lore::repository::v1::forwarded_repository_service_server::ForwardedRepositoryService;
use lore_revision::environment::EnvironmentConfig;
use lore_telemetry::InstrumentProvider;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::repository_create;
use crate::grpc::timeout_grpc;
use crate::hooks::HookDispatcher;

#[derive(Clone)]
struct ForwardedRepositoryServiceInstrumentProvider;

impl InstrumentProvider for ForwardedRepositoryServiceInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.forwarded_repository.v1.service"
    }
}

/// Mirrors particular RPCs of `LoreRepositoryV1Service`
#[derive(Clone)]
pub struct LoreForwardedRepositoryV1Service {
    environment: EnvironmentConfig,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    hook_dispatcher: Arc<HookDispatcher>,
    instrument_provider: ForwardedRepositoryServiceInstrumentProvider,
    rpc_timeout: Duration,
}

impl LoreForwardedRepositoryV1Service {
    pub fn new(
        environment: EnvironmentConfig,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        hook_dispatcher: Arc<HookDispatcher>,
        rpc_timeout: Duration,
    ) -> Self {
        let instrument_provider = ForwardedRepositoryServiceInstrumentProvider;
        Self {
            environment,
            immutable_store,
            mutable_store,
            hook_dispatcher,
            rpc_timeout,
            instrument_provider,
        }
    }

    fn auth_url(&self) -> Option<String> {
        self.environment
            .endpoint
            .as_ref()
            .and_then(|endpoint| endpoint.auth_url.clone())
    }
}

#[tonic::async_trait]
impl ForwardedRepositoryService for LoreForwardedRepositoryV1Service {
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
                &self.hook_dispatcher,
                &self.instrument_provider,
            ),
        )
        .await
    }
}
