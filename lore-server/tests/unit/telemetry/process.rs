// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod observe_file_descriptors {
    use std::fs::File;

    use lore_server::telemetry::FILE_DESCRIPTOR_LIMIT;
    use lore_server::telemetry::OPEN_FILE_DESCRIPTORS;
    use lore_server::telemetry::observe_file_descriptors;
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::PeriodicReader;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::data::MetricData;
    use opentelemetry_sdk::metrics::data::ResourceMetrics;

    fn gauge(metrics: &ResourceMetrics, name: &str) -> Option<u64> {
        metrics
            .scope_metrics()
            .flat_map(|scope| scope.metrics())
            .filter(|metric| metric.name() == name)
            .find_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                    gauge.data_points().next().map(|point| point.value())
                }
                _ => None,
            })
    }

    /// The soft limit as the OS reports it, or `None` when it is unlimited.
    fn soft_limit() -> Option<u64> {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit only writes the struct it is given.
        let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
        assert_eq!(result, 0, "getrlimit failed");
        (limit.rlim_cur != libc::RLIM_INFINITY).then_some(limit.rlim_cur)
    }

    /// Other tests in the process open and close descriptors concurrently, so the count is only
    /// bounded below by what this test holds.
    #[test]
    fn counts_held_descriptors_against_the_soft_limit() {
        const HELD: u64 = 64;
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        observe_file_descriptors(&provider.meter("test"));

        let held: Vec<File> = (0..HELD)
            .map(|_| File::open("/dev/null").expect("open /dev/null"))
            .collect();
        provider.force_flush().expect("flush metrics");
        let snapshots = exporter.get_finished_metrics().expect("exported metrics");
        drop(held);
        let metrics = snapshots.last().expect("a metrics snapshot");

        let open = gauge(metrics, OPEN_FILE_DESCRIPTORS).expect("an open descriptor sample");
        assert!(open >= HELD, "{open} open descriptors, holding {HELD}");
        let limit = gauge(metrics, FILE_DESCRIPTOR_LIMIT);
        assert_eq!(limit, soft_limit());
        if let Some(limit) = limit {
            assert!(
                open <= limit,
                "{open} open descriptors over a limit of {limit}"
            );
        }
    }
}
