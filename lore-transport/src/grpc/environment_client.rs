// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::error::Maintenance;
use lore_base::lore_warn;
use lore_proto::lore::environment::v1::EnvironmentGetRequest;
use lore_proto::lore::environment::v1::environment_service_client::EnvironmentServiceClient;

use super::Channel;
use super::CorrelationInterceptor;
use super::UnauthenticatedService;
use super::grpc_retry;
use super::handle_error;
use crate::error::ProtocolError;
use crate::types::CompressionMode;
use crate::types::Endpoint;
use crate::types::EnvironmentConfig;
use crate::types::EnvironmentServerConfig;

impl From<lore_proto::lore::environment::v1::Environment> for EnvironmentConfig {
    fn from(value: lore_proto::lore::environment::v1::Environment) -> Self {
        Self {
            endpoint: value.endpoint.map(|endpoint| Endpoint {
                auth_url: if !endpoint.auth_url.is_empty() {
                    Some(endpoint.auth_url.clone())
                } else {
                    None
                },
                repository_url: if !endpoint.repository_url.is_empty() {
                    Some(endpoint.repository_url.clone())
                } else {
                    None
                },
                storage_url: if !endpoint.storage_url.is_empty() {
                    Some(endpoint.storage_url.clone())
                } else {
                    None
                },
                revision_url: if !endpoint.revision_url.is_empty() {
                    Some(endpoint.revision_url.clone())
                } else {
                    None
                },
                lock_url: if !endpoint.lock_url.is_empty() {
                    Some(endpoint.lock_url.clone())
                } else {
                    None
                },
                notification_url: if !endpoint.notification_url.is_empty() {
                    Some(endpoint.notification_url.clone())
                } else {
                    None
                },
            }),
            config: value.config.map(|config| EnvironmentServerConfig {
                max_query_batch: if config.max_query_batch > 0 {
                    Some(config.max_query_batch as usize)
                } else {
                    None
                },
                compression_mode: config
                    .compression_mode
                    .map(|mode| CompressionMode::from_u32(mode as u32)),
            }),
        }
    }
}

#[derive(Clone)]
pub struct EnvironmentService {
    client: EnvironmentServiceClient<UnauthenticatedService>,
}

impl EnvironmentService {
    pub fn new(channel: Channel) -> Self {
        let client = EnvironmentServiceClient::with_interceptor(channel, CorrelationInterceptor);

        Self { client }
    }

    /// Fetches the environment configuration from the remote server.
    ///
    /// Returns `ProtocolError::Maintenance` when the server signals maintenance mode,
    /// or `ProtocolError::Internal` when the response is missing environment data.
    pub async fn get(&self) -> Result<EnvironmentConfig, ProtocolError> {
        let mut retry = grpc_retry();
        let response = loop {
            let request = EnvironmentGetRequest {};

            let mut client = self.client.clone();

            match client.environment_get(request).await {
                Ok(response) => {
                    break response.into_inner();
                }
                Err(status)
                    if status.code() == tonic::Code::Unavailable
                        && status
                            .message()
                            .to_ascii_lowercase()
                            .contains("maintenance") =>
                {
                    lore_warn!("Server in maintenance mode: {}", status.message());
                    return Err(ProtocolError::from(Maintenance));
                }
                Err(status) => {
                    handle_error(&mut retry, status).await?;
                }
            }
        };

        if let Some(environment) = response.environment {
            Ok(environment.into())
        } else {
            Err(ProtocolError::internal(
                "get: No environment config data in response",
            ))
        }
    }
}
