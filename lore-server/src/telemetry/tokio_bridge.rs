// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
#[cfg(tokio_unstable)]
use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;
use opentelemetry::metrics::Meter;
#[cfg(tokio_unstable)]
use tokio::runtime::Handle;
#[cfg(tokio_unstable)]
use tokio_metrics::RuntimeMetrics;
use tokio_metrics::TaskMetrics;
use tracing::trace;

/// OpenTelemetry bridge for Tokio runtime metrics.
///
/// Records various Tokio runtime statistics as OpenTelemetry metrics including:
/// - Worker thread statistics (busy duration, park counts, etc.)
/// - Task scheduling metrics
/// - Queue depths
/// - I/O driver statistics
///
/// One instance monitors one runtime. Lore runs two — core and net — so every
/// metric is tagged with the `labels` this bridge was built with (`runtime=core`
/// or `runtime=net`) and the instrument names are shared between them. A query
/// that does not filter on `runtime` therefore aggregates both.
#[cfg(tokio_unstable)]
pub struct OtelTokioRuntimeMetrics {
    labels: Vec<KeyValue>,

    /// Handle to the runtime this bridge monitors, for the metrics reachable only
    /// through it. Held rather than taken from `Handle::current()`, which yields
    /// whichever runtime the recording task runs on.
    handle: Handle,

    budget_forced_yield_count: Gauge<u64>,
    elapsed: Histogram<u64>,
    io_driver_ready_count: Gauge<u64>,
    global_queue_depth: Gauge<u64>,
    max_busy_duration: Histogram<u64>,
    max_local_queue_depth: Gauge<u64>,
    max_local_schedule_count: Gauge<u64>,
    max_noop_count: Gauge<u64>,
    max_overflow_count: Gauge<u64>,
    max_polls_count: Gauge<u64>,
    max_steal_count: Gauge<u64>,
    max_steal_operations: Gauge<u64>,
    min_busy_duration: Histogram<u64>,
    min_local_queue_depth: Gauge<u64>,
    min_local_schedule_count: Gauge<u64>,
    min_noop_count: Gauge<u64>,
    min_overflow_count: Gauge<u64>,
    max_park_count: Gauge<u64>,
    min_park_count: Gauge<u64>,
    min_polls_count: Gauge<u64>,
    min_steal_count: Gauge<u64>,
    num_active_tasks: Gauge<u64>,
    num_blocking_threads: Gauge<u64>,
    num_idle_blocking_threads: Gauge<u64>,
    num_remote_schedules: Gauge<u64>,
    spawned_tasks_count: Gauge<u64>,
    total_busy_duration: Histogram<u64>,
    total_local_queue_depth: Gauge<u64>,
    total_local_schedule_count: Gauge<u64>,
    total_noop_count: Gauge<u64>,
    total_overflow_count: Gauge<u64>,
    total_park_count: Gauge<u64>,
    total_polls_count: Gauge<u64>,
    total_steal_count: Gauge<u64>,
    total_steal_operations: Gauge<u64>,
    workers_count: Gauge<u64>,
    mean_polls_per_park: Gauge<f64>,
    busy_ratio: Gauge<f64>,
}

#[cfg(tokio_unstable)]
impl OtelTokioRuntimeMetrics {
    pub fn new(meter: &Meter, handle: Handle, labels: Vec<KeyValue>) -> Self {
        Self {
            labels,
            handle,
            budget_forced_yield_count: meter.u64_gauge("budget_forced_yield_count").build(),
            elapsed: meter.u64_histogram("elapsed").build(),
            io_driver_ready_count: meter.u64_gauge("io_driver_ready_count").build(),
            global_queue_depth: meter.u64_gauge("injection_queue_depth").build(),
            max_busy_duration: meter.u64_histogram("max_busy_duration").build(),
            max_local_queue_depth: meter.u64_gauge("max_local_queue_depth").build(),
            max_local_schedule_count: meter.u64_gauge("max_local_schedule_count").build(),
            max_noop_count: meter.u64_gauge("max_noop_count").build(),
            max_overflow_count: meter.u64_gauge("max_overflow_count").build(),
            max_park_count: meter.u64_gauge("max_park_count").build(),
            max_polls_count: meter.u64_gauge("max_polls_count").build(),
            max_steal_count: meter.u64_gauge("max_steal_count").build(),
            max_steal_operations: meter.u64_gauge("max_steal_operations").build(),
            min_busy_duration: meter.u64_histogram("min_busy_duration").build(),
            min_local_queue_depth: meter.u64_gauge("min_local_queue_depth").build(),
            min_local_schedule_count: meter.u64_gauge("min_local_schedule_count").build(),
            min_noop_count: meter.u64_gauge("min_noop_count").build(),
            min_overflow_count: meter.u64_gauge("min_overflow_count").build(),
            min_park_count: meter.u64_gauge("min_park_count").build(),
            min_polls_count: meter.u64_gauge("min_polls_count").build(),
            min_steal_count: meter.u64_gauge("min_steal_count").build(),
            num_active_tasks: meter.u64_gauge("num_active_tasks").build(),
            num_blocking_threads: meter.u64_gauge("num_blocking_threads").build(),
            num_idle_blocking_threads: meter.u64_gauge("num_idle_blocking_threads").build(),
            num_remote_schedules: meter.u64_gauge("num_remote_schedules").build(),
            spawned_tasks_count: meter.u64_gauge("spawned_task_count").build(),
            total_busy_duration: meter.u64_histogram("total_busy_duration").build(),
            total_local_queue_depth: meter.u64_gauge("total_local_queue_depth").build(),
            total_local_schedule_count: meter.u64_gauge("total_local_schedule_count").build(),
            total_noop_count: meter.u64_gauge("total_noop_count").build(),
            total_overflow_count: meter.u64_gauge("total_overflow_count").build(),
            total_park_count: meter.u64_gauge("total_park_count").build(),
            total_polls_count: meter.u64_gauge("total_polls_count").build(),
            total_steal_count: meter.u64_gauge("total_steal_count").build(),
            total_steal_operations: meter.u64_gauge("total_steal_operations").build(),
            workers_count: meter.u64_gauge("workers_count").build(),
            mean_polls_per_park: meter.f64_gauge("mean_polls_per_park").build(),
            busy_ratio: meter.f64_gauge("busy_ratio").build(),
        }
    }

    pub fn record(&self, metrics: RuntimeMetrics) {
        macro_rules! record {
            ( $field:ident, "gauge" ) => {{
                let data = metrics.$field as u64;
                trace!(
                    "Recording tokio runtime metric {} as gauge with value {}",
                    stringify!($field),
                    data
                );
                self.$field.record(data, &self.labels);
            }};
            ( $field:ident, "float gauge" ) => {{
                let data = metrics.$field();
                trace!(
                    "Recording tokio runtime metric {} as derived counter with value {}",
                    stringify!($field),
                    data
                );
                self.$field.record(data, &self.labels);
            }};
            ( $field:ident, "histogram" ) => {{
                let data = metrics.$field.as_millis() as u64;
                trace!(
                    "Recording tokio runtime metric {} as histogram with value {}",
                    stringify!($field),
                    data
                );
                self.$field.record(data, &self.labels)
            }};
        }

        record!(budget_forced_yield_count, "gauge");
        record!(elapsed, "histogram");
        record!(io_driver_ready_count, "gauge");
        record!(global_queue_depth, "gauge");
        record!(max_busy_duration, "histogram");
        record!(max_local_queue_depth, "gauge");
        record!(max_local_schedule_count, "gauge");
        record!(max_noop_count, "gauge");
        record!(max_overflow_count, "gauge");
        record!(max_park_count, "gauge");
        record!(max_polls_count, "gauge");
        record!(max_steal_count, "gauge");
        record!(max_steal_operations, "gauge");
        record!(min_busy_duration, "histogram");
        record!(min_local_queue_depth, "gauge");
        record!(min_local_schedule_count, "gauge");
        record!(min_noop_count, "gauge");
        record!(min_overflow_count, "gauge");
        record!(min_park_count, "gauge");
        record!(min_polls_count, "gauge");
        record!(min_steal_count, "gauge");
        record!(num_remote_schedules, "gauge");
        record!(total_busy_duration, "histogram");
        record!(total_local_queue_depth, "gauge");
        record!(total_local_schedule_count, "gauge");
        record!(total_noop_count, "gauge");
        record!(total_overflow_count, "gauge");
        record!(total_park_count, "gauge");
        record!(total_polls_count, "gauge");
        record!(total_steal_count, "gauge");
        record!(total_steal_operations, "gauge");
        record!(workers_count, "gauge");
        record!(mean_polls_per_park, "float gauge");
        record!(busy_ratio, "float gauge");

        let m = self.handle.metrics();

        self.num_active_tasks
            .record(m.num_alive_tasks() as u64, &self.labels);
        self.num_blocking_threads
            .record(m.num_blocking_threads() as u64, &self.labels);
        self.num_idle_blocking_threads
            .record(m.num_idle_blocking_threads() as u64, &self.labels);
        self.spawned_tasks_count
            .record(m.spawned_tasks_count(), &self.labels);
    }
}

/// OpenTelemetry bridge for Tokio task metrics.
///
/// Records various Tokio task statistics as OpenTelemetry metrics including:
/// - Task counts (instrumented, dropped, first poll)
/// - Timing metrics (poll duration, idle duration, scheduled duration)
/// - Derived metrics (slow poll ratio, long delay ratio, etc.)
pub struct OtelTokioTaskMetrics {
    labels: Vec<KeyValue>,

    instrumented_count: Counter<u64>,
    dropped_count: Counter<u64>,
    first_poll_count: Counter<u64>,
    total_first_poll_delay: Histogram<u64>,
    total_idled_count: Counter<u64>,
    total_idle_duration: Histogram<u64>,
    total_scheduled_count: Counter<u64>,
    total_scheduled_duration: Histogram<u64>,
    total_poll_count: Counter<u64>,
    total_poll_duration: Histogram<u64>,
    total_fast_poll_count: Counter<u64>,
    total_fast_poll_duration: Histogram<u64>,
    total_slow_poll_count: Counter<u64>,
    total_slow_poll_duration: Histogram<u64>,
    total_short_delay_count: Counter<u64>,
    total_short_delay_duration: Histogram<u64>,
    total_long_delay_count: Counter<u64>,
    total_long_delay_duration: Histogram<u64>,

    // derived metrics
    mean_first_poll_delay: Histogram<u64>,
    mean_idle_duration: Histogram<u64>,
    mean_scheduled_duration: Histogram<u64>,
    mean_poll_duration: Histogram<u64>,
    slow_poll_ratio: Histogram<f64>,
    mean_fast_poll_duration: Histogram<u64>,
    mean_slow_poll_duration: Histogram<u64>,
    long_delay_ratio: Histogram<f64>,
    mean_short_delay_duration: Histogram<u64>,
    mean_long_delay_duration: Histogram<u64>,
}

impl OtelTokioTaskMetrics {
    pub fn new(meter: &Meter, labels: Vec<KeyValue>) -> Self {
        Self {
            labels,
            instrumented_count: meter.u64_counter("instrumented_count").build(),
            dropped_count: meter.u64_counter("dropped_count").build(),
            first_poll_count: meter.u64_counter("first_poll_count").build(),
            total_first_poll_delay: meter.u64_histogram("total_first_poll_delay").build(),
            total_idled_count: meter.u64_counter("total_idled_count").build(),
            total_idle_duration: meter.u64_histogram("total_idle_duration").build(),
            total_scheduled_count: meter.u64_counter("total_scheduled_count").build(),
            total_scheduled_duration: meter.u64_histogram("total_scheduled_duration").build(),
            total_poll_count: meter.u64_counter("total_poll_count").build(),
            total_poll_duration: meter.u64_histogram("total_poll_duration").build(),
            total_fast_poll_count: meter.u64_counter("total_fast_poll_count").build(),
            total_fast_poll_duration: meter.u64_histogram("total_fast_poll_duration").build(),
            total_slow_poll_count: meter.u64_counter("total_slow_poll_count").build(),
            total_slow_poll_duration: meter.u64_histogram("total_slow_poll_duration").build(),
            total_short_delay_count: meter.u64_counter("total_short_delay_count").build(),
            total_short_delay_duration: meter.u64_histogram("total_short_delay_duration").build(),
            total_long_delay_count: meter.u64_counter("total_long_delay_count").build(),
            total_long_delay_duration: meter.u64_histogram("total_long_delay_duration").build(),
            mean_first_poll_delay: meter.u64_histogram("mean_first_poll_delay").build(),
            mean_idle_duration: meter.u64_histogram("mean_idle_duration").build(),
            mean_scheduled_duration: meter.u64_histogram("mean_scheduled_duration").build(),
            mean_poll_duration: meter.u64_histogram("mean_poll_duration").build(),
            slow_poll_ratio: meter.f64_histogram("slow_poll_ratio").build(),
            mean_fast_poll_duration: meter.u64_histogram("mean_fast_poll_duration").build(),
            mean_slow_poll_duration: meter.u64_histogram("mean_slow_poll_duration").build(),
            long_delay_ratio: meter.f64_histogram("long_delay_ratio").build(),
            mean_short_delay_duration: meter.u64_histogram("mean_short_delay_duration").build(),
            mean_long_delay_duration: meter.u64_histogram("mean_long_delay_duration").build(),
        }
    }

    pub fn record(&self, metrics: TaskMetrics) {
        macro_rules! record {
            ( $field:ident, "counter" ) => {{
                let data = metrics.$field as u64;
                trace!(
                    "Recording tokio task metric {} as gauge with value {:?}",
                    stringify!($field),
                    data
                );
                self.$field.add(data, &self.labels);
            }};
            ( $field:ident, "histogram" ) => {{
                let data = metrics.$field.as_millis() as u64;
                trace!(
                    "Recording tokio task metric {} as histogram with value {:?}",
                    stringify!($field),
                    data
                );
                self.$field.record(data, &self.labels)
            }};
            ( $field:ident, "derived histogram" ) => {{
                let data = metrics.$field().as_millis() as u64;
                trace!(
                    "Recording tokio task metric {} as derived histogram with value {:?}",
                    stringify!($field),
                    data
                );
                self.$field.record(data, &self.labels);
            }};
            ( $field:ident, "ratio" ) => {{
                let data = metrics.$field();
                trace!(
                    "Recording tokio task metric {} as ratio with value {:?}",
                    stringify!($field),
                    data
                );
                self.$field.record(data, &self.labels);
            }};
        }

        record!(instrumented_count, "counter");
        record!(dropped_count, "counter");
        record!(first_poll_count, "counter");
        record!(total_first_poll_delay, "histogram");
        record!(total_idled_count, "counter");
        record!(total_idle_duration, "histogram");
        record!(total_scheduled_count, "counter");
        record!(total_scheduled_duration, "histogram");
        record!(total_poll_count, "counter");
        record!(total_poll_duration, "histogram");
        record!(total_fast_poll_count, "counter");
        record!(total_fast_poll_duration, "histogram");
        record!(total_slow_poll_count, "counter");
        record!(total_slow_poll_duration, "histogram");
        record!(total_short_delay_count, "counter");
        record!(total_short_delay_duration, "histogram");
        record!(total_long_delay_count, "counter");
        record!(total_long_delay_duration, "histogram");

        // derived metrics
        record!(mean_first_poll_delay, "derived histogram");
        record!(mean_idle_duration, "derived histogram");
        record!(mean_scheduled_duration, "derived histogram");
        record!(mean_poll_duration, "derived histogram");
        record!(slow_poll_ratio, "ratio");
        record!(mean_fast_poll_duration, "derived histogram");
        record!(mean_slow_poll_duration, "derived histogram");
        record!(long_delay_ratio, "ratio");
        record!(mean_short_delay_duration, "derived histogram");
        record!(mean_long_delay_duration, "derived histogram");
    }
}
