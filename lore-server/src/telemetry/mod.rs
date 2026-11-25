// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Telemetry initialization module for lore-server.
//!
//! This module provides the server-side telemetry initialization including:
//! - Log, metrics, and trace providers
//! - OTLP export configuration
//! - Resource detection for AWS/Nomad environments
//! - Tokio runtime metrics bridging
//! - `lore-io` syscall pool metrics

mod guard;
mod io_pool;
#[cfg(not(feature = "test-util"))]
mod log;
#[cfg(feature = "test-util")]
pub mod log;
#[cfg(not(feature = "test-util"))]
mod metrics;
#[cfg(feature = "test-util")]
pub mod metrics;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process;
#[cfg(not(feature = "test-util"))]
mod protocol;
#[cfg(feature = "test-util")]
pub mod protocol;
#[cfg(not(feature = "test-util"))]
mod resource;
#[cfg(feature = "test-util")]
pub mod resource;
pub mod resource_provider;
mod tokio_bridge;
#[cfg(not(feature = "test-util"))]
mod trace;
#[cfg(feature = "test-util")]
pub mod trace;

use std::fs::File;

pub use guard::TelemetryGuard;
pub use io_pool::register_io_pool_metrics;
use lore_error_set::prelude::*;
use lore_telemetry::LogFormat;
use lore_telemetry::LogOutput;
use lore_telemetry::TelemetryConfig;
use lore_telemetry::TelemetryError;
pub use metrics::init_meter_provider;
use opentelemetry::trace::TracerProvider;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use process::FILE_DESCRIPTOR_LIMIT;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use process::OPEN_FILE_DESCRIPTORS;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use process::observe_file_descriptors;
pub use protocol::StorageProtocol;
pub use protocol::Transport;
pub use resource_provider::ResourceDetectorProvider;
use tokio::runtime::Handle;
#[cfg(tokio_unstable)]
pub use tokio_bridge::OtelTokioRuntimeMetrics;
pub use tokio_bridge::OtelTokioTaskMetrics;
use tracing_opentelemetry::MetricsLayer;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::Registry;

/// Builds the log filter from `RUST_LOG`-style directives.
///
/// Defaults to warnings, so a server started without `RUST_LOG` reports the
/// conditions an operator has to act on. An empty or unparsable set of
/// directives leaves that default in force.
#[lore_macro::test_pub]
fn log_filter(directives: &str) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .parse_lossy(directives)
}

/// Builds an OTLP exporter with the net runtime current.
///
/// An exporter's tonic channel runs its connection tasks on the runtime current when it is built,
/// and every reconnect resolves the collector's address on that runtime's blocking pool. Built on
/// net, export keeps working while long-running work holds the core blocking pool.
fn on_net_runtime<T>(build: impl FnOnce() -> T) -> T {
    let _net = lore_base::runtime::net_runtime().enter();
    build()
}

fn is_filtered_otel_name(name: &str) -> bool {
    matches!(
        name,
        "urc-quic" | "ReplicationClient::PutStreamImplementation"
    )
}

/// Initializes the telemetry stack for the Lore server.
///
/// The `TelemetryInitializer` sets up multiple layers:
/// 1. File/Stdout logging (json/ansi/text)
/// 2. Log export (OTLP)
/// 3. Metrics export (OTLP)
/// 4. Trace export (OTLP)
///
/// # Example
///
/// ```
/// let config = lore_telemetry::TelemetryConfig::new();
/// let guard = lore_server::telemetry::TelemetryInitializer::from_config(
///         &config, lore_revision::runtime::runtime(), None,
///     )
///     .expect("Failed to init telemetry")
///     .init()
///     .expect("Failed to init telemetry");
/// // Keep guard alive for the duration of the application
/// ```
pub struct TelemetryInitializer {
    guard: TelemetryGuard,
    layers: Vec<Box<dyn Layer<Registry> + Send + Sync>>,
}

impl TelemetryInitializer {
    /// Creates a new telemetry initializer from configuration.
    ///
    /// Building the OTLP exporters builds the net runtime, whose size is fixed from then on, so a
    /// process sizes its runtimes before calling this.
    ///
    /// # Arguments
    ///
    /// * `config` - Telemetry configuration specifying exporters, loggers, etc.
    /// * `runtime_handle` - Tokio runtime handle for async operations in resource detection.
    /// * `resource_detector_provider` - Optional provider for environment-specific resource
    ///   detectors. Pass `None` when no detectors are needed.
    ///
    /// # Returns
    ///
    /// A `TelemetryInitializer` ready to be initialized with additional layers.
    pub fn from_config(
        config: &TelemetryConfig,
        runtime_handle: Handle,
        resource_detector_provider: Option<&dyn ResourceDetectorProvider>,
    ) -> Result<Self, TelemetryError> {
        let logger_config = config.logger.clone().unwrap_or_default();

        let mut layers = vec![];

        // Setup non-blocking stdout writer
        let (writer, guard) = match logger_config.output {
            LogOutput::File(path) => {
                let log_file = File::create(&path).internal("Failed to create log file")?;

                tracing_appender::non_blocking(log_file)
            }
            LogOutput::Stderr => tracing_appender::non_blocking(std::io::stderr()),
            LogOutput::Stdout => tracing_appender::non_blocking(std::io::stdout()),
        };

        layers.push(match logger_config.format {
            LogFormat::Ansi => tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(writer)
                .boxed(),
            LogFormat::Json => tracing_ecs::ECSLayerBuilder::default()
                .normalize_json(false) // This is a performance optimization, better to normalize in collection pipelines
                .build_with_writer(writer)
                .boxed(),
            LogFormat::Text => tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .boxed(),
        });

        // Setup OTLP export if configured
        let telemetry_guard = if let Some(exporter_config) = &config.exporter {
            let logger_provider = if logger_config.enable_otlp {
                let logger_provider = log::init_logger_provider(
                    exporter_config,
                    &config.additional_labels,
                    runtime_handle.clone(),
                    resource_detector_provider,
                )?;
                layers.push(OpenTelemetryTracingBridge::new(&logger_provider).boxed());

                Some(logger_provider)
            } else {
                None
            };

            let meter_provider = if let Some(metrics_config) = &config.metrics {
                let meter_provider = init_meter_provider(
                    exporter_config,
                    metrics_config,
                    &config.additional_labels,
                    runtime_handle.clone(),
                    resource_detector_provider,
                )?;
                layers.push(MetricsLayer::new(meter_provider.clone()).boxed());

                Some(meter_provider)
            } else {
                None
            };

            let tracer_provider = if let Some(trace_config) = &config.traces {
                let service_name = match &trace_config.service_name {
                    Some(service_name) => service_name.clone(),
                    None => "lore".to_string(),
                };

                let tracer_provider = trace::init_tracer_provider(
                    exporter_config,
                    trace_config,
                    &config.additional_labels,
                    runtime_handle.clone(),
                    resource_detector_provider,
                )?;
                let tracer = tracer_provider.tracer(service_name);
                let otel_layer = OpenTelemetryLayer::new(tracer)
                    .with_filter(filter_fn(|m| !is_filtered_otel_name(m.name())));
                layers.push(otel_layer.boxed());

                Some(tracer_provider)
            } else {
                None
            };

            TelemetryGuard {
                guard,
                logger_provider,
                meter_provider,
                tracer_provider,
            }
        } else {
            TelemetryGuard {
                guard,
                logger_provider: None,
                meter_provider: None,
                tracer_provider: None,
            }
        };

        Ok(Self {
            guard: telemetry_guard,
            layers,
        })
    }

    /// Adds an additional tracing layer.
    ///
    /// # Arguments
    ///
    /// * `layer` - A tracing subscriber layer to add to the stack.
    pub fn with_layer<L>(mut self, layer: L) -> Self
    where
        L: Layer<Registry> + Send + Sync,
    {
        self.layers.push(layer.boxed());
        self
    }

    /// Initializes the telemetry stack and returns a guard.
    ///
    /// This method consumes the initializer and sets up the global tracing
    /// subscriber. The returned guard must be kept alive for the duration
    /// of the application to ensure telemetry providers are properly shut down.
    ///
    /// # Returns
    ///
    /// A `TelemetryGuard` that will clean up providers when dropped.
    pub fn init(self) -> Result<TelemetryGuard, TelemetryError> {
        tracing_subscriber::registry()
            .with(self.layers)
            .with(log_filter(
                &std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_default(),
            ))
            .try_init()
            .internal("Failed to initialize tracing subscriber")?;

        Ok(self.guard)
    }
}
