// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_revision::fs::fuse::listing_cache::LISTING_BLOCK_BYTES;
use lore_revision::fs::fuse::listing_cache::LISTING_CACHE_BYTES;
use lore_revision::fs::fuse::listing_cache::ListingCache;
use lore_revision::fs::fuse::served::Served;
use lore_revision::fs::fuse::served::ServedCell;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

async fn test_repository() -> Arc<RepositoryContext> {
    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");
    Arc::new(RepositoryContext::new_null_context(
        immutable_store,
        mutable_store,
    ))
}

async fn test_served() -> Arc<Served> {
    Arc::new(Served::new(
        State::new(),
        test_repository().await,
        Arc::new(ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES)),
    ))
}

#[tokio::test]
async fn publish_replaces_and_marks_previous_superseded() {
    let first = test_served().await;
    let cell = ServedCell::new(first.clone());
    let second = test_served().await;

    let returned_previous = cell.publish(second.clone());

    assert!(Arc::ptr_eq(&returned_previous, &first));
    assert!(first.superseded());
    assert!(!second.superseded());
    assert!(Arc::ptr_eq(&cell.load(), &second));
}

#[tokio::test]
async fn publish_releases_the_previous_once_no_load_holds_it() {
    let first = test_served().await;
    let previous = Arc::downgrade(&first);
    let cell = ServedCell::new(first);
    let held = cell.load();

    drop(cell.publish(test_served().await));
    assert!(previous.upgrade().is_some(), "a load holds the previous");
    drop(held);

    assert!(
        previous.upgrade().is_none(),
        "the previous outlived the last load holding it"
    );
}

/// Loads racing publishes hold no replaced value past their own end.
#[tokio::test(flavor = "multi_thread")]
async fn loads_racing_publishes_release_every_replaced_value() {
    const LOADERS: usize = 8;
    const PUBLISHES: usize = 1_000;
    let first = test_served().await;
    let (state, repository, listings) = (
        first.state.clone(),
        first.repository.clone(),
        first.listings.clone(),
    );
    let mut published = vec![Arc::downgrade(&first)];
    let cell = ServedCell::new(first);

    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..LOADERS {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    std::hint::black_box(cell.load());
                }
            });
        }
        for _ in 0..PUBLISHES {
            let next = Arc::new(Served::new(
                state.clone(),
                repository.clone(),
                listings.clone(),
            ));
            published.push(Arc::downgrade(&next));
            drop(cell.publish(next));
        }
        done.store(true, Ordering::Relaxed);
    });

    let current = published.pop().and_then(|last| last.upgrade());
    assert!(current.is_some_and(|current| Arc::ptr_eq(&current, &cell.load())));
    assert!(
        published
            .iter()
            .all(|replaced| replaced.upgrade().is_none()),
        "a replaced value outlived the loads"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn load_allocates_nothing_once_its_thread_has_loaded() {
    let cell = ServedCell::new(test_served().await);
    drop(cell.load());

    lore_base::allocator::set_allocation_counting(true);
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    let _restore = Restore;

    let before = lore_base::allocator::thread_allocation_count();
    let _ = std::hint::black_box(cell.load());
    let after = lore_base::allocator::thread_allocation_count();
    assert_eq!(
        after, before,
        "a load allocated on a thread that had loaded"
    );
}

/// Loads per thread in [`bench_served_load`].
const BENCH_LOADS: usize = 2_000_000;

/// The mean nanoseconds one of `threads` threads takes to load what is served, each loading
/// [`BENCH_LOADS`] times, beside a publish every millisecond where `publishing`.
fn time_loads(cell: &ServedCell, threads: usize, publishing: bool) -> f64 {
    let barrier = Barrier::new(threads + 1);
    let done = AtomicBool::new(false);
    let template = cell.load();
    std::thread::scope(|scope| {
        let loaders: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    let started = Instant::now();
                    for _ in 0..BENCH_LOADS {
                        std::hint::black_box(cell.load());
                    }
                    started.elapsed()
                })
            })
            .collect();
        if publishing {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    cell.publish(Arc::new(Served::new(
                        template.state.clone(),
                        template.repository.clone(),
                        template.listings.clone(),
                    )));
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        }
        barrier.wait();
        let total: Duration = loaders
            .into_iter()
            .map(|loader| loader.join().expect("the loader completes"))
            .sum();
        done.store(true, Ordering::Relaxed);
        total.as_nanos() as f64 / (threads * BENCH_LOADS) as f64
    })
}

/// What a load costs as more threads load at once, with and without publishes beside them.
///
/// `cargo test -p lore-revision --release --test unit -- --ignored --nocapture --test-threads=1
/// fs::fuse::served::bench_served_load`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_served_load() {
    let cell = ServedCell::new(test_served().await);
    for threads in [1, 4, 16, 64] {
        let quiet = time_loads(&cell, threads, false);
        let publishing = time_loads(&cell, threads, true);
        println!(
            "{threads:>3} threads: {quiet:>6.1} ns/load, {publishing:>6.1} ns/load beside a \
             publish every ms"
        );
    }
}
