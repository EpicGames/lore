// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_server::telemetry::metrics::*;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;

use crate::log_capture::CapturedLogs;
use crate::telemetry::assert_connects_while_the_core_blocking_pool_is_held;

/// An exporter whose every export fails as an unreachable collector's would.
struct Unreachable;

impl PushMetricExporter for Unreachable {
    async fn export(&self, _metrics: &ResourceMetrics) -> OTelSdkResult {
        Err(OTelSdkError::InternalFailure(
            "collector unreachable".into(),
        ))
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}

mod metric_exporter {
    use super::*;

    /// The metric exporter reaches a collector named by hostname while every core blocking thread
    /// is held.
    #[test]
    fn connects_by_hostname_while_the_core_blocking_pool_is_held() {
        assert_connects_while_the_core_blocking_pool_is_held(|config| {
            let exporter = metric_exporter(config).expect("building the exporter");
            Box::new(move || {
                let _ = futures::executor::block_on(exporter.export(&ResourceMetrics::default()));
            })
        });
    }
}

mod logged_metric_exporter {
    use super::*;

    /// A failed export reaches the reader unchanged and is logged at warn.
    #[test]
    fn a_failed_export_is_logged_at_warn() {
        let (logs, _capture) = CapturedLogs::start();

        let error = futures::executor::block_on(
            LoggedMetricExporter(Unreachable).export(&ResourceMetrics::default()),
        )
        .expect_err("the export fails");

        assert!(
            matches!(&error, OTelSdkError::InternalFailure(message) if message == "collector unreachable"),
            "unexpected error: {error:?}"
        );
        let lines = logs.lines();
        let line = lines
            .iter()
            .find(|line| line["fields"]["message"] == "Failed to export metrics")
            .unwrap_or_else(|| panic!("no export failure logged: {lines:?}"));
        assert_eq!(line["level"], "WARN");
        assert_eq!(
            line["fields"]["error"],
            "Operation failed: collector unreachable"
        );
    }
}
