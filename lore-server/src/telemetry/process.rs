// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use opentelemetry::metrics::Meter;

/// Gauge of the file descriptors the process holds open.
pub const OPEN_FILE_DESCRIPTORS: &str = "lore.server.open_file_descriptors";

/// Gauge of the soft `RLIMIT_NOFILE` limit, past which opening a file or socket fails.
pub const FILE_DESCRIPTOR_LIMIT: &str = "lore.server.file_descriptor_limit";

/// Lists one entry per descriptor the calling process holds open.
#[cfg(target_os = "linux")]
const OPEN_DESCRIPTORS_DIR: &str = "/proc/self/fd";
#[cfg(target_os = "macos")]
const OPEN_DESCRIPTORS_DIR: &str = "/dev/fd";

/// Reports the process's open file descriptors against its limit, sampled each time the meter's
/// provider collects.
///
/// Sockets count against the same limit as files, so a flood of connections shows here before
/// opens start failing with `EMFILE`.
pub fn observe_file_descriptors(meter: &Meter) {
    meter
        .u64_observable_gauge(OPEN_FILE_DESCRIPTORS)
        .with_callback(|observer| {
            // A failed read leaves this collection without a sample; a gap in the series is
            // better than a zero that reads as plenty of headroom.
            if let Some(open) = open_file_descriptors() {
                observer.observe(open, &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge(FILE_DESCRIPTOR_LIMIT)
        .with_callback(|observer| {
            if let Some(limit) = file_descriptor_limit() {
                observer.observe(limit, &[]);
            }
        })
        .build();
}

fn open_file_descriptors() -> Option<u64> {
    let entries = std::fs::read_dir(OPEN_DESCRIPTORS_DIR).ok()?;
    // The listing includes the descriptor read_dir holds open to produce it.
    Some((entries.count() as u64).saturating_sub(1))
}

fn file_descriptor_limit() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the struct it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    // An unlimited soft limit has no ceiling to measure against.
    if limit.rlim_cur == libc::RLIM_INFINITY {
        return None;
    }
    Some(limit.rlim_cur)
}
