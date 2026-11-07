// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A bytes-bounded cache of built [`Listing`]s keyed by the directory's node id, which a
//! directory keeps for the life of the filesystem State.
//!
//! A hit takes one map shard's read lock and marks the entry referenced, writing only a clear
//! bit. Eviction is the provider's shared CLOCK (`clock.rs`). Two budgets bound the cache apart:
//! the listings' own bytes, and the node blocks cached listings hold in memory, each counted
//! once: the first listing to hold a block charges it and the last to leave gives the charge
//! back. The caches one provider creates share the block charge, since a listing of one may pin
//! a block a listing of another releases.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

use crate::fs::fuse::clock;
use crate::fs::fuse::clock::Clock;
use crate::fs::fuse::listing::Listing;
use crate::node::NodeID;

/// Default budget for the listings' own bytes: generous enough that a large tree's directories
/// stay resident, bounded so a pathological one cannot grow the cache without limit.
pub const LISTING_CACHE_BYTES: usize = 256 << 20;

/// Default budget for the node blocks cached listings hold, each counted once. A build's working
/// set is a few hundred megabytes of blocks (326 MB for a `ShaderCompileWorker` cold build); under
/// the listings' own budget they evicted listings the build walked again.
pub const LISTING_BLOCK_BYTES: usize = 1 << 30;

#[lore_macro::test_pub]
struct Slot {
    listing: Arc<Listing>,
    referenced: AtomicBool,
}

/// Node-keyed cache of built directory listings, bounded to `max_bytes` of the listings' own
/// bytes ([`Listing::bytes`]) and `max_pinned` of the node blocks they hold.
#[lore_macro::test_pub]
pub struct ListingCache {
    slots: DashMap<NodeID, Slot>,
    clock: Clock<NodeID>,
    bytes: AtomicUsize,
    /// Bytes of the node blocks the listings of this cache and those it shares the charge with
    /// hold, each counted once.
    pinned: Arc<AtomicUsize>,
    max_bytes: usize,
    max_pinned: usize,
}

impl ListingCache {
    pub fn new(max_bytes: usize, max_pinned: usize) -> Self {
        Self {
            slots: DashMap::new(),
            clock: Clock::default(),
            bytes: AtomicUsize::new(0),
            pinned: Arc::new(AtomicUsize::new(0)),
            max_bytes,
            max_pinned,
        }
    }

    /// An empty cache with this one's budgets, sharing its pinned-block charge.
    pub fn fresh(&self) -> Self {
        let mut fresh = Self::new(self.max_bytes, self.max_pinned);
        fresh.pinned = self.pinned.clone();
        fresh
    }

    /// The listing cached for directory `node`, if any. A hit marks the entry referenced; never
    /// allocates.
    pub fn get(&self, node: NodeID) -> Option<Arc<Listing>> {
        let slot = self.slots.get(&node)?;
        clock::mark(&slot.referenced);
        Some(slot.listing.clone())
    }

    /// Caches `listing` under directory `node`, then evicts until the cache is back within its
    /// budgets.
    pub fn insert(&self, node: NodeID, listing: Arc<Listing>) {
        self.insert_if(node, listing, || true);
    }

    /// [`Self::insert`] where `admit` agrees, asked under the shard lock the insertion holds, so
    /// no reader sees a listing it refuses. False where it refused.
    pub fn insert_if(
        &self,
        node: NodeID,
        listing: Arc<Listing>,
        admit: impl FnOnce() -> bool,
    ) -> bool {
        // Charged before the shard lock, which then covers only the decision and the insert.
        self.admit(&listing);
        let slot = Slot {
            listing,
            referenced: AtomicBool::new(false),
        };
        #[allow(clippy::disallowed_methods)]
        let entry = self.slots.entry(node);
        if !admit() {
            drop(entry);
            self.release(&slot.listing);
            return false;
        }
        let replaced = match entry {
            Entry::Occupied(mut occupied) => Some(occupied.insert(slot)),
            Entry::Vacant(vacant) => {
                let _ = vacant.insert(slot);
                None
            }
        };
        match replaced {
            Some(replaced) => self.release(&replaced.listing),
            None => self.clock.push(node),
        }
        self.evict();
        true
    }

    /// Drops the listing of directory `node`; its key leaves the clock when the hand reaches it.
    pub fn remove(&self, node: NodeID) {
        if let Some((_, slot)) = self.slots.remove(&node) {
            self.release(&slot.listing);
        }
    }

    /// Drops every listing.
    pub fn clear(&self) {
        self.slots.retain(|_, slot| {
            self.release(&slot.listing);
            false
        });
    }

    /// Counts `listing` in the budget: its own bytes and each block it holds that no cached
    /// listing held.
    fn admit(&self, listing: &Listing) {
        self.bytes.fetch_add(listing.bytes(), Ordering::SeqCst);
        for block in listing.held_blocks() {
            if let Some(charge) = block.pin_for_listing() {
                self.pinned.fetch_add(charge, Ordering::SeqCst);
            }
        }
    }

    /// Takes `listing` out of the budget, giving back each block no other cached listing holds.
    fn release(&self, listing: &Listing) {
        self.bytes.fetch_sub(listing.bytes(), Ordering::SeqCst);
        for block in listing.held_blocks() {
            if let Some(charge) = block.unpin_for_listing() {
                self.pinned.fetch_sub(charge, Ordering::SeqCst);
            }
        }
    }

    /// Runs the clock hand while either budget is exceeded.
    #[lore_macro::test_pub]
    fn evict(&self) {
        self.clock.evict(
            || self.over_budget(),
            |key| {
                clock::visit_keyed(
                    &self.slots,
                    key,
                    |slot| &slot.referenced,
                    |slot| self.release(&slot.listing),
                )
            },
        );
    }

    fn over_budget(&self) -> bool {
        self.bytes.load(Ordering::SeqCst) > self.max_bytes
            || self.pinned.load(Ordering::SeqCst) > self.max_pinned
    }

    /// The listings' own bytes.
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The bytes of the node blocks the listings of this cache and those it shares the charge
    /// with hold, each counted once.
    pub fn pinned_bytes(&self) -> usize {
        self.pinned.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for ListingCache {
    fn drop(&mut self) {
        self.clear();
    }
}
