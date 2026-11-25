// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::time::Duration;

use lore_error_set::prelude::*;
use lore_telemetry::ExporterConfig;
use lore_telemetry::TelemetryError;
use opentelemetry_otlp::LogExporter;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::BatchConfigBuilder;
use opentelemetry_sdk::logs::BatchLogProcessor;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use tokio::runtime::Handle;

use super::on_net_runtime;
use super::resource::resource;
use super::resource_provider::ResourceDetectorProvider;

/// Builds the OTLP log exporter, on the net runtime.
#[lore_macro::test_pub]
fn log_exporter(exporter_config: &ExporterConfig) -> Result<LogExporter, TelemetryError> {
    let exporter = on_net_runtime(|| {
        LogExporter::builder()
            .with_tonic()
            .with_endpoint(exporter_config.endpoint.clone())
            .with_timeout(Duration::from_millis(exporter_config.timeout))
            .build()
    })
    .internal("Failed to build OTLP log exporter")?;
    Ok(exporter)
}

/// Initializes an OTLP logger provider for exporting logs.
///
/// Creates a logger provider configured with:
/// - OTLP exporter using gRPC/tonic
/// - Batch processing for efficient export
/// - Resource attributes from the environment and configuration
pub fn init_logger_provider(
    exporter_config: &ExporterConfig,
    additional_labels: &Option<HashMap<String, String>>,
    runtime_handle: Handle,
    resource_detector_provider: Option<&dyn ResourceDetectorProvider>,
) -> Result<SdkLoggerProvider, TelemetryError> {
    let exporter = log_exporter(exporter_config)?;

    let processor = BatchLogProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(exporter_config.queue_size)
                .build(),
        )
        .build();

    let logger_provider = SdkLoggerProvider::builder()
        .with_log_processor(processor)
        .with_resource(resource(
            additional_labels,
            runtime_handle,
            resource_detector_provider,
        ))
        .build();

    Ok(logger_provider)
}
