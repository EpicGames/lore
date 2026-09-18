// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_revision::notification::NotificationSender;
use lore_telemetry::InstrumentProvider;
use opentelemetry::metrics::Histogram;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::handlers::branch_create;
use super::handlers::branch_delete;
use super::handlers::branch_diff;
use super::handlers::branch_get;
use super::handlers::branch_list;
use super::handlers::branch_metadata_get;
use super::handlers::branch_metadata_set;
use super::handlers::branch_protect;
use super::handlers::branch_push;
use super::handlers::branch_query;
use super::handlers::branch_revision_list;
use super::handlers::branch_unprotect;
use super::handlers::revision_describe;
use super::handlers::revision_diff;
use super::handlers::revision_state_history;
use super::handlers::revision_tree;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::handlers::revision_list;
use crate::grpc::timeout_grpc;
use crate::hooks::HookDispatcher;
use crate::legacy::rpc::revision_service_server::RevisionService;

#[derive(Clone)]
pub struct RevisionListInstruments {
    pub resolve_start_duration: Histogram<f64>,
    pub relative_age_seconds: Histogram<u64>,
    pub walk_duration: Histogram<f64>,
}

#[derive(Clone)]
struct RevisionServiceInstrumentProvider;

impl InstrumentProvider for RevisionServiceInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.revision_service"
    }
}

#[derive(Clone)]
pub struct LoreRevisionService {
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: Arc<HookDispatcher>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    rpc_timeout: Duration,

    instrument_provider: RevisionServiceInstrumentProvider,
    revision_list_instruments: RevisionListInstruments,
}

impl LoreRevisionService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        notification: Arc<dyn NotificationSender>,
        hook_dispatcher: Arc<HookDispatcher>,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
        history_step_size: u64,
        acceleration: crate::grpc::server::RevisionListAcceleration,
        rpc_timeout: Duration,
    ) -> Self {
        let instrument_provider = RevisionServiceInstrumentProvider {};
        let seconds_in_one_day = 86400f64;
        let revision_list_instruments = RevisionListInstruments {
            resolve_start_duration: instrument_provider
                .latency_histogram_ms("revision_list.resolve_start.duration"),
            relative_age_seconds: instrument_provider.length_histogram(
                "revision_list.resolve_start.relative_age_seconds",
                vec![
                    seconds_in_one_day / 24f64,  // 1 hour
                    seconds_in_one_day / 2f64,   // 12 hours
                    seconds_in_one_day,          // 1 day
                    seconds_in_one_day * 3f64,   // 3 days
                    seconds_in_one_day * 7f64,   // 7 days
                    seconds_in_one_day * 14f64,  // 14 days
                    seconds_in_one_day * 30f64,  // 30 days
                    seconds_in_one_day * 60f64,  // 60 days
                    seconds_in_one_day * 180f64, // 180 days
                ],
            ),
            walk_duration: instrument_provider.latency_histogram_ms("revision_list.walk.duration"),
        };
        Self {
            immutable_store,
            mutable_store,
            notification,
            hook_dispatcher,
            repository_authorizer,
            history_step_size,
            acceleration,
            rpc_timeout,
            instrument_provider,
            revision_list_instruments,
        }
    }
}

#[tonic::async_trait]
impl RevisionService for LoreRevisionService {
    async fn branch_create(
        &self,
        request: Request<lore_proto::BranchCreateRequest>,
    ) -> Result<Response<lore_proto::BranchCreateResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_create::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.notification.clone(),
                &self.hook_dispatcher,
                &self.instrument_provider,
            ),
        )
        .await
    }

    async fn branch_delete(
        &self,
        request: Request<lore_proto::BranchDeleteRequest>,
    ) -> Result<Response<lore_proto::BranchDeleteResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_delete::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.notification.clone(),
                &self.hook_dispatcher,
                &self.instrument_provider,
            ),
        )
        .await
    }

    async fn branch_get(
        &self,
        request: Request<lore_proto::BranchGetRequest>,
    ) -> Result<Response<lore_proto::BranchGetResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_get::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_list(
        &self,
        request: Request<lore_proto::BranchListRequest>,
    ) -> Result<Response<lore_proto::BranchListResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_list::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_query(
        &self,
        request: Request<lore_proto::BranchQueryRequest>,
    ) -> Result<Response<lore_proto::BranchQueryResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_query::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_push(
        &self,
        request: Request<lore_proto::BranchPushRequest>,
    ) -> Result<Response<lore_proto::BranchPushResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_push::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.notification.clone(),
                &self.hook_dispatcher,
                self.history_step_size,
                self.acceleration,
                &self.instrument_provider,
            ),
        )
        .await
    }

    async fn revision_describe(
        &self,
        request: Request<lore_proto::RevisionDescribeRequest>,
    ) -> Result<Response<lore_proto::RevisionDescribeResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            revision_describe::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn revision_diff(
        &self,
        request: Request<lore_proto::RevisionDiffRequest>,
    ) -> Result<Response<lore_proto::RevisionDiffResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            revision_diff::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.repository_authorizer.clone(),
            ),
        )
        .await
    }

    async fn revision_tree(
        &self,
        request: Request<lore_proto::RevisionTreeRequest>,
    ) -> Result<Response<lore_proto::RevisionTreeResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            revision_tree::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.repository_authorizer.clone(),
            ),
        )
        .await
    }

    async fn revision_state_history(
        &self,
        request: Request<lore_proto::RevisionStateHistoryRequest>,
    ) -> Result<Response<lore_proto::RevisionStateHistoryResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            revision_state_history::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_diff(
        &self,
        request: Request<lore_proto::BranchDiffRequest>,
    ) -> Result<Response<lore_proto::BranchDiffResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_diff::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.repository_authorizer.clone(),
            ),
        )
        .await
    }

    async fn branch_revision_list(
        &self,
        request: Request<lore_proto::BranchRevisionListRequest>,
    ) -> Result<Response<lore_proto::BranchRevisionListResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_revision_list::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_protect(
        &self,
        request: Request<lore_proto::BranchProtectRequest>,
    ) -> Result<Response<lore_proto::BranchProtectResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_protect::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_metadata_get(
        &self,
        request: Request<lore_proto::BranchMetadataGetRequest>,
    ) -> Result<Response<lore_proto::BranchMetadataGetResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_metadata_get::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_metadata_set(
        &self,
        request: Request<lore_proto::BranchMetadataSetRequest>,
    ) -> Result<Response<lore_proto::BranchMetadataSetResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_metadata_set::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn branch_unprotect(
        &self,
        request: Request<lore_proto::BranchUnprotectRequest>,
    ) -> Result<Response<lore_proto::BranchUnprotectResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_unprotect::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    async fn revision_list(
        &self,
        request: Request<lore_proto::RevisionListRequest>,
    ) -> Result<Response<lore_proto::RevisionListResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            revision_list::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.history_step_size,
                self.acceleration,
                &self.revision_list_instruments,
            ),
        )
        .await
    }
}
