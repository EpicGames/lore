// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lore_proto::lore::thin_client::v1::ContentDiffRequest;
use lore_proto::lore::thin_client::v1::ContentDiffResponse;
use lore_proto::lore::thin_client::v1::RevisionDiffRequest;
use lore_proto::lore::thin_client::v1::RevisionDiffResponse;
use lore_proto::lore::thin_client::v1::RevisionInfoRequest;
use lore_proto::lore::thin_client::v1::RevisionInfoResponse;
use lore_proto::lore::thin_client::v1::RevisionTreeRequest;
use lore_proto::lore::thin_client::v1::RevisionTreeResponse;
use lore_proto::lore::thin_client::v1::thin_client_service_server::ThinClientService;
use lore_telemetry::InstrumentProvider;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::codegen::tokio_stream::Stream;

use super::content_diff;
use super::revision_diff;
use super::revision_info;
use super::revision_tree;
use crate::grpc::timeout_grpc;

type ContentDiffStream =
    Pin<Box<dyn Stream<Item = Result<ContentDiffResponse, Status>> + Send + 'static>>;
type RevisionDiffStream =
    Pin<Box<dyn Stream<Item = Result<RevisionDiffResponse, Status>> + Send + 'static>>;
type RevisionTreeStream =
    Pin<Box<dyn Stream<Item = Result<RevisionTreeResponse, Status>> + Send + 'static>>;

/// Zero-sized `InstrumentProvider` carrying the v1 thin-client service's
/// metric namespace. Standalone so the constructor can mint instruments
/// before `LoreThinClientV1Service` exists, and so the service struct
/// stays free of trait bounds it does not own.
#[derive(Clone)]
pub(crate) struct ThinClientServiceInstrumentProvider;

impl InstrumentProvider for ThinClientServiceInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.thin_client.v1.thin_client_service"
    }
}

/// Dispatch struct for `lore.thin_client.v1.ThinClientService`. Method
/// bodies start as gRPC `Unimplemented` placeholders and are replaced by
/// real handlers backed by `lore-revision` and `lore-storage` primitives.
#[derive(Clone)]
pub struct LoreThinClientV1Service {
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    rpc_timeout: Duration,
    revision_diff_config: revision_diff::RevisionDiffConfig,
    #[allow(dead_code)]
    instrument_provider: ThinClientServiceInstrumentProvider,
}

impl LoreThinClientV1Service {
    pub fn new(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        rpc_timeout: Duration,
        revision_diff_config: revision_diff::RevisionDiffConfig,
    ) -> Self {
        Self {
            immutable_store,
            mutable_store,
            rpc_timeout,
            revision_diff_config,
            instrument_provider: ThinClientServiceInstrumentProvider,
        }
    }

    pub fn immutable_store(&self) -> &Arc<dyn lore_storage::ImmutableStore> {
        &self.immutable_store
    }

    pub fn mutable_store(&self) -> &Arc<dyn lore_storage::MutableStore> {
        &self.mutable_store
    }
}

#[tonic::async_trait]
impl ThinClientService for LoreThinClientV1Service {
    type ContentDiffStream = ContentDiffStream;

    async fn content_diff(
        &self,
        request: Request<ContentDiffRequest>,
    ) -> Result<Response<Self::ContentDiffStream>, Status> {
        content_diff::handler(request, self.immutable_store().clone()).await
    }

    async fn revision_info(
        &self,
        request: Request<RevisionInfoRequest>,
    ) -> Result<Response<RevisionInfoResponse>, Status> {
        timeout_grpc(
            self.rpc_timeout,
            revision_info::handler(
                request,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
            ),
        )
        .await
    }

    type RevisionDiffStream = RevisionDiffStream;

    async fn revision_diff(
        &self,
        request: Request<RevisionDiffRequest>,
    ) -> Result<Response<Self::RevisionDiffStream>, Status> {
        revision_diff::handler(
            request,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            self.revision_diff_config,
        )
        .await
    }

    type RevisionTreeStream = RevisionTreeStream;

    async fn revision_tree(
        &self,
        request: Request<RevisionTreeRequest>,
    ) -> Result<Response<Self::RevisionTreeStream>, Status> {
        revision_tree::handler(
            request,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use lore_proto::lore::thin_client::v1::thin_client_service_server::ThinClientServiceServer;

    use super::*;

    /// Compile-time check that `LoreThinClientV1Service` fully implements
    /// the generated `ThinClientService` trait — wrapping it in
    /// `ThinClientServiceServer` requires the trait bound to hold.
    #[allow(dead_code)]
    fn assert_implements_trait(
        service: LoreThinClientV1Service,
    ) -> ThinClientServiceServer<LoreThinClientV1Service> {
        ThinClientServiceServer::new(service)
    }
}
