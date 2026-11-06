// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_proto::lore::environment::v1::Config;
use lore_proto::lore::environment::v1::Endpoint;
use lore_proto::lore::environment::v1::Environment;
use lore_proto::lore::environment::v1::EnvironmentGetRequest;
use lore_proto::lore::environment::v1::EnvironmentGetResponse;
use lore_proto::lore::environment::v1::Oidc;
use lore_proto::lore::environment::v1::environment_service_server::EnvironmentService;
use lore_storage::CompressionMode;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::instrument;
use tracing::warn;

fn endpoint_to_proto(endpoint: &Option<lore_transport::Endpoint>) -> Option<Endpoint> {
    endpoint.as_ref().map(|endpoint| Endpoint {
        auth_url: endpoint.auth_url.clone().unwrap_or_default(),
        repository_url: endpoint.repository_url.clone().unwrap_or_default(),
        storage_url: endpoint.storage_url.clone().unwrap_or_default(),
        revision_url: endpoint.revision_url.clone().unwrap_or_default(),
        lock_url: endpoint.lock_url.clone().unwrap_or_default(),
        notification_url: endpoint.notification_url.clone().unwrap_or_default(),
        user_url: endpoint.user_url.clone().unwrap_or_default(),
    })
}

fn config_to_proto(config: &Option<lore_revision::environment::Config>) -> Option<Config> {
    config.as_ref().map(|config| Config {
        max_query_batch: config.max_query_batch.unwrap_or_default() as u32,
        compression_mode: config.compression_mode.as_ref().map(|mode| {
            let v1_mode = match mode {
                CompressionMode::NotSpecified => {
                    lore_proto::lore::environment::v1::CompressionMode::NotSpecified
                }
                CompressionMode::NoCompression => {
                    lore_proto::lore::environment::v1::CompressionMode::NoCompression
                }
                CompressionMode::Lz4 => lore_proto::lore::environment::v1::CompressionMode::Lz4,
                CompressionMode::Oodle => {
                    warn!("Env config compression mode overridden from Oodle to Zstd");
                    lore_proto::lore::environment::v1::CompressionMode::Zstd
                }
                CompressionMode::Zstd => lore_proto::lore::environment::v1::CompressionMode::Zstd,
            };
            v1_mode as i32
        }),
    })
}

fn oidc_to_proto(oidc: &lore_transport::Oidc) -> Oidc {
    Oidc {
        issuer: oidc.issuer.clone(),
        client_id: oidc.client_id.clone(),
        scopes: oidc.scopes.clone(),
        preferred: oidc.preferred,
        resource_template: oidc.resource_template.clone().unwrap_or_default(),
        scope_template: oidc.scope_template.clone().unwrap_or_default(),
        token_exchange_issuer: oidc.token_exchange_issuer.clone().unwrap_or_default(),
        client_credentials_scopes: oidc.client_credentials_scopes.clone(),
        // `identity_claim` (8) is deprecated and left empty.
        ..Default::default()
    }
}

#[lore_macro::test_pub]
fn environment_to_proto(
    environment: &lore_revision::environment::EnvironmentConfig,
) -> Environment {
    Environment {
        endpoint: endpoint_to_proto(&environment.endpoint),
        config: config_to_proto(&environment.config),
        oidc: environment.oidc.as_ref().map(oidc_to_proto),
    }
}

/// `lore.environment.v1.EnvironmentService` dispatch struct. Carries the
/// pre-built `Environment` proto and a maintenance flag — both decided
/// once at server startup.
#[derive(Clone)]
pub struct LoreEnvironmentV1Service {
    environment: Environment,
    maintenance: bool,
}

impl LoreEnvironmentV1Service {
    pub fn new(environment: lore_revision::environment::EnvironmentConfig) -> Self {
        Self {
            environment: environment_to_proto(&environment),
            maintenance: false,
        }
    }

    pub fn maintenance(environment: lore_revision::environment::EnvironmentConfig) -> Self {
        Self {
            environment: environment_to_proto(&environment),
            maintenance: true,
        }
    }
}

#[tonic::async_trait]
impl EnvironmentService for LoreEnvironmentV1Service {
    #[instrument(name = "EnvironmentGet::v1::handle", skip_all)]
    async fn environment_get(
        &self,
        _request: Request<EnvironmentGetRequest>,
    ) -> Result<Response<EnvironmentGetResponse>, Status> {
        if self.maintenance {
            return Err(Status::unavailable("Server is in maintenance"));
        }
        Ok(Response::new(EnvironmentGetResponse {
            environment: Some(self.environment.clone()),
        }))
    }
}
