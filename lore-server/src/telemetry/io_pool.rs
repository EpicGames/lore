// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Once;
use std::time::Duration;

use opentelemetry::metrics::Meter;

/// Registers OpenTelemetry instruments for the `lore-io` syscall pool under `lore.io.pool`. The
/// pool runs every file operation on the default `psync` backend; on `uring` and `iocp`,
/// positional reads and writes bypass it and only the other operations reach it.
///
/// Each instrument reads the pool from its callback, which runs once per metrics collection. The
/// counters report the pool's totals since it was created. The `window_*` gauges report the peaks
/// since the previous collection and reset them, so they capture bursts shorter than the
/// collection interval that the instantaneous gauges miss. Each instrument reads the pool
/// separately, so values from one collection are not a single snapshot and are not meant to be
/// subtracted from one another.
///
/// A collection whose export fails has still reset the peaks, so a burst in that interval is
/// lost from the `window_*` gauges; the counters are totals and lose nothing.
///
/// Each peak has a single reader, so only the first call in a process registers anything. The
/// callbacks stay registered for the life of that call's meter provider.
pub fn register_io_pool_metrics(meter: &Meter) {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| register(meter));
}

fn register(meter: &Meter) {
    let gauge = |name: &'static str, description: &'static str, read: fn() -> usize| {
        meter
            .u64_observable_gauge(name)
            .with_description(description)
            .with_callback(move |observer| observer.observe(read() as u64, &[]))
            .build();
    };
    let counter = |name: &'static str, description: &'static str, read: fn() -> u64| {
        meter
            .u64_observable_counter(name)
            .with_description(description)
            .with_callback(move |observer| observer.observe(read(), &[]))
            .build();
    };
    let seconds_gauge = |name: &'static str, description: &'static str, read: fn() -> Duration| {
        meter
            .f64_observable_gauge(name)
            .with_unit("s")
            .with_description(description)
            .with_callback(move |observer| observer.observe(read().as_secs_f64(), &[]))
            .build();
    };
    let seconds_counter =
        |name: &'static str, description: &'static str, read: fn() -> Duration| {
            meter
                .f64_observable_counter(name)
                .with_unit("s")
                .with_description(description)
                .with_callback(move |observer| observer.observe(read().as_secs_f64(), &[]))
                .build();
        };
    gauge("lore.io.pool.queued", "Jobs waiting for a thread.", || {
        lore_io::pool_stats().queued
    });
    gauge(
        "lore.io.pool.executing",
        "Jobs running on a thread.",
        || lore_io::pool_stats().executing,
    );
    gauge(
        "lore.io.pool.threads",
        "Threads alive, idle ones included.",
        || lore_io::pool_stats().threads,
    );
    gauge("lore.io.pool.max_threads", "The thread cap.", || {
        lore_io::pool_stats().max_threads
    });
    gauge(
        "lore.io.pool.window_queue_high_water",
        "Deepest queue since the previous collection; reset by each collection.",
        lore_io::take_pool_queue_high_water,
    );
    gauge(
        "lore.io.pool.window_threads_high_water",
        "Most threads alive since the previous collection; reset by each collection.",
        lore_io::take_pool_threads_high_water,
    );
    seconds_gauge(
        "lore.io.pool.window_max_queue_wait_seconds",
        "Longest queue wait since the previous collection, counting jobs still queued; reset by \
         each collection.",
        lore_io::take_pool_max_queue_wait,
    );
    counter(
        "lore.io.pool.submitted",
        "Jobs queued for a pool thread.",
        || lore_io::pool_stats().submitted,
    );
    counter(
        "lore.io.pool.started",
        "Jobs taken from the queue by a thread.",
        || lore_io::pool_stats().started,
    );
    counter(
        "lore.io.pool.saturated",
        "Jobs queued with no thread idle and no room under the cap.",
        || lore_io::pool_stats().saturated,
    );
    counter(
        "lore.io.pool.spawn_refused",
        "Submissions whose new thread the operating system refused.",
        || lore_io::pool_stats().spawn_refused,
    );
    seconds_counter(
        "lore.io.pool.queue_wait_seconds",
        "Total time started jobs spent queued.",
        || lore_io::pool_stats().queue_wait,
    );
}
