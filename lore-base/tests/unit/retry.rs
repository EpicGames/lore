// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_base::retry::*;

const CLAMP: u64 = DEFAULT_JITTER_CLAMP_MS;

mod reset_attempts {
    use super::*;

    /// Restored attempts carry on from the interval already reached rather than the start.
    #[test]
    fn restores_every_wait_and_keeps_the_interval() {
        let mut retry = retry_with_jitter(100, 10_000, 2, 0.0);
        assert_eq!(retry.next_interval(), Some(Duration::from_millis(100)));
        assert_eq!(retry.next_interval(), Some(Duration::from_millis(200)));
        assert_eq!(retry.next_interval(), None);

        retry.reset_attempts();

        assert_eq!(retry.next_interval(), Some(Duration::from_millis(400)));
        assert_eq!(retry.next_interval(), Some(Duration::from_millis(800)));
        assert_eq!(retry.next_interval(), None);
    }
}

#[test]
fn deviation_is_symmetric_about_the_interval() {
    assert_eq!(jittered(100, 0.5, CLAMP, 0.0), 50);
    assert_eq!(jittered(100, 0.5, CLAMP, 0.5), 100);
    assert_eq!(jittered(100, 0.5, CLAMP, 1.0), 150);
}

#[test]
fn deviation_is_capped_in_both_directions() {
    assert_eq!(jittered(1_000, 0.5, CLAMP, 0.0), 1_000 - CLAMP);
    assert_eq!(jittered(1_000, 0.5, CLAMP, 1.0), 1_000 + CLAMP);
}

#[test]
fn deviation_below_the_cap_stays_proportional() {
    for interval in [1, 2, 50, 100, 199] {
        assert_eq!(jittered(interval, 0.5, CLAMP, 0.0), interval - interval / 2);
        assert_eq!(jittered(interval, 0.5, CLAMP, 1.0), interval + interval / 2);
    }
}

#[test]
fn a_raised_clamp_restores_the_full_deviation() {
    assert_eq!(jittered(1_000, 0.5, DEFAULT_JITTER_CLAMP_MS, 0.0), 900);
    assert_eq!(jittered(1_000, 0.5, 1_000, 0.0), 500);
    assert_eq!(jittered(1_000, 0.5, 1_000, 1.0), 1_500);
}

#[test]
fn a_lowered_clamp_narrows_the_deviation() {
    assert_eq!(jittered(1_000, 0.5, 10, 0.0), 990);
    assert_eq!(jittered(1_000, 0.5, 0, 0.0), 1_000);
    assert_eq!(jittered(1_000, 0.5, 0, 1.0), 1_000);
}

#[test]
fn deviation_never_underflows_the_interval() {
    assert_eq!(jittered(10, 5.0, CLAMP, 0.0), 0);
    assert_eq!(jittered(0, 0.5, CLAMP, 0.0), 0);
}

#[test]
fn sample_outside_the_unit_range_saturates() {
    assert_eq!(jittered(100, 0.5, CLAMP, -1.0), 50);
    assert_eq!(jittered(100, 0.5, CLAMP, 2.0), 150);
}

#[test]
fn zero_jitter_leaves_the_interval_alone() {
    assert_eq!(jittered(750, 0.0, CLAMP, 0.0), 750);
    assert_eq!(jittered(750, 0.0, CLAMP, 1.0), 750);
}
