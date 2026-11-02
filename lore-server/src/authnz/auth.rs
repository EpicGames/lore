// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::lore_spawn_net;
use lore_proto::auth::CheckUserPermissionRequest;
use lore_proto::auth::CheckUserPermissionResponse;
use lore_proto::auth::LookupUserPermissionsRequest;
use lore_proto::auth::LookupUserPermissionsResponse;
use lore_proto::auth::urc_auth_api_client::UrcAuthApiClient;
use lore_proto::lore::user::v1::repository_registry_service_client::RepositoryRegistryServiceClient;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use lore_transport::auth::ucs_auth::grpc_endpoint;
use lore_transport::grpc::CorrelationInterceptor;
use lore_transport::tls::grpc_client_tls_config;
use opentelemetry::KeyValue;
use smallvec::SmallVec;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::codegen::InterceptedService;
use tonic::transport::Channel;

use crate::grpc::ServerResultExt;

type LoreAuthApiResult<T> = Result<Response<T>, Status>;

pub struct LoreAuthClientHelper {
    client: UrcAuthApiClient<InterceptedService<tonic::transport::Channel, CorrelationInterceptor>>,
}

/// Builds the endpoint to dial for `auth_url`, whose scheme is the configured
/// one (`ucs-auth://`, `oidc://`) rather than one tonic can dial.
#[lore_macro::test_pub]
fn auth_endpoint(auth_url: &str) -> Result<tonic::transport::Endpoint, Status> {
    let auth_url = grpc_endpoint(auth_url);
    let mut endpoint = tonic::transport::Endpoint::from_shared(auth_url.clone())
        .warn_map_err(|_| Status::internal("Failed to create lore auth endpoint"))?;
    if auth_url.starts_with("https://") {
        endpoint = endpoint
            .tls_config(grpc_client_tls_config())
            .warn_map_err(|_| Status::internal("Failed to configure TLS for lore auth"))?;
    }
    Ok(endpoint)
}

async fn connect(url: &str, service: &str) -> Result<Channel, Status> {
    let endpoint = auth_endpoint(url)?;
    // Connect from net so the hyper/h2 driver tasks this spawns bind there
    // rather than to the core runtime the caller runs on.
    lore_spawn_net!(async move { endpoint.connect().await })
        .await
        .warn_map_err(|_| Status::internal(format!("{service} connection task failed")))?
        .warn_map_err(|_| Status::internal(format!("Failed to connect to {service}")))
}

impl LoreAuthClientHelper {
    async fn new(auth_url: String) -> Result<LoreAuthClientHelper, Status> {
        let channel = connect(&auth_url, "lore auth service").await?;
        let client = UrcAuthApiClient::with_interceptor(channel, CorrelationInterceptor);
        Ok(LoreAuthClientHelper { client })
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

/// A `lore.user.v1.RepositoryRegistryService` client for `user_url`, dialed
/// like `auth_url`.
pub async fn grpc_get_repository_registry_client(
    user_url: &str,
) -> Result<
    RepositoryRegistryServiceClient<InterceptedService<Channel, CorrelationInterceptor>>,
    Status,
> {
    let channel = connect(user_url, "repository registry").await?;
    Ok(RepositoryRegistryServiceClient::with_interceptor(
        channel,
        CorrelationInterceptor,
    ))
}

impl InstrumentProvider for LoreAuthClientHelper {
    fn namespace(&self) -> &'static str {
        "urc.authnz.urc_auth"
    }
}
