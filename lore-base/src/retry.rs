// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Exponential back-off with jitter, shared by every crate that retries a remote operation.

use std::time::Duration;

/// Largest deviation jitter may introduce, in milliseconds.
const JITTER_CLAMP_MS: u64 = 100;

/// Fractional deviation applied to each interval: half of it either way, subject to
/// [`JITTER_CLAMP_MS`].
pub const DEFAULT_JITTER: f32 = 0.5;

/// Retry waiter with exponential back-off and jitter.
pub struct Retry {
    current: u64,
    maximum: u64,
    jitter: f32,
    counter: usize,
    limit: usize,
}

impl Retry {
    /// Sleep for the current interval, then double it up to the maximum. Returns `false` once
    /// `limit` waits have been spent, so a caller can treat it as the loop condition and `false`
    /// as "give up".
    pub async fn wait(&mut self) -> bool {
        if self.counter >= self.limit {
            return false;
        }

        tokio::time::sleep(Duration::from_millis(jittered(
            self.current,
            self.jitter,
            rand::random::<f32>(),
        )))
        .await;

        self.current = std::cmp::min(self.current * 2, self.maximum);
        self.counter += 1;

        true
    }

    pub fn counter(&self) -> usize {
        self.counter
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// `interval` displaced by up to `jitter` of itself in either direction, for a `sample` in
/// `0.0..=1.0`, and by no more than [`JITTER_CLAMP_MS`] either way.
///
/// The deviation is symmetric about `interval`, so callers that backed off in step spread across
/// the interval instead of converging on a single point offset from it.
fn jittered(interval: u64, jitter: f32, sample: f32) -> u64 {
    let magnitude = ((interval as f32 * jitter) as u64).min(JITTER_CLAMP_MS);
    let deviation = (magnitude as f32 * (sample.clamp(0.0, 1.0) * 2.0 - 1.0)) as i64;
    interval.saturating_add_signed(deviation)
}

/// Create a retry waiter, start and maximum times in milliseconds. Will give up
/// after trying for the limit number of times.
pub fn retry(start: u64, maximum: u64, limit: usize) -> Retry {
    retry_with_jitter(start, maximum, limit, DEFAULT_JITTER)
}

/// [`retry`] with a caller-chosen fractional deviation in place of [`DEFAULT_JITTER`].
pub fn retry_with_jitter(start: u64, maximum: u64, limit: usize, jitter: f32) -> Retry {
    Retry {
        current: start,
        maximum,
        jitter,
        counter: 0,
        limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deviation_is_symmetric_about_the_interval() {
        assert_eq!(jittered(100, 0.5, 0.0), 50);
        assert_eq!(jittered(100, 0.5, 0.5), 100);
        assert_eq!(jittered(100, 0.5, 1.0), 150);
    }

    #[test]
    fn deviation_is_capped_in_both_directions() {
        assert_eq!(jittered(1_000, 0.5, 0.0), 1_000 - JITTER_CLAMP_MS);
        assert_eq!(jittered(1_000, 0.5, 1.0), 1_000 + JITTER_CLAMP_MS);
    }

    #[test]
    fn deviation_below_the_cap_stays_proportional() {
        for interval in [1, 2, 50, 100, 199] {
            assert_eq!(jittered(interval, 0.5, 0.0), interval - interval / 2);
            assert_eq!(jittered(interval, 0.5, 1.0), interval + interval / 2);
        }
    }

    #[test]
    fn deviation_never_underflows_the_interval() {
        assert_eq!(jittered(10, 5.0, 0.0), 0);
        assert_eq!(jittered(0, 0.5, 0.0), 0);
    }

    #[test]
    fn sample_outside_the_unit_range_saturates() {
        assert_eq!(jittered(100, 0.5, -1.0), 50);
        assert_eq!(jittered(100, 0.5, 2.0), 150);
    }

    #[test]
    fn zero_jitter_leaves_the_interval_alone() {
        assert_eq!(jittered(750, 0.0, 0.0), 750);
        assert_eq!(jittered(750, 0.0, 1.0), 750);
    }
}
