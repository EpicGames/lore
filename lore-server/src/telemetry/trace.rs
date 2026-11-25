// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::Duration;

use lore_error_set::prelude::*;
use lore_telemetry::ExporterConfig;
use lore_telemetry::TelemetryError;
use lore_telemetry::TraceConfig;
use lore_telemetry::tracing::fields::SAMPLING_TIER_LOW;
use opentelemetry::Context;
use opentelemetry::KeyValue;
use opentelemetry::trace::Link;
use opentelemetry::trace::SamplingResult;
use opentelemetry::trace::SpanKind;
use opentelemetry::trace::TraceId;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::BatchConfigBuilder;
use opentelemetry_sdk::trace::BatchSpanProcessor;
use opentelemetry_sdk::trace::RandomIdGenerator;
use opentelemetry_sdk::trace::Sampler;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::ShouldSample;
use tokio::runtime::Handle;
use tracing::debug;
use tracing::error;

use super::on_net_runtime;
use super::resource::resource;
use super::resource_provider::ResourceDetectorProvider;

static TRACER_PROVIDER: OnceLock<RwLock<Arc<SdkTracerProvider>>> = OnceLock::new();

fn tracer_provider_lock() -> &'static RwLock<Arc<SdkTracerProvider>> {
    TRACER_PROVIDER.get_or_init(|| RwLock::new(Arc::new(SdkTracerProvider::builder().build())))
}

/// Sets the global tracer provider.
pub fn set_tracer_provider(provider: SdkTracerProvider) {
    if let Ok(ref mut tracer_provider) = tracer_provider_lock().write() {
        **tracer_provider = Arc::new(provider);
        debug!("Set new tracer provider");
    } else {
        error!("Failed to obtain write lock to set tracer provider");
    }
}

/// Gets a reference to the global tracer provider.
#[allow(dead_code)]
pub fn tracer_provider() -> Arc<SdkTracerProvider> {
    if let Ok(provider) = tracer_provider_lock().read() {
        provider.clone()
    } else {
        error!("Failed to obtain read lock for tracer provider, returning a no-op provider");
        Arc::new(SdkTracerProvider::builder().build())
    }
}

#[lore_macro::test_pub]
#[derive(Clone, Debug)]
struct PerOpSampler {
    low_tier: Sampler,
    default_: Sampler,
}

impl PerOpSampler {
    #[lore_macro::test_pub]
    fn new(default_rate: f64, low_tier_rate: f64) -> Self {
        Self {
            low_tier: Sampler::TraceIdRatioBased(low_tier_rate),
            default_: Sampler::TraceIdRatioBased(default_rate),
        }
    }
}

impl ShouldSample for PerOpSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        let is_low_tier = attributes.iter().any(|kv| {
            kv.key.as_str() == SAMPLING_TIER_LOW
                && matches!(kv.value, opentelemetry::Value::Bool(true))
        });
        let inner = if is_low_tier {
            &self.low_tier
        } else {
            &self.default_
        };
        inner.should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}

/// Builds the OTLP span exporter, on the net runtime.
#[lore_macro::test_pub]
fn span_exporter(exporter_config: &ExporterConfig) -> Result<SpanExporter, TelemetryError> {
    let exporter = on_net_runtime(|| {
        SpanExporter::builder()
            .with_tonic()
            .with_endpoint(exporter_config.endpoint.clone())
            .with_timeout(Duration::from_millis(exporter_config.timeout))
            .build()
    })
    .internal("Failed to build OTLP span exporter")?;
    Ok(exporter)
}

/// Initializes an OTLP tracer provider for exporting traces.
pub fn init_tracer_provider(
    exporter_config: &ExporterConfig,
    trace_config: &TraceConfig,
    additional_labels: &Option<HashMap<String, String>>,
    runtime_handle: Handle,
    resource_detector_provider: Option<&dyn ResourceDetectorProvider>,
) -> Result<SdkTracerProvider, TelemetryError> {
    let sampler = Sampler::ParentBased(Box::new(PerOpSampler::new(
        trace_config.sample_rate,
        trace_config.sample_rate_low_tier,
    )));

    let exporter = span_exporter(exporter_config)?;

    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(exporter_config.queue_size)
        .build();

    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();

    let tracer_provider = SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_sampler(sampler)
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource(
            additional_labels,
            runtime_handle,
            resource_detector_provider,
        ))
        .build();

    set_tracer_provider(tracer_provider.clone());

    Ok(tracer_provider)
}
