// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_proto::RebacApiClient as RebacApiGrpcClient;
use lore_proto::rebac::CreateResourceRequest;
use lore_proto::rebac::CreateResourceResponse;
use lore_proto::rebac::DeleteResourceRequest;
use lore_proto::rebac::DeleteResourceResponse;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use lore_transport::grpc::CorrelationInterceptor;
use opentelemetry::KeyValue;
use smallvec::SmallVec;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::codegen::InterceptedService;
use tonic::transport::ClientTlsConfig;

use crate::grpc::ServerResultExt;

pub type RebacApiResult<T> = Result<Response<T>, Status>;

#[async_trait::async_trait]
pub trait RebacApiClient {
    async fn create_resource(
        &mut self,
        request: Request<CreateResourceRequest>,
    ) -> RebacApiResult<CreateResourceResponse>;

    async fn delete_resource(
        &mut self,
        request: Request<DeleteResourceRequest>,
    ) -> RebacApiResult<DeleteResourceResponse>;
}

pub struct RebacClientHelper {
    client:
        RebacApiGrpcClient<InterceptedService<tonic::transport::Channel, CorrelationInterceptor>>,
}

impl RebacClientHelper {
    async fn new(auth_url: String) -> Result<Self, Status> {
        let mut endpoint = tonic::transport::Endpoint::from_shared(auth_url.clone())
            .warn_map_err(|_| Status::internal("Failed to create rebac endpoint"))?;
        if auth_url.starts_with("https://") {
            endpoint = endpoint
                .tls_config(
                    ClientTlsConfig::new()
                        .assume_http2(true)
                        .with_native_roots(),
                )
                .warn_map_err(|_| Status::internal("Failed to configure TLS for rebac"))?;
        }
        let channel = endpoint
            .connect()
            .await
            .warn_map_err(|_| Status::internal("Failed to connect to rebac service"))?;
        let client = RebacApiGrpcClient::with_interceptor(channel, CorrelationInterceptor);
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl RebacApiClient for RebacClientHelper {
    async fn create_resource(
        &mut self,
        request: Request<CreateResourceRequest>,
    ) -> RebacApiResult<CreateResourceResponse> {
        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("create_resource"),
            self.client.create_resource(request).await
        )
        .result
    }

    async fn delete_resource(
        &mut self,
        request: Request<DeleteResourceRequest>,
    ) -> RebacApiResult<DeleteResourceResponse> {
        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("delete_resource"),
            self.client.delete_resource(request).await
        )
        .result
    }
}

pub async fn grpc_get_rebac_client(auth_url: String) -> Result<RebacClientHelper, Status> {
    RebacClientHelper::new(auth_url).await
}

impl InstrumentProvider for RebacClientHelper {
    fn namespace(&self) -> &'static str {
        "urc.authnz.rebac"
    }
}
