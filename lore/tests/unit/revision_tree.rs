// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod add;
mod call;
mod close;
mod commit;
mod delete;
mod handle;
mod info;
mod list_children;
mod load;
mod metadata_clear;
mod metadata_get;
mod metadata_set;
mod modify;
mod move_node;
mod node_info;
mod node_path;
mod resolve_path;

/// Round-trip a `RevisionTreeInternal` through the registry: a fresh
/// registration produces a non-zero handle; `lookup` returns the same
/// `Arc`; `unregister` removes the entry so subsequent `lookup`
/// returns `None`.
#[tokio::test]
async fn registry_register_lookup_unregister_round_trip() {
    use std::sync::Arc;

    use lore::revision_tree::handle;

    use crate::revision_tree::handle::test_support;

    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal.clone());
    assert_ne!(handle_value.handle_id, 0);
    let looked_up = handle::lookup(handle_value).expect("registered handle must look up");
    assert!(Arc::ptr_eq(&looked_up, &internal));
    let removed = handle::unregister(handle_value).expect("first unregister returns the held Arc");
    assert!(Arc::ptr_eq(&removed, &internal));
    assert!(handle::lookup(handle_value).is_none());
}

/// Unregistering an already-removed handle returns `None`. The second
/// close call from the C side must see a defined miss, not a panic or
/// a stale double-drop.
#[tokio::test]
async fn registry_double_unregister_returns_none() {
    use lore::revision_tree::handle;

    use crate::revision_tree::handle::test_support;

    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal);
    assert!(handle::unregister(handle_value).is_some());
    assert!(handle::unregister(handle_value).is_none());
}

/// The `INVALID` sentinel must never match a real registry entry.
/// Lookup and unregister on it return `None` unconditionally.
#[test]
fn registry_invalid_sentinel_misses() {
    use lore::revision_tree::handle;
    use lore::revision_tree::handle::LoreRevisionTree;

    assert!(handle::lookup(LoreRevisionTree::INVALID).is_none());
    assert!(handle::unregister(LoreRevisionTree::INVALID).is_none());
}

/// Each call to `register` produces a distinct `handle_id`.
/// Two concurrent registrations against the same `Arc` must not
/// collide.
#[tokio::test]
async fn registry_two_registrations_produce_distinct_ids() {
    use lore::revision_tree::handle;

    use crate::revision_tree::handle::test_support;

    let a_internal = test_support::new_for_testing().await;
    let b_internal = test_support::new_for_testing().await;
    let a = handle::register(a_internal);
    let b = handle::register(b_internal);
    assert_ne!(a.handle_id, b.handle_id);
    handle::unregister(a);
    handle::unregister(b);
}

/// `RevisionTreeGuard::enter` increments the in-flight counter while
/// the guard is live; dropping it decrements. Concurrent enters
/// observe the counter at or above the number of live guards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guard_increments_and_drops_decrement_in_flight_counter() {
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::Ordering;
    use std::thread;

    use lore::revision_tree::handle;
    use lore::revision_tree::handle::RevisionTreeGuard;

    use crate::revision_tree::handle::test_support;

    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal.clone());
    assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);

    const THREADS: usize = 8;
    let start = Arc::new(Barrier::new(THREADS + 1));
    let observed = Arc::new(Barrier::new(THREADS + 1));
    let release = Arc::new(Barrier::new(THREADS + 1));
    let mut joins = Vec::new();
    for _ in 0..THREADS {
        let start = start.clone();
        let observed = observed.clone();
        let release = release.clone();
        joins.push(thread::spawn(move || {
            start.wait();
            let guard = RevisionTreeGuard::enter(handle_value)
                .expect("enter must succeed on a registered, non-invalid handle");
            observed.wait();
            release.wait();
            drop(guard);
        }));
    }
    start.wait();
    observed.wait();
    assert_eq!(internal.in_flight.load(Ordering::Acquire), THREADS as u64);
    release.wait();
    for j in joins {
        j.join().unwrap();
    }
    assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);
    handle::unregister(handle_value);
}

/// `RevisionTreeGuard::enter` returns `None` when the handle has
/// already been marked invalid. The increment-then-check ordering
/// ensures the counter is balanced even on the rejection path.
#[tokio::test]
async fn guard_enter_after_mark_invalid_returns_none() {
    use std::sync::atomic::Ordering;

    use lore::revision_tree::handle;
    use lore::revision_tree::handle::RevisionTreeGuard;

    use crate::revision_tree::handle::test_support;

    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal.clone());
    internal.invalid.store(true, Ordering::Release);
    assert!(RevisionTreeGuard::enter(handle_value).is_none());
    assert_eq!(internal.in_flight.load(Ordering::Acquire), 0);
    handle::unregister(handle_value);
}

/// A close racing an enter either waits for the op that entered or refuses it: the two never
/// both miss each other, with the close returning while the op runs.
#[tokio::test]
async fn a_close_racing_an_enter_waits_for_it_or_refuses_it() {
    use std::sync::atomic::Ordering;

    use futures::FutureExt;
    use lore::revision_tree::handle;
    use lore::revision_tree::handle::RevisionTreeGuard;

    use crate::close_race;
    use crate::revision_tree::handle::test_support;

    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal.clone());
    let missed = close_race::rounds_missing_each_other(
        || RevisionTreeGuard::enter(handle_value),
        || internal.mark_invalid_and_await().now_or_never().is_some(),
        || internal.invalid.store(false, Ordering::SeqCst),
    );
    handle::unregister(handle_value);
    assert_eq!(missed, 0, "a close returned while an op it admitted ran");
}

/// `RevisionTreeGuard::enter` returns `None` when the handle is
/// unknown (never registered or already unregistered).
#[tokio::test]
async fn guard_enter_unregistered_handle_returns_none() {
    use lore::revision_tree::handle;
    use lore::revision_tree::handle::RevisionTreeGuard;

    use crate::revision_tree::handle::test_support;

    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal);
    handle::unregister(handle_value);
    assert!(RevisionTreeGuard::enter(handle_value).is_none());
}

/// The cascade takes the handles loaded against one storage handle and no others.
/// Keyed on ids no other test can be using, since it sweeps the live registry.
#[tokio::test]
async fn drain_for_storage_handle_takes_only_the_handles_loaded_on_it() {
    use lore::revision_tree::handle;

    use crate::revision_tree::handle::test_support;

    const OWNER: u64 = 0xC105_E001;
    const SIBLING: u64 = 0xC105_E002;

    let first = handle::register(test_support::new_for_testing_on_storage_handle(OWNER).await);
    let second = handle::register(test_support::new_for_testing_on_storage_handle(OWNER).await);
    let other = handle::register(test_support::new_for_testing_on_storage_handle(SIBLING).await);

    let drained: Vec<u64> = handle::drain_for_storage_handle(OWNER)
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    assert_eq!(
        drained.len(),
        2,
        "both handles on the closing storage handle must drain, got {drained:?}"
    );
    assert!(drained.contains(&first.handle_id));
    assert!(drained.contains(&second.handle_id));
    assert!(handle::lookup(first).is_none());
    assert!(handle::lookup(second).is_none());
    assert!(
        handle::lookup(other).is_some(),
        "a handle loaded against another storage handle must survive",
    );

    handle::unregister(other);
}

/// The worker both close paths funnel through. Driven with an explicit entry list, so
/// it cannot close handles other tests own.
#[tokio::test]
async fn drain_in_parallel_marks_each_handle_invalid() {
    use std::sync::atomic::Ordering;

    use lore::revision_tree::drain_in_parallel;

    use crate::revision_tree::handle::test_support;

    let first = test_support::new_for_testing().await;
    let second = test_support::new_for_testing().await;

    drain_in_parallel(vec![(1, first.clone()), (2, second.clone())]).await;

    for internal in [first, second] {
        assert!(
            internal.invalid.load(Ordering::Acquire),
            "every drained handle must be marked invalid",
        );
    }
}

/// An op that will not finish — a commit uploading over the connection that just
/// dropped — must not hold the teardown open.
#[tokio::test]
async fn a_teardown_cascade_gives_up_on_an_op_that_will_not_finish() {
    use std::time::Duration;

    use lore::revision_tree::close_for_storage_handle_within;
    use lore::revision_tree::handle;
    use lore::revision_tree::handle::RevisionTreeGuard;

    use crate::revision_tree::handle::test_support;

    const OWNER: u64 = 0xC105_E005;

    let stuck = handle::register(test_support::new_for_testing_on_storage_handle(OWNER).await);
    let guard = RevisionTreeGuard::enter(stuck).expect("guard enter must succeed");

    close_for_storage_handle_within(OWNER, Duration::from_millis(20)).await;

    assert!(
        handle::lookup(stuck).is_none(),
        "the handle is unregistered before the wait, so the timeout still reclaims it",
    );
    drop(guard);
}

/// Shutdown must not tear down a handle out from under a call still running on
/// it: the drain parks until the in-flight counter reaches zero.
#[allow(clippy::disallowed_methods)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_in_parallel_waits_for_an_in_flight_op() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use lore::revision_tree::drain_in_parallel;
    use lore::revision_tree::handle;
    use lore::revision_tree::handle::RevisionTreeGuard;

    use crate::revision_tree::handle::test_support;

    let internal = test_support::new_for_testing().await;
    let handle_value = handle::register(internal.clone());
    let guard = RevisionTreeGuard::enter(handle_value).expect("guard enter must succeed");

    let entries = vec![(handle_value.handle_id, internal.clone())];
    let drain = tokio::spawn(async move { drain_in_parallel(entries).await });

    // A task that was never polled is also not finished, so wait for the drain to be
    // observably inside the await before asserting it is parked.
    let deadline = Instant::now() + Duration::from_secs(1);
    while !internal.invalid.load(Ordering::Acquire) {
        if Instant::now() > deadline {
            panic!("the drain never reached the handle");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        !drain.is_finished(),
        "the drain must park while an op is in flight",
    );

    drop(guard);
    drain.await.expect("drain task join");

    handle::unregister(handle_value);
}
