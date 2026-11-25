// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Exponential back-off with jitter, shared by every crate that retries an operation.

use std::time::Duration;

/// Largest deviation jitter may introduce by default, in milliseconds.
///
/// Keeps a long back-off from scattering callers across seconds. It also flattens the spread on
/// intervals above 200ms, so callers that want the full fraction raise it.
#[lore_macro::test_pub]
const DEFAULT_JITTER_CLAMP_MS: u64 = 100;

/// Fractional deviation applied to each interval: half of it either way, by default no more than
/// 100ms.
pub const DEFAULT_JITTER: f32 = 0.5;

/// Retry waiter with exponential back-off and jitter.
#[derive(Clone)]
pub struct Retry {
    current: u64,
    maximum: u64,
    jitter: f32,
    jitter_clamp: u64,
    counter: usize,
    limit: usize,
}

impl Retry {
    /// Sleep for the [`next_interval`](Self::next_interval). Returns `false` once `limit` waits
    /// have been spent, so a caller can treat it as the loop condition and `false` as "give up".
    pub async fn wait(&mut self) -> bool {
        let Some(interval) = self.next_interval() else {
            return false;
        };
        tokio::time::sleep(interval).await;
        true
    }

    /// The current interval with jitter, the current interval doubling up to the maximum. `None`
    /// once `limit` intervals have been taken.
    pub fn next_interval(&mut self) -> Option<Duration> {
        if self.counter >= self.limit {
            return None;
        }
        let interval = jittered(
            self.current,
            self.jitter,
            self.jitter_clamp,
            rand::random::<f32>(),
        );
        self.current = std::cmp::min(self.current * 2, self.maximum);
        self.counter += 1;
        Some(Duration::from_millis(interval))
    }

    /// Cap the jitter deviation at `millis` instead of the default 100ms.
    ///
    /// Above half the ladder's largest interval the clamp never binds, leaving the full fraction.
    #[must_use]
    pub fn with_jitter_clamp(mut self, millis: u64) -> Self {
        self.jitter_clamp = millis;
        self
    }

    /// Restore all `limit` waits, keeping the current interval so the back-off goes on growing
    /// from where it is.
    pub fn reset_attempts(&mut self) {
        self.counter = 0;
    }

    pub fn counter(&self) -> usize {
        self.counter
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// `interval` displaced by up to `jitter` of itself in either direction, for a `sample` in
/// `0.0..=1.0`, and by no more than `clamp` either way.
///
/// The deviation is symmetric about `interval`, so callers that backed off in step spread across
/// the interval instead of converging on a single point offset from it.
#[lore_macro::test_pub]
fn jittered(interval: u64, jitter: f32, clamp: u64, sample: f32) -> u64 {
    let magnitude = ((interval as f32 * jitter) as u64).min(clamp);
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
        jitter_clamp: DEFAULT_JITTER_CLAMP_MS,
        counter: 0,
        limit,
    }
}
