// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_proto::auth::CheckUserPermissionRequest;
use lore_proto::auth::CheckUserPermissionResponse;
use lore_proto::auth::LookupUserPermissionsRequest;
use lore_proto::auth::LookupUserPermissionsResponse;
use lore_proto::auth::urc_auth_api_client::UrcAuthApiClient;
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

type LoreAuthApiResult<T> = Result<Response<T>, Status>;

pub struct LoreAuthClientHelper {
    client: UrcAuthApiClient<InterceptedService<tonic::transport::Channel, CorrelationInterceptor>>,
}

impl LoreAuthClientHelper {
    async fn new(auth_url: String) -> Result<Self, Status> {
        let mut endpoint = tonic::transport::Endpoint::from_shared(auth_url.clone())
            .warn_map_err(|_| Status::internal("Failed to create lore auth endpoint"))?;
        if auth_url.starts_with("https://") {
            endpoint = endpoint
                .tls_config(
                    ClientTlsConfig::new()
                        .assume_http2(true)
                        .with_native_roots(),
                )
                .warn_map_err(|_| Status::internal("Failed to configure TLS for lore auth"))?;
        }
        let channel = endpoint
            .connect()
            .await
            .warn_map_err(|_| Status::internal("Failed to connect to lore auth service"))?;
        let client = UrcAuthApiClient::with_interceptor(channel, CorrelationInterceptor);
        Ok(Self { client })
    }

    pub async fn lookup_user_permissions(
        &mut self,
        request: Request<LookupUserPermissionsRequest>,
    ) -> LoreAuthApiResult<LookupUserPermissionsResponse> {
        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("lookup_user_permissions"),
            self.client.lookup_user_permissions(request).await
        )
        .result
    }

    pub async fn check_user_permission(
        &mut self,
        request: Request<CheckUserPermissionRequest>,
    ) -> LoreAuthApiResult<CheckUserPermissionResponse> {
        timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("check_user_permission"),
            self.client.check_user_permission(request).await
        )
        .result
    }
}

pub async fn grpc_get_auth_client(auth_url: String) -> Result<LoreAuthClientHelper, Status> {
    LoreAuthClientHelper::new(auth_url).await
}

impl InstrumentProvider for LoreAuthClientHelper {
    fn namespace(&self) -> &'static str {
        "urc.authnz.urc_auth"
    }
}
