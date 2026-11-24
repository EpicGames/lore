// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_io::IoDriver;
use lore_server::telemetry::register_io_pool_metrics;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;

struct Recorder {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl Recorder {
    fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        register_io_pool_metrics(&provider.meter("io_pool_test"));
        Self { provider, exporter }
    }

    /// Collects once, and returns the named instrument's single value from that collection.
    fn collect_u64(&self, name: &str) -> u64 {
        self.collect(name, |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                sum.data_points().next().map(|point| point.value())
            }
            AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                gauge.data_points().next().map(|point| point.value())
            }
            other => panic!("{name} is not a u64 sum or gauge: {other:?}"),
        })
    }

    fn collect_f64(&self, name: &str) -> f64 {
        self.collect(name, |data| match data {
            AggregatedMetrics::F64(MetricData::Sum(sum)) => {
                sum.data_points().next().map(|point| point.value())
            }
            AggregatedMetrics::F64(MetricData::Gauge(gauge)) => {
                gauge.data_points().next().map(|point| point.value())
            }
            other => panic!("{name} is not an f64 sum or gauge: {other:?}"),
        })
    }

    fn collect<T>(&self, name: &str, read: impl FnOnce(&AggregatedMetrics) -> Option<T>) -> T {
        self.provider.force_flush().expect("flush metrics");
        let snapshots = self
            .exporter
            .get_finished_metrics()
            .expect("exported metrics");
        let metric = snapshots
            .last()
            .expect("a metrics snapshot")
            .scope_metrics()
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == name)
            .unwrap_or_else(|| panic!("no {name} metric was exported"));
        let value = read(metric.data());
        value.unwrap_or_else(|| panic!("{name} has no data point"))
    }
}

/// Runs one operation on the process-wide syscall pool.
async fn use_the_pool() {
    let dir = lore_base::test_util::TempDir::new("io-pool-metrics-");
    IoDriver::global()
        .stat(dir.path())
        .await
        .expect("stat a directory that exists");
}

/// The pool's totals, read outside any collection.
fn totals() -> (u64, u64, f64) {
    let stats = lore_io::pool_stats();
    (
        stats.submitted,
        stats.started,
        stats.queue_wait.as_secs_f64(),
    )
}

mod register_io_pool_metrics {
    use super::*;

    /// A counter reports the pool's running total at each collection, and every instrument is
    /// exported under its name. Other tests share the process-wide pool, so each exported total
    /// is bounded by totals read on either side of its collection.
    #[tokio::test]
    async fn exports_the_pool_totals_and_gauges() {
        let recorder = Recorder::new();
        use_the_pool().await;

        let (submitted_before, started_before, wait_before) = totals();
        let submitted = recorder.collect_u64("lore.io.pool.submitted");
        let started = recorder.collect_u64("lore.io.pool.started");
        let queue_wait = recorder.collect_f64("lore.io.pool.queue_wait_seconds");
        let (submitted_after, started_after, wait_after) = totals();

        assert!(started_before >= 1, "a job has run");
        assert!((submitted_before..=submitted_after).contains(&submitted));
        assert!(
            (started_before..=started_after).contains(&started),
            "{started} exported, {started_before} to {started_after} started"
        );
        assert!(
            (wait_before..=wait_after).contains(&queue_wait),
            "{queue_wait} exported, {wait_before} to {wait_after} waited"
        );

        use_the_pool().await;
        let later = recorder.collect_u64("lore.io.pool.submitted");
        assert!(
            submitted < later,
            "{later} exported after {submitted} and another job"
        );

        assert_eq!(
            recorder.collect_u64("lore.io.pool.max_threads"),
            lore_io::pool_stats().max_threads as u64
        );
        for name in [
            "lore.io.pool.queued",
            "lore.io.pool.executing",
            "lore.io.pool.threads",
            "lore.io.pool.window_queue_high_water",
            "lore.io.pool.window_threads_high_water",
            "lore.io.pool.saturated",
            "lore.io.pool.spawn_refused",
        ] {
            recorder.collect_u64(name);
        }
        recorder.collect_f64("lore.io.pool.window_max_queue_wait_seconds");
    }
}
