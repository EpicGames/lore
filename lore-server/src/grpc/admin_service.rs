// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lore_base::version::LORE_LIBRARY_VERSION;
use lore_proto::rpc::HostInfo;
use lore_proto::rpc::ServerInfoRequest;
use lore_proto::rpc::ServerInfoResponse;
use lore_proto::rpc::admin_service_server::AdminService;
use lore_revision::notification::NotificationSender;
use sysinfo::CpuRefreshKind;
use sysinfo::MemoryRefreshKind;
use sysinfo::RefreshKind;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use super::handlers::obliterate;
use super::timeout_grpc;
use crate::auth::jwt::JwtVerifier;
use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::hooks::HookDispatcher;

pub struct LoreAdminService {
    server_info: ServerInfoResponse,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    jwt_verifier: Arc<Option<JwtVerifier>>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: Arc<HookDispatcher>,
    rpc_timeout: Duration,
}

impl LoreAdminService {
    pub fn new(
        settings: HashMap<String, String>,
        features: Vec<String>,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        notification: Arc<dyn NotificationSender>,
        hook_dispatcher: Arc<HookDispatcher>,
    ) -> Self {
        let mut sys =
            sysinfo::System::new_with_specifics(RefreshKind::everything().without_processes());

        sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_frequency());
        sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());

        let cores = sys.cpus().len();
        let cpu = sys
            .cpus()
            .iter()
            .next()
            .map_or("unknown".to_string(), |cpu| {
                format!(
                    "{} ({} cores) {:.1}ghz",
                    cpu.brand(),
                    cores,
                    cpu.frequency() as f64 / 1000f64
                )
            });

        let ram = format!("{} GiB", sys.total_memory() / 1024 / 1024 / 1024);

        Self {
            server_info: ServerInfoResponse {
                version: LORE_LIBRARY_VERSION.to_string(),
                features,
                settings,
                host: Some(HostInfo {
                    arch: env!("LORE_RUSTC_HOST_TRIPLE").to_string(),
                    cpu,
                    ram,
                    hostname: sysinfo::System::host_name().unwrap_or("unknown".to_string()),
                    environment: std::env::var("LORE_ENV").unwrap_or("unknown".to_string()),
                }),
            },
            immutable_store,
            mutable_store,
            jwt_verifier: Arc::new(None),
            repository_authorizer: Arc::new(AllowAllRepositoryAuthorizer),
            notification,
            hook_dispatcher,
            rpc_timeout: Duration::from_secs(60),
        }
    }

    /// Set together so the verifier and the authorizer selected for it cannot
    /// disagree: a verifier left beside the allow-all default would let any
    /// authenticated caller obliterate.
    pub fn set_auth(
        &mut self,
        jwt_verifier: Option<JwtVerifier>,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) {
        if jwt_verifier.is_none() {
            warn!(
                "No JWT verifier - Admin Service RPCs including obliterate will be unauthenticated"
            );
        }
        self.jwt_verifier = Arc::new(jwt_verifier);
        self.repository_authorizer = repository_authorizer;
    }

    pub fn set_rpc_timeout(&mut self, rpc_timeout: Duration) {
        self.rpc_timeout = rpc_timeout;
    }
}

#[tonic::async_trait]
impl AdminService for LoreAdminService {
    #[instrument(name = "AdminService::ServerInfo", skip_all)]
    async fn server_info(
        &self,
        _request: Request<ServerInfoRequest>,
    ) -> Result<Response<ServerInfoResponse>, Status> {
        info!("Request for ServerInfo");

        Ok(Response::new(self.server_info.clone()))
    }

    async fn obliterate(
        &self,
        request: Request<lore_proto::ObliterateRequest>,
    ) -> Result<Response<lore_proto::ObliterateResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            obliterate::handler(
                request,
                &self.repository_authorizer,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.notification.clone(),
                &self.hook_dispatcher,
                &self.jwt_verifier,
            ),
        )
        .await
    }
}
