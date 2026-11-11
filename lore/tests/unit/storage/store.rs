// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::*;

async fn register_store() -> (Arc<StoreInternal>, LoreStore) {
    let store = in_memory_for_tests("test").await;
    let store_handle = handle::register(store.clone());
    (store, store_handle)
}

#[tokio::test]
async fn op_enter_after_mark_invalid_returns_none() {
    let (store, store_handle) = register_store().await;
    store.invalid.store(true, Ordering::Release);
    assert!(OpGuard::enter(store_handle).is_none());
    handle::unregister(store_handle);
}

#[tokio::test]
async fn op_enter_unregistered_handle_returns_none() {
    let (_, store_handle) = register_store().await;
    handle::unregister(store_handle);
    assert!(OpGuard::enter(store_handle).is_none());
}

#[tokio::test]
async fn op_guard_increments_and_decrements_counter() {
    let (store, store_handle) = register_store().await;
    assert_eq!(store.in_flight.load(Ordering::Acquire), 0);
    {
        let _guard = OpGuard::enter(store_handle).expect("enter must succeed");
        assert_eq!(store.in_flight.load(Ordering::Acquire), 1);
    }
    assert_eq!(store.in_flight.load(Ordering::Acquire), 0);
    handle::unregister(store_handle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_invalid_and_await_blocks_until_drained() {
    let (store, store_handle) = register_store().await;
    let guard = OpGuard::enter(store_handle).expect("enter must succeed");

    let store_for_closer = store.clone();
    let closer = {
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move { store_for_closer.mark_invalid_and_await().await })
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !store.invalid.load(Ordering::Acquire) {
        if std::time::Instant::now() > deadline {
            panic!("closer never set invalid=true");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    assert!(
        !closer.is_finished(),
        "closer must wait for the in-flight op"
    );

    // Proves the invalid-check-after-increment ordering: a fresh enter still rejects.
    assert!(OpGuard::enter(store_handle).is_none());

    drop(guard);
    closer.await.expect("closer join");
    handle::unregister(store_handle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ops_and_close_converge_cleanly() {
    const THREADS: usize = 8;
    const PRE_OBSERVED: usize = 8;
    const POST_OBSERVED: usize = 248;
    let (store, store_handle) = register_store().await;

    let barrier_start = Arc::new(Barrier::new(THREADS + 1));
    let barrier_observed = Arc::new(Barrier::new(THREADS + 1));
    let mut joins = Vec::new();
    for _ in 0..THREADS {
        let bs = barrier_start.clone();
        let bo = barrier_observed.clone();
        joins.push(thread::spawn(move || {
            bs.wait();
            // Pre-close guarantee: every worker observes the store at least PRE_OBSERVED
            // times before the close is attempted, removing the "did anyone observe it?"
            // timing dependency.
            for _ in 0..PRE_OBSERVED {
                let _guard = OpGuard::enter(store_handle)
                    .expect("pre-close enter must succeed — close has not been called yet");
            }
            bo.wait();
            let mut post = 0usize;
            for _ in 0..POST_OBSERVED {
                match OpGuard::enter(store_handle) {
                    Some(_guard) => post += 1,
                    None => break,
                }
            }
            post
        }));
    }

    barrier_start.wait();
    barrier_observed.wait();
    store.mark_invalid_and_await().await;

    let post_total: usize = joins.into_iter().map(|j| j.join().unwrap()).sum();
    // `post_total` may reasonably be zero if close wins every race, so the verifiable
    // invariant is the counter being quiescent — assert that, not the count.
    let _ = post_total;
    assert_eq!(store.in_flight.load(Ordering::Acquire), 0);
    handle::unregister(store_handle);
}

#[tokio::test]
async fn mark_invalid_and_await_does_not_deadlock_on_already_invalid() {
    let (store, store_handle) = register_store().await;
    store.mark_invalid_and_await().await;
    tokio::time::timeout(Duration::from_secs(1), store.mark_invalid_and_await())
        .await
        .expect("second mark_invalid_and_await must return without blocking");
    handle::unregister(store_handle);
}

/// The read options a handle's effective flags build carry its trust of the remote, whatever the
/// handle's local and remote flags and whether a session is in reach.
#[test]
fn read_options_carry_the_handles_trust_of_its_remote() {
    for trust_remote in [false, true] {
        for (no_remote, no_local) in [(false, false), (true, false), (false, true)] {
            let flags = EffectiveFlags {
                no_remote,
                no_local,
                trust_remote,
            };
            for has_remote_session in [false, true] {
                assert_eq!(
                    flags.read_options(has_remote_session).trust_remote,
                    trust_remote
                );
            }
        }
    }
}

/// A batch that interleaves partitions reuses each partition's session for the whole call rather
/// than resolving a new one each time the partition changes.
#[tokio::test]
async fn a_batch_reuses_the_session_of_each_partition_it_returns_to() {
    let bare = in_memory_for_tests("reuse").await;
    let store = StoreInternal::new(
        "reuse",
        bare.immutable.clone(),
        bare.mutable.clone(),
        Some(Arc::new(lore_revision::protocol::RemoteEndpoint::new(
            "lore://127.0.0.1:1",
            None,
        ))),
        BoundFlags::default(),
        false,
        lore_storage::local::store_lock::StoreHold::default(),
    );
    let (first, second) = (
        lore_base::types::Partition::from([0xa1u8; 16]),
        lore_base::types::Partition::from([0xb2u8; 16]),
    );

    let mut reuse = SessionReuse::default();
    let before = reuse
        .session_for(&store, first, true)
        .expect("a remote session");
    reuse
        .session_for(&store, second, true)
        .expect("a remote session");
    let after = reuse
        .session_for(&store, first, true)
        .expect("a remote session");

    assert!(Arc::ptr_eq(&before, &after));
}
