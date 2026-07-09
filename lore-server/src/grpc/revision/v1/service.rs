// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lore_proto::lore::revision::v1::BranchCreateRequest;
use lore_proto::lore::revision::v1::BranchCreateResponse;
use lore_proto::lore::revision::v1::BranchDeleteRequest;
use lore_proto::lore::revision::v1::BranchDeleteResponse;
use lore_proto::lore::revision::v1::BranchGetRequest;
use lore_proto::lore::revision::v1::BranchGetResponse;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_proto::lore::revision::v1::BranchMetadataGetRequest;
use lore_proto::lore::revision::v1::BranchMetadataGetResponse;
use lore_proto::lore::revision::v1::BranchMetadataSetRequest;
use lore_proto::lore::revision::v1::BranchMetadataSetResponse;
use lore_proto::lore::revision::v1::BranchPushRequest;
use lore_proto::lore::revision::v1::BranchPushResponse;
use lore_proto::lore::revision::v1::RevisionListRequest;
use lore_proto::lore::revision::v1::RevisionListResponse;
use lore_proto::lore::revision::v1::revision_service_server::RevisionService;
use lore_revision::notification::NotificationSender;
use lore_telemetry::InstrumentProvider;
use opentelemetry::metrics::Histogram;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::codegen::tokio_stream::Stream;

use super::branch_create;
use super::branch_delete;
use super::branch_get;
use super::branch_list;
use super::branch_metadata_get;
use super::branch_metadata_set;
use super::branch_push;
use super::revision_list;
use crate::grpc::forwarded_requests::ForwardedRequests;
use crate::grpc::timeout_grpc;
use crate::hooks::HookDispatcher;

type BranchListStream =
    Pin<Box<dyn Stream<Item = Result<BranchListResponse, Status>> + Send + 'static>>;

#[derive(Clone)]
pub struct RevisionListInstruments {
    pub resolve_start_duration: Histogram<f64>,
    pub relative_age_seconds: Histogram<u64>,
    pub walk_duration: Histogram<f64>,
}

/// Zero-sized `InstrumentProvider` carrying the v1 service's metric
/// namespace. Standalone so the constructor can mint histograms before
/// `LoreRevisionV1Service` exists.
#[derive(Clone)]
struct RevisionServiceInstrumentProvider;

impl InstrumentProvider for RevisionServiceInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.revision.v1.revision_service"
    }
}

/// Dispatch struct for `lore.revision.v1.RevisionService`. Placeholder
/// methods are replaced one by one with real handlers backed by
/// `lore-revision` and `lore-storage` primitives.
#[derive(Clone)]
pub struct LoreRevisionV1Service {
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: Arc<HookDispatcher>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
    rpc_timeout: Duration,
    instrument_provider: RevisionServiceInstrumentProvider,
    revision_list_instruments: RevisionListInstruments,
}

impl LoreRevisionV1Service {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        notification: Arc<dyn NotificationSender>,
        hook_dispatcher: Arc<HookDispatcher>,
        history_step_size: u64,
        acceleration: crate::grpc::server::RevisionListAcceleration,
        forwarded_requests: Option<Arc<dyn ForwardedRequests>>,
        rpc_timeout: Duration,
    ) -> Self {
        let instrument_provider = RevisionServiceInstrumentProvider;
        let seconds_in_one_day = 86400f64;
        let revision_list_instruments = RevisionListInstruments {
            resolve_start_duration: instrument_provider
                .latency_histogram_ms("revision_list.resolve_start.duration"),
            relative_age_seconds: instrument_provider.length_histogram(
                "revision_list.resolve_start.relative_age_seconds",
                vec![
                    seconds_in_one_day / 24f64,
                    seconds_in_one_day / 2f64,
                    seconds_in_one_day,
                    seconds_in_one_day * 3f64,
                    seconds_in_one_day * 7f64,
                    seconds_in_one_day * 14f64,
                    seconds_in_one_day * 30f64,
                    seconds_in_one_day * 60f64,
                    seconds_in_one_day * 180f64,
                ],
            ),
            walk_duration: instrument_provider.latency_histogram_ms("revision_list.walk.duration"),
        };
        Self {
            immutable_store,
            mutable_store,
            notification,
            hook_dispatcher,
            history_step_size,
            acceleration,
            forwarded_requests,
            rpc_timeout,
            instrument_provider,
            revision_list_instruments,
        }
    }

    pub fn immutable_store(&self) -> &Arc<dyn lore_storage::ImmutableStore> {
        &self.immutable_store
    }

    pub fn mutable_store(&self) -> &Arc<dyn lore_storage::MutableStore> {
        &self.mutable_store
    }

    pub fn notification(&self) -> &Arc<dyn NotificationSender> {
        &self.notification
    }

    pub fn hook_dispatcher(&self) -> &Arc<HookDispatcher> {
        &self.hook_dispatcher
    }

    pub fn history_step_size(&self) -> u64 {
        self.history_step_size
    }
}

#[tonic::async_trait]
impl RevisionService for LoreRevisionV1Service {
    async fn branch_create(
        &self,
        request: Request<BranchCreateRequest>,
    ) -> Result<Response<BranchCreateResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_create::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.notification.clone(),
                &self.forwarded_requests,
                &self.hook_dispatcher,
                &self.instrument_provider,
            ),
        )
        .await
    }

    async fn branch_delete(
        &self,
        request: Request<BranchDeleteRequest>,
    ) -> Result<Response<BranchDeleteResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_delete::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                self.notification.clone(),
                &self.forwarded_requests,
                &self.hook_dispatcher,
                &self.instrument_provider,
            ),
        )
        .await
    }

    async fn branch_get(
        &self,
        request: Request<BranchGetRequest>,
    ) -> Result<Response<BranchGetResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            branch_get::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                &self.forwarded_requests,
            ),
        )
        .await
    }

    type BranchListStream = BranchListStream;

    async fn branch_list(
        &self,
        request: Request<BranchListRequest>,
    ) -> Result<Response<Self::BranchListStream>, Status> {
        branch_list::handler(
            request,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
        )
        .await
    }

    async fn branch_push(
        &self,
        request: Request<BranchPushRequest>,
    ) -> Result<Response<BranchPushResponse>, Status> {
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

    async fn branch_metadata_get(
        &self,
        request: Request<BranchMetadataGetRequest>,
    ) -> Result<Response<BranchMetadataGetResponse>, Status> {
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
        request: Request<BranchMetadataSetRequest>,
    ) -> Result<Response<BranchMetadataSetResponse>, Status> {
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

    async fn revision_list(
        &self,
        request: Request<RevisionListRequest>,
    ) -> Result<Response<RevisionListResponse>, Status> {
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

#[cfg(test)]
mod tests {
    use lore_proto::lore::revision::v1::revision_service_server::RevisionServiceServer;

    use super::*;

    /// Compile-time check that `LoreRevisionV1Service` fully implements
    /// the generated `RevisionService` trait — wrapping it in
    /// `RevisionServiceServer` requires the trait bound to hold.
    #[allow(dead_code)]
    fn assert_implements_trait(
        service: LoreRevisionV1Service,
    ) -> RevisionServiceServer<LoreRevisionV1Service> {
        RevisionServiceServer::new(service)
    }
}
