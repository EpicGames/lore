// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Most reports one [`ReportRateLimit`] admits in a second. A burst of slow responses is reported
/// in part rather than flooding the log; the metrics still record every response.
#[lore_macro::test_pub]
const MAX_REPORTS_PER_SECOND: u32 = 1;

/// Caps how many reports are made in each second of the process's life.
///
/// Lock-free, so a caller pays one atomic update per report it considers.
pub struct ReportRateLimit {
    /// The second the count belongs to in the high 32 bits, and the reports admitted in that
    /// second in the low 32.
    window: AtomicU64,
}

impl ReportRateLimit {
    pub const fn new() -> Self {
        Self {
            window: AtomicU64::new(0),
        }
    }

    /// Whether a report may be made now.
    pub fn admit(&self) -> bool {
        self.admit_in_second(process_second())
    }

    /// Whether a report may be made in `second`. A second earlier than the current window counts
    /// against the window, so a caller that read the clock before another cannot reopen it.
    #[lore_macro::test_pub]
    fn admit_in_second(&self, second: u32) -> bool {
        self.window
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |window| {
                let window_second = (window >> 32) as u32;
                let admitted = window as u32;
                if second > window_second {
                    Some(pack(second, 1))
                } else {
                    (admitted < MAX_REPORTS_PER_SECOND).then(|| pack(window_second, admitted + 1))
                }
            })
            .is_ok()
    }
}

impl Default for ReportRateLimit {
    fn default() -> Self {
        Self::new()
    }
}

fn pack(second: u32, admitted: u32) -> u64 {
    (u64::from(second) << 32) | u64::from(admitted)
}

/// Seconds since the first call, which start at one so that the first second opens a window.
fn process_second() -> u32 {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_secs();
    u32::try_from(elapsed + 1).unwrap_or(u32::MAX)
}
