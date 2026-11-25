// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::time::Duration;

use lore_error_set::prelude::*;
use lore_telemetry::ExporterConfig;
use lore_telemetry::MetricsConfig;
use lore_telemetry::TelemetryError;
use lore_telemetry::set_meter_provider;
use opentelemetry_otlp::MetricExporter;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use tokio::runtime::Handle;
use tracing::debug;
use tracing::warn;

use super::on_net_runtime;
use super::resource::resource;
use super::resource_provider::ResourceDetectorProvider;

/// Builds the OTLP metric exporter, on the net runtime.
#[lore_macro::test_pub]
fn metric_exporter(exporter_config: &ExporterConfig) -> Result<MetricExporter, TelemetryError> {
    let exporter = on_net_runtime(|| {
        MetricExporter::builder()
            .with_tonic()
            .with_endpoint(exporter_config.endpoint.clone())
            .with_timeout(Duration::from_millis(exporter_config.timeout))
            .build()
    })
    .internal("Failed to build OTLP metric exporter")?;
    Ok(exporter)
}

/// Initializes an OTLP meter provider for exporting metrics.
///
/// Creates a meter provider configured with:
/// - OTLP exporter using gRPC/tonic
/// - Periodic reader with configurable export interval
/// - Resource attributes from the environment and configuration
///
/// Also sets this provider as the global meter provider via `set_meter_provider`.
pub fn init_meter_provider(
    exporter_config: &ExporterConfig,
    metrics_config: &MetricsConfig,
    additional_labels: &Option<HashMap<String, String>>,
    runtime_handle: Handle,
    resource_detector_provider: Option<&dyn ResourceDetectorProvider>,
) -> Result<SdkMeterProvider, TelemetryError> {
    let exporter = metric_exporter(exporter_config)?;

    let reader = PeriodicReader::builder(LoggedMetricExporter(exporter))
        .with_interval(Duration::from_millis(metrics_config.sample_interval_millis))
        .build();

    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource(
            additional_labels,
            runtime_handle,
            resource_detector_provider,
        ))
        .with_reader(reader)
        .build();

    debug!("Setting meter provider");
    set_meter_provider(meter_provider.clone());

    Ok(meter_provider)
}

/// A metric exporter that logs each failed export at warn.
///
/// The SDK's periodic reader reports a failed export only at debug, so without this an instance
/// that has lost its collector stops sending metrics without a trace in its own logs.
#[lore_macro::test_pub]
struct LoggedMetricExporter<E>(E);

impl<E: PushMetricExporter> PushMetricExporter for LoggedMetricExporter<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.0.export(metrics).await;
        if let Err(error) = &result {
            warn!(error = %error, "Failed to export metrics");
        }
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.0.temporality()
    }
}
