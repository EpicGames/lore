// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Runs AWS SDK HTTP requests on the net runtime.
//!
//! The SDK's pooled hyper client spawns a connection driver task per connection,
//! lazily, while the request future is being polled — and it spawns it through
//! its own executor, so it lands on whatever runtime is current at that moment.
//! Store calls are issued from core, so without this wrapper every S3 and
//! `DynamoDB` connection driver would sit on core and, because connections are
//! pooled, stay there for the connection's life.
//!
//! Wrapping the *connector* handed to hyper is not enough: that only covers the
//! TCP/TLS handshake, since hyper spawns the driver itself rather than through
//! the connector. The wrap has to be at the [`HttpConnector`] level, which is the
//! future hyper's `request()` is polled inside.
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::connector_metadata::ConnectorMetadata;
use aws_smithy_runtime_api::client::http::HttpClient;
use aws_smithy_runtime_api::client::http::HttpConnector;
use aws_smithy_runtime_api::client::http::HttpConnectorFuture;
use aws_smithy_runtime_api::client::http::HttpConnectorSettings;
use aws_smithy_runtime_api::client::http::SharedHttpConnector;
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponentsBuilder;
use aws_smithy_types::config_bag::ConfigBag;
use lore_base::lore_spawn_net;

/// Wraps an [`HttpClient`] so the connectors it hands out dispatch on net.
#[derive(Debug)]
pub struct NetHttpClient<C> {
    inner: C,
}

impl<C> NetHttpClient<C> {
    pub fn new(inner: C) -> Self {
        Self { inner }
    }
}

impl<C: HttpClient + 'static> HttpClient for NetHttpClient<C> {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(NetHttpConnector {
            inner: self.inner.http_connector(settings, components),
        })
    }

    fn validate_base_client_config(
        &self,
        runtime_components: &RuntimeComponentsBuilder,
        cfg: &ConfigBag,
    ) -> Result<(), BoxError> {
        self.inner
            .validate_base_client_config(runtime_components, cfg)
    }

    fn validate_final_config(
        &self,
        runtime_components: &RuntimeComponents,
        cfg: &ConfigBag,
    ) -> Result<(), BoxError> {
        self.inner.validate_final_config(runtime_components, cfg)
    }

    // Forwarded so the inner client's crate still shows up in the user agent.
    fn connector_metadata(&self) -> Option<ConnectorMetadata> {
        self.inner.connector_metadata()
    }
}

/// Dispatches each request as a net-runtime task.
#[derive(Debug)]
struct NetHttpConnector {
    inner: SharedHttpConnector,
}

impl HttpConnector for NetHttpConnector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let inner = self.inner.clone();
        HttpConnectorFuture::new(async move {
            match lore_spawn_net!(async move { inner.call(request).await }).await {
                Ok(result) => result,
                // Only a panic or an abort in the dispatch task; nothing aborts it.
                Err(join_error) => Err(ConnectorError::other(Box::new(join_error), None)),
            }
        })
    }
}
