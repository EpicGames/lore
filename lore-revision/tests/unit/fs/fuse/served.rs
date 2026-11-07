// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

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

#[tokio::test(flavor = "current_thread")]
async fn load_allocates_nothing() {
    let cell = ServedCell::new(test_served().await);

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
    assert_eq!(after, before, "ServedCell::load must not allocate");
}
