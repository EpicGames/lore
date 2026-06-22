// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TelemetryConfig {
    pub exporter: Option<ExporterConfig>,
    pub logger: Option<LoggerConfig>,
    pub metrics: Option<MetricsConfig>,
    pub traces: Option<TraceConfig>,
    pub additional_labels: Option<HashMap<String, String>>,
}

impl TelemetryConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_exporter(self, exporter: ExporterConfig) -> Self {
        Self {
            exporter: Some(exporter),
            ..self
        }
    }

    pub fn with_logger(self, logger: LoggerConfig) -> Self {
        Self {
            logger: Some(logger),
            ..self
        }
    }

    pub fn with_metrics(self, metrics: MetricsConfig) -> Self {
        Self {
            metrics: Some(metrics),
            ..self
        }
    }

    pub fn with_traces(self, traces: TraceConfig) -> Self {
        Self {
            traces: Some(traces),
            ..self
        }
    }

    pub fn with_additional_labels(self, additional_labels: HashMap<String, String>) -> Self {
        Self {
            additional_labels: Some(additional_labels),
            ..self
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExporterConfig {
    pub endpoint: String,
    pub queue_size: usize,
    pub timeout: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogOutput {
    File(String),
    Stderr,
    #[default]
    Stdout,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Ansi,
    Json,
    #[default]
    Text,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LoggerConfig {
    pub enable_otlp: bool,
    pub format: LogFormat,
    pub output: LogOutput,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetricsConfig {
    pub export_interval_millis: u64,
    pub sample_interval_millis: u64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            export_interval_millis: 30000,
            sample_interval_millis: 10000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TraceConfig {
    pub sample_rate: f64,
    #[serde(default = "TraceConfig::default_sample_rate_low_tier")]
    pub sample_rate_low_tier: f64,
    pub service_name: Option<String>,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            sample_rate: 0.05,
            sample_rate_low_tier: 0.001,
            service_name: None,
        }
    }
}

impl TraceConfig {
    fn default_sample_rate_low_tier() -> f64 {
        0.001
    }

    pub fn validate(&self) -> Result<(), TraceConfigError> {
        if !(0.0..=1.0).contains(&self.sample_rate) {
            return Err(OutOfRange {
                field: "sample_rate",
                value: self.sample_rate,
            }
            .into());
        }
        if !(0.0..=1.0).contains(&self.sample_rate_low_tier) {
            return Err(OutOfRange {
                field: "sample_rate_low_tier",
                value: self.sample_rate_low_tier,
            }
            .into());
        }
        Ok(())
    }
}

#[derive(Debug, Error, FfiError)]
#[error("trace config field {field} value {value} is outside [0.0, 1.0]")]
#[ffi_code(1)]
pub struct OutOfRange {
    pub field: &'static str,
    pub value: f64,
}

#[error_set]
pub enum TraceConfigError {
    OutOfRange,
}
