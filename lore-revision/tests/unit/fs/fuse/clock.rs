// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use lore_revision::fs::fuse::clock::Clock;
use lore_revision::fs::fuse::clock::Visit;
use lore_revision::fs::fuse::clock::mark;
use lore_revision::fs::fuse::clock::spare;
use lore_revision::fs::fuse::clock::visit_keyed;

/// A keyed cache of unit-sized entries over the clock, as the provider's caches use it.
#[derive(Default)]
struct Units {
    entries: DashMap<u32, AtomicBool>,
    clock: Clock<u32>,
    visits: AtomicUsize,
}

impl Units {
    fn insert(&self, key: u32) {
        self.entries.insert(key, AtomicBool::new(false));
        self.clock.push(key);
    }

    fn evict_to(&self, budget: usize) {
        self.clock.evict(
            || self.entries.len() > budget,
            |key| {
                self.visits.fetch_add(1, Ordering::Relaxed);
                visit_keyed(&self.entries, key, |bit| bit, drop)
            },
        );
    }
}

#[test]
fn evicts_the_oldest_unreferenced_and_spares_a_referenced_one_once() {
    let units = Units::default();
    for key in 0..4 {
        units.insert(key);
    }
    mark(&units.entries.get(&0).expect("held"));
    units.evict_to(2);
    let mut held: Vec<u32> = units.entries.iter().map(|entry| *entry.key()).collect();
    held.sort_unstable();
    assert_eq!(held, [0, 3]);
}

#[test]
fn returns_when_the_hand_is_empty_while_over_budget() {
    let units = Units::default();
    units.entries.insert(0, AtomicBool::new(false));
    units.evict_to(0);
    assert_eq!(units.visits.load(Ordering::Relaxed), 0);
    assert_eq!(units.entries.len(), 1);
}

#[test]
fn returns_after_a_sweep_that_keeps_every_entry() {
    let units = Units::default();
    for key in 0..4 {
        units.insert(key);
    }
    units.clock.evict(
        || true,
        |_| {
            units.visits.fetch_add(1, Ordering::Relaxed);
            Visit::Kept
        },
    );
    assert_eq!(units.visits.load(Ordering::Relaxed), 2 * 4 + 1);
    assert_eq!(units.clock.len(), 4);
}

#[test]
fn drops_stale_keys_and_leaves_the_flag_free() {
    let units = Units::default();
    for key in 0..3 {
        units.clock.push(key);
    }
    units.entries.insert(9, AtomicBool::new(false));
    units.evict_to(0);
    assert_eq!(units.clock.len(), 0);
    assert!(!units.clock.evicting.load(Ordering::SeqCst));
}

#[test]
fn a_held_flag_leaves_the_work_to_its_holder() {
    let units = Units::default();
    units.insert(0);
    units.clock.evicting.store(true, Ordering::SeqCst);
    units.evict_to(0);
    assert_eq!(units.visits.load(Ordering::Relaxed), 0);
}

#[test]
fn mark_writes_only_a_clear_bit() {
    let bit = AtomicBool::new(false);
    mark(&bit);
    assert!(bit.load(Ordering::Relaxed));
    mark(&bit);
    assert!(spare(&bit));
    assert!(!spare(&bit));
}
