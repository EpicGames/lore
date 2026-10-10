// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Helpers the examples share.

use std::time::Duration;

/// Process CPU time, on the platforms that expose it.
///
/// `getrusage` is Unix-only. Elsewhere this is `None` and the CPU column reports `NaN`, rather
/// than a wall-clock stand-in that would read like a measurement.
#[cfg(unix)]
pub fn cpu_time() -> Option<Duration> {
    // SAFETY: `getrusage` only writes into the zeroed struct handed to it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(result, 0, "getrusage failed");

    let as_duration = |time: libc::timeval| {
        Duration::new(
            time.tv_sec as u64,
            (time.tv_usec as u32).saturating_mul(1000),
        )
    };
    Some(as_duration(usage.ru_utime) + as_duration(usage.ru_stime))
}

#[cfg(not(unix))]
pub fn cpu_time() -> Option<Duration> {
    None
}
