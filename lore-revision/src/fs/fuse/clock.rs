// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! CLOCK eviction shared by the provider's caches: a lock-free queue of keys is the hand, an
//! entry read since the hand last passed is spared once, and one thread at a time evicts.
//!
//! A hit costs one [`mark`]: a load, and a store only when the bit is clear.

use std::hash::Hash;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crossbeam_queue::SegQueue;
use dashmap::DashMap;

/// Marks an entry read. Writes only when the bit is clear, so the hits of a hot entry share its
/// cache line rather than bouncing it.
#[lore_macro::test_pub]
#[inline]
pub(crate) fn mark(referenced: &AtomicBool) {
    if !referenced.load(Ordering::Relaxed) {
        referenced.store(true, Ordering::Relaxed);
    }
}

/// Clears `referenced`, answering whether the entry was read since the hand last passed.
#[lore_macro::test_pub]
#[inline]
pub(crate) fn spare(referenced: &AtomicBool) -> bool {
    referenced.swap(false, Ordering::Relaxed)
}

/// What visiting a key did.
#[lore_macro::test_pub]
pub(crate) enum Visit {
    /// The entry was removed and its bytes counted out; the key leaves the hand.
    Evicted,
    /// The entry stays; the key goes to the back of the hand.
    Kept,
    /// The key names nothing held any more; it leaves the hand.
    Stale,
}

/// Visits `key` of a keyed cache: spares an entry a hit marked, and otherwise removes it unless a
/// hit marks it first, handing the removed value to `evicted`.
#[lore_macro::test_pub]
pub(crate) fn visit_keyed<K: Eq + Hash + Clone, V>(
    map: &DashMap<K, V>,
    key: &K,
    referenced: impl Fn(&V) -> &AtomicBool,
    evicted: impl FnOnce(V),
) -> Visit {
    match map.get(key) {
        None => return Visit::Stale,
        Some(entry) if spare(referenced(&entry)) => return Visit::Kept,
        Some(_) => {}
    }
    match map.remove_if(key, |_, value| !referenced(value).load(Ordering::Relaxed)) {
        Some((_, value)) => {
            evicted(value);
            Visit::Evicted
        }
        None if map.contains_key(key) => Visit::Kept,
        None => Visit::Stale,
    }
}

/// The hand and the evicting flag of one cache.
#[lore_macro::test_pub]
pub(crate) struct Clock<K> {
    hand: SegQueue<K>,
    evicting: AtomicBool,
}

impl<K> Default for Clock<K> {
    fn default() -> Self {
        Self {
            hand: SegQueue::new(),
            evicting: AtomicBool::new(false),
        }
    }
}

impl<K> Clock<K> {
    /// Puts `key` at the back of the hand. Each held entry has its key on the hand once.
    #[lore_macro::test_pub]
    pub(crate) fn push(&self, key: K) {
        self.hand.push(key);
    }

    pub(crate) fn pop(&self) -> Option<K> {
        self.hand.pop()
    }

    /// Keys on the hand, stale ones included. For tests.
    #[cfg(feature = "test-util")]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.hand.len()
    }

    #[lore_macro::test_pub]
    pub(crate) fn clear(&self) {
        while self.hand.pop().is_some() {}
    }

    /// Turns the hand while `is_over`, unless another thread is evicting. `try_evict` visits one
    /// key: it spares a referenced entry once and removes an unreferenced one only if a hit has
    /// not marked it since (a `remove_if` re-check).
    ///
    /// Returns once within budget, when another thread holds the flag, or after a sweep that
    /// evicted nothing: an empty hand, or entries every visit had to keep. The evictor re-checks
    /// the budget after releasing the flag, so an insert that found it taken is not left unseen.
    #[lore_macro::test_pub]
    pub(crate) fn evict(&self, is_over: impl Fn() -> bool, mut try_evict: impl FnMut(&K) -> Visit) {
        while is_over() {
            if self
                .evicting
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_err()
            {
                return;
            }
            let progressed = self.sweep(&is_over, &mut try_evict);
            self.evicting.store(false, Ordering::SeqCst);
            if !progressed {
                return;
            }
        }
    }

    /// Two laps of the hand, the most a referenced entry survives; answers whether any visit
    /// evicted.
    fn sweep(&self, is_over: &impl Fn() -> bool, try_evict: &mut impl FnMut(&K) -> Visit) -> bool {
        let mut steps = 2 * self.hand.len() + 1;
        let mut evicted = false;
        while steps > 0 && is_over() {
            steps -= 1;
            let Some(key) = self.hand.pop() else {
                break;
            };
            match try_evict(&key) {
                Visit::Evicted => evicted = true,
                Visit::Kept => self.hand.push(key),
                Visit::Stale => {}
            }
        }
        evicted
    }
}
