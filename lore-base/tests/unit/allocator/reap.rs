// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_base::allocator::reap::REAP_MIN_IDLE_MS;
use lore_base::allocator::reap::REAP_WAIT_MAX;
use lore_base::allocator::reap::next_reap_wait;

/// The wait between reaps doubles while they find no heap to reap, up to the longest wait, and
/// drops back to the minimum idle time after a reap that finds heaps.
#[test]
fn the_reap_wait_backs_off_until_a_reap_finds_heaps() {
    let min_wait = Duration::from_millis(u64::from(REAP_MIN_IDLE_MS));
    let mut wait = min_wait;
    let mut waits = Vec::new();
    while wait < REAP_WAIT_MAX {
        wait = next_reap_wait(wait, 0);
        waits.push(wait);
    }
    assert_eq!(
        waits,
        [2, 4, 8].map(Duration::from_secs),
        "the waits after reaps finding nothing"
    );
    assert_eq!(next_reap_wait(REAP_WAIT_MAX, 0), REAP_WAIT_MAX);
    assert_eq!(next_reap_wait(REAP_WAIT_MAX, 3), min_wait);
}
