// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::types::Address;
use lore_revision::fs::fuse::listing::Listing;
use lore_revision::fs::fuse::listing::ListingEntry;
use lore_revision::fs::fuse::listing_cache::LISTING_BLOCK_BYTES;
use lore_revision::fs::fuse::listing_cache::LISTING_CACHE_BYTES;
use lore_revision::fs::fuse::listing_cache::ListingCache;
use lore_revision::fs::fuse::listing_cache::Slot;
use lore_revision::node::NodeID;

fn node(marker: u64) -> NodeID {
    marker as NodeID
}

fn listing_with_entries(count: usize) -> Arc<Listing> {
    let entry = ListingEntry {
        name_hash: 0,
        size: 0,
        address: Address::default(),
        name_offset: 0,
        name_len: 0,
        mode: 0,
        flags: 0,
        filter_state: 0,
        block: 0,
        view_line: 0,
    };
    Arc::new(Listing::from_parts(
        vec![entry; count].into_boxed_slice(),
        Box::new([]),
        vec![0u32; count].into_boxed_slice(),
    ))
}

#[test]
fn get_returns_same_arc_for_same_node() {
    let cache = ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES);
    let listing = listing_with_entries(3);
    let addr = node(42);
    cache.insert(addr, listing.clone());

    let got = cache.get(addr).expect("the inserted listing must be found");
    assert!(
        Arc::ptr_eq(&got, &listing),
        "get must return the same Arc that was inserted"
    );
}

#[test]
fn remove_drops_the_listing_and_its_bytes() {
    let cache = ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES);
    cache.insert(node(7), listing_with_entries(2));
    cache.insert(node(8), listing_with_entries(1));
    cache.remove(node(7));
    assert!(cache.get(node(7)).is_none());
    assert_eq!(cache.bytes(), listing_with_entries(1).bytes());
    cache.insert(node(7), listing_with_entries(2));
    assert!(cache.get(node(7)).is_some());
}

fn block() -> Arc<lore_revision::node::NodeBlock> {
    lore_revision::node::NodeBlock::new(lore_base::allocator::HeapBox::new_zeroed_in(None))
}

fn holding(blocks: Vec<Arc<lore_revision::node::NodeBlock>>) -> Arc<Listing> {
    Arc::new(
        Arc::into_inner(listing_with_entries(1))
            .expect("unshared")
            .holding(blocks),
    )
}

/// The listings' own bytes over their budget evict the oldest unreferenced listing, with no
/// block pinned.
#[test]
fn listing_bytes_over_their_budget_evict_with_no_pins() {
    let unit_bytes = listing_with_entries(1).bytes();
    let cache = ListingCache::new(unit_bytes * 2, LISTING_BLOCK_BYTES);

    cache.insert(node(1), listing_with_entries(1));
    cache.insert(node(2), listing_with_entries(1));
    // Over budget for the first time: evicts node(1), the oldest entry,
    // since neither prior entry was hit after being inserted.
    cache.insert(node(3), listing_with_entries(1));

    assert!(
        cache.get(node(1)).is_none(),
        "the oldest unreferenced entry must be evicted"
    );
    assert!(cache.get(node(2)).is_some());
    assert!(cache.get(node(3)).is_some());
    assert!(cache.bytes() <= unit_bytes * 2);
    assert_eq!(cache.pinned_bytes(), 0);
}

/// Listings pinning distinct blocks all stay while the pinned bytes are within the block
/// budget, though those bytes alone pass the listing budget.
#[test]
fn pinned_blocks_within_their_budget_evict_no_listing() {
    const LISTINGS: usize = 4;
    let blocks: Vec<_> = (0..LISTINGS).map(|_| block()).collect();
    let listing_bytes = holding(vec![blocks[0].clone()]).bytes();
    let block_bytes = blocks[0].memory_bytes();
    let cache = ListingCache::new(LISTINGS * listing_bytes, LISTINGS * block_bytes);
    assert!(
        block_bytes > LISTINGS * listing_bytes,
        "one block passes the listing budget"
    );

    for (marker, block) in blocks.iter().enumerate() {
        cache.insert(node(marker as u64), holding(vec![block.clone()]));
    }
    for marker in 0..LISTINGS {
        assert!(
            cache.get(node(marker as u64)).is_some(),
            "listing {marker} stays"
        );
    }
    assert_eq!(cache.pinned_bytes(), LISTINGS * block_bytes);
}

/// Pinned bytes over their budget evict the oldest unreferenced listing, with the listings'
/// own bytes far under theirs.
#[test]
fn pinned_bytes_over_their_budget_evict_with_listing_bytes_under_theirs() {
    let blocks: Vec<_> = (0..3).map(|_| block()).collect();
    let block_bytes = blocks[0].memory_bytes();
    let cache = ListingCache::new(LISTING_CACHE_BYTES, 2 * block_bytes);

    for (marker, block) in blocks.iter().enumerate() {
        cache.insert(node(marker as u64), holding(vec![block.clone()]));
    }
    assert!(
        cache.get(node(0)).is_none(),
        "the oldest listing is evicted"
    );
    assert!(cache.get(node(1)).is_some());
    assert!(cache.get(node(2)).is_some());
    assert_eq!(cache.pinned_bytes(), 2 * block_bytes);
}

#[test]
fn hit_allocates_nothing() {
    let cache = ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES);
    let addr = node(99);
    cache.insert(addr, listing_with_entries(4));

    lore_base::allocator::set_allocation_counting(true);
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    let _restore = Restore;

    let before = lore_base::allocator::thread_allocation_count();
    let _ = std::hint::black_box(cache.get(addr));
    let after = lore_base::allocator::thread_allocation_count();
    assert_eq!(after, before, "a cache hit must not allocate");
}

/// Eviction gives back the charge of the blocks a listing held, as a drop does.
#[test]
fn eviction_gives_back_the_charge_of_the_blocks_a_listing_held() {
    let block =
        lore_revision::node::NodeBlock::new(lore_base::allocator::HeapBox::new_zeroed_in(None));
    let holding = Arc::into_inner(listing_with_entries(1))
        .expect("unshared")
        .holding(vec![block.clone()]);
    let cache = ListingCache::new(holding.bytes(), block.memory_bytes());
    cache.insert(node(1), Arc::new(holding));
    assert_eq!(cache.pinned_bytes(), block.memory_bytes());

    cache.insert(node(2), listing_with_entries(1));
    assert!(
        cache.get(node(1)).is_none(),
        "the holding listing is evicted"
    );
    assert_eq!(cache.pinned_bytes(), 0, "its block's charge comes back");
}

/// A block pinned by a listing of one cache and released by a listing of its fresh successor
/// is charged once and given back once.
#[test]
fn a_charge_taken_by_one_cache_is_given_back_by_its_fresh_successor() {
    let block =
        lore_revision::node::NodeBlock::new(lore_base::allocator::HeapBox::new_zeroed_in(None));
    let holding = || {
        Arc::new(
            Arc::into_inner(listing_with_entries(1))
                .expect("unshared")
                .holding(vec![block.clone()]),
        )
    };
    let old = ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES);
    let fresh = old.fresh();
    old.insert(node(1), holding());
    fresh.insert(node(1), holding());
    assert_eq!(fresh.pinned_bytes(), block.memory_bytes(), "charged once");
    drop(old);
    assert_eq!(fresh.pinned_bytes(), block.memory_bytes(), "still held");
    fresh.remove(node(1));
    assert_eq!(fresh.pinned_bytes(), 0, "given back once");
}

/// A fresh cache evicts against the block budget of the cache it came from.
#[test]
fn a_fresh_cache_keeps_the_block_budget() {
    let blocks: Vec<_> = (0..3).map(|_| block()).collect();
    let block_bytes = blocks[0].memory_bytes();
    let fresh = ListingCache::new(LISTING_CACHE_BYTES, 2 * block_bytes).fresh();

    for (marker, block) in blocks.iter().enumerate() {
        fresh.insert(node(marker as u64), holding(vec![block.clone()]));
    }
    assert!(
        fresh.get(node(0)).is_none(),
        "the oldest listing is evicted"
    );
    assert_eq!(fresh.pinned_bytes(), 2 * block_bytes);
}

/// A refused insert caches nothing and gives back what it charged.
#[test]
fn a_refused_insert_caches_and_charges_nothing() {
    let block =
        lore_revision::node::NodeBlock::new(lore_base::allocator::HeapBox::new_zeroed_in(None));
    let holding = Arc::into_inner(listing_with_entries(1))
        .expect("unshared")
        .holding(vec![block]);
    let cache = ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES);
    assert!(!cache.insert_if(node(1), Arc::new(holding), || false));
    assert!(cache.get(node(1)).is_none());
    assert_eq!(cache.bytes(), 0);
    assert_eq!(cache.pinned_bytes(), 0);
}

#[test]
fn referenced_entry_gets_a_second_chance() {
    let unit_bytes = listing_with_entries(1).bytes();
    let cache = ListingCache::new(unit_bytes * 2, LISTING_BLOCK_BYTES);

    cache.insert(node(1), listing_with_entries(1));
    cache.insert(node(2), listing_with_entries(1));
    assert!(cache.get(node(1)).is_some());
    cache.insert(node(3), listing_with_entries(1));

    assert!(cache.get(node(1)).is_some(), "a hit spares the entry");
    assert!(cache.get(node(2)).is_none());
    assert!(cache.get(node(3)).is_some());
}

/// An inserter has counted its bytes and filled its slot but not yet pushed its key: the
/// evictor finds the hand empty while over budget and returns rather than spinning.
#[test]
fn eviction_returns_when_the_hand_is_empty_while_over_budget() {
    let unit_bytes = listing_with_entries(1).bytes();
    let cache = Arc::new(ListingCache::new(unit_bytes, LISTING_BLOCK_BYTES));
    cache.insert(node(1), listing_with_entries(1));
    cache.bytes.fetch_add(unit_bytes, Ordering::SeqCst);
    cache.slots.insert(
        node(2),
        Slot {
            listing: listing_with_entries(1),
            referenced: AtomicBool::new(false),
        },
    );
    cache.clock.clear();

    let (done, finished) = std::sync::mpsc::channel();
    let evictor = cache.clone();
    std::thread::spawn(move || {
        evictor.evict();
        let _ = done.send(());
    });
    assert!(
        finished.recv_timeout(Duration::from_secs(5)).is_ok(),
        "evict must return when a sweep makes no progress"
    );
}

#[test]
fn reinserting_a_key_replaces_it_and_counts_its_bytes_once() {
    let cache = ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES);
    cache.insert(node(1), listing_with_entries(1));
    let larger = listing_with_entries(5);
    cache.insert(node(1), larger.clone());

    assert_eq!(cache.len(), 1);
    assert_eq!(cache.bytes(), larger.bytes());
    let got = cache.get(node(1)).expect("replaced entry");
    assert!(Arc::ptr_eq(&got, &larger));
}

#[test]
fn budget_holds_under_concurrent_inserts_and_hits() {
    let unit_bytes = listing_with_entries(1).bytes();
    let cache = Arc::new(ListingCache::new(unit_bytes * 64, LISTING_BLOCK_BYTES));
    let threads: Vec<_> = (0..8u64)
        .map(|thread| {
            let cache = cache.clone();
            std::thread::spawn(move || {
                for i in 0..2_000u64 {
                    let marker = thread * 1_000_000 + i;
                    cache.insert(node(marker), listing_with_entries(1));
                    std::hint::black_box(cache.get(node(marker / 2)));
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("worker");
    }

    assert!(
        cache.bytes() <= unit_bytes * 64,
        "{} bytes over a {} budget",
        cache.bytes(),
        unit_bytes * 64
    );
    assert_eq!(cache.bytes(), cache.len() * unit_bytes);
}
