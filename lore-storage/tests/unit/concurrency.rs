// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsStr;
use std::sync::Arc;

use lore_storage::compress::FRAGMENT_SIZE_THRESHOLD;
use lore_storage::concurrency::*;
use tokio::sync::Semaphore;

fn budget_mib(value: &str) -> usize {
    fragment_budget_kib_from(Some(OsStr::new(value))) / 1024
}

#[test]
fn fragment_budget_defaults_to_one_gib_when_unset_or_blank() {
    assert_eq!(fragment_budget_kib_from(None), 1024 * 1024);
    assert_eq!(budget_mib(""), 1024);
    assert_eq!(budget_mib(" \t"), 1024);
}

#[test]
fn fragment_budget_takes_a_value_in_range() {
    assert_eq!(budget_mib("64"), 64);
    assert_eq!(budget_mib(" 64\n"), 64);
    assert_eq!(budget_mib("4"), 4);
    assert_eq!(budget_mib("524287"), 524_287);
}

#[test]
fn fragment_budget_raises_a_value_below_the_floor() {
    assert_eq!(budget_mib("0"), FRAGMENT_BUDGET_MINIMUM_MIB);
    assert_eq!(budget_mib("3"), FRAGMENT_BUDGET_MINIMUM_MIB);
}

/// A whole number too large to read is above the ceiling, not unparsable.
#[test]
fn fragment_budget_lowers_a_value_above_the_ceiling() {
    assert_eq!(budget_mib("524288"), FRAGMENT_BUDGET_MAXIMUM_MIB);
    assert_eq!(
        budget_mib("99999999999999999999999"),
        FRAGMENT_BUDGET_MAXIMUM_MIB
    );
}

#[test]
fn fragment_budget_uses_the_default_for_a_value_that_is_not_a_whole_number() {
    for value in ["abc", "-1", "1.5", "64MiB"] {
        assert_eq!(budget_mib(value), FRAGMENT_BUDGET_DEFAULT_MIB, "{value:?}");
    }
}

#[cfg(unix)]
#[test]
fn fragment_budget_uses_the_default_for_a_value_that_is_not_unicode() {
    use std::os::unix::ffi::OsStrExt;

    assert_eq!(
        fragment_budget_kib_from(Some(OsStr::from_bytes(b"6\xff4"))),
        FRAGMENT_BUDGET_DEFAULT_MIB * 1024
    );
}

#[test]
fn fragment_permit_count_minimum() {
    // Very small content should be clamped to FRAGMENT_MINIMUM_COST_KIB
    assert_eq!(fragment_permit_count(0), FRAGMENT_MINIMUM_COST_KIB);
    assert_eq!(fragment_permit_count(1), FRAGMENT_MINIMUM_COST_KIB);
    assert_eq!(fragment_permit_count(1024), FRAGMENT_MINIMUM_COST_KIB);
}

/// Above the limiter's total an acquire waits forever, and past `u32::MAX >> 3` tokio panics.
#[test]
fn fragment_permit_count_maximum() {
    assert_eq!(
        fragment_permit_count(usize::MAX),
        fragment_budget_kib() as u32
    );
}

/// The chunker charges two windows plus a chunk in one acquire, so a cap at one fragment
/// would silently under-reserve what it holds.
#[test]
fn a_multi_window_reservation_is_charged_in_full() {
    let two_windows_and_a_chunk = 5 * FRAGMENT_SIZE_THRESHOLD;

    assert_eq!(
        fragment_permit_count(two_windows_and_a_chunk),
        (two_windows_and_a_chunk / 1024) as u32
    );
}

#[test]
fn fragment_permit_count_mid_range() {
    // 100 KiB content -> ceil(100*1024/1024) = 100 permits
    let size = 100 * 1024;
    assert_eq!(fragment_permit_count(size), 100);
}

#[tokio::test]
async fn acquire_fragment_memory_permit_sizes_by_buffer() {
    // Inspect the permit's own `num_permits()` so the test does not sample
    // the global semaphore's available_permits (which other concurrent
    // tests perturb).
    let permit_small = acquire_fragment_memory_permit(1).await.expect("small");
    assert_eq!(
        permit_small.num_permits(),
        FRAGMENT_MINIMUM_COST_KIB as usize,
        "1-byte buffer should cost FRAGMENT_MINIMUM_COST_KIB permits"
    );
    drop(permit_small);

    let permit_mid = acquire_fragment_memory_permit(100 * 1024)
        .await
        .expect("mid");
    assert_eq!(
        permit_mid.num_permits(),
        100,
        "100 KiB buffer should cost 100 permits"
    );
    drop(permit_mid);
}

#[tokio::test(flavor = "multi_thread")]
async fn fragment_memory_permit_saturation_does_not_deadlock() {
    // Use a dedicated Arc<Semaphore> for this stress test so we don't
    // perturb the global fragment_limiter (other tests sample it). The
    // permit-sizing logic is the same function (fragment_permit_count),
    // the only difference is which semaphore we acquire against.
    let semaphore = Arc::new(Semaphore::new(16 * FRAGMENT_MINIMUM_COST_KIB as usize));

    const N: usize = 100;
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let semaphore = Arc::clone(&semaphore);
        handles.push(lore_base::lore_spawn!(async move {
            let permit_count = fragment_permit_count(1);
            let p = semaphore
                .acquire_many_owned(permit_count)
                .await
                .expect("acquire");
            drop(p);
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    assert_eq!(
        semaphore.available_permits(),
        16 * FRAGMENT_MINIMUM_COST_KIB as usize,
        "all permits must be released after the stress burst"
    );
}

/// The premise behind [`acquire_chunk_budget`]'s fallback: tokio assigns released
/// permits to queued waiters, so `try_acquire` keeps failing while any waiter is
/// queued even though the budget has room. If this ever stops holding, the
/// fallback is merely redundant rather than wrong.
#[tokio::test]
async fn a_queued_waiter_starves_try_acquire_of_released_permits() {
    let limiter = Arc::new(Semaphore::new(16));
    let held = Arc::clone(&limiter)
        .try_acquire_many_owned(16)
        .expect("budget starts free");

    // Polled to Pending, so the waiter is queued rather than merely spawned.
    let mut waiter = Box::pin(Arc::clone(&limiter).acquire_many_owned(16));
    assert!(
        futures::poll!(&mut waiter).is_pending(),
        "waiter not queued"
    );

    drop(held);
    assert_eq!(
        limiter.available_permits(),
        0,
        "waiter absorbed the release"
    );
    assert!(
        Arc::clone(&limiter).try_acquire_many_owned(4).is_err(),
        "try_acquire must fail behind a queued waiter — the whole reason \
             acquire_chunk_budget cannot treat that as saturation"
    );
}

#[tokio::test]
async fn chunk_budget_prefers_a_permit_when_the_budget_has_room() {
    let limiter = Arc::new(Semaphore::new(fragment_budget_kib()));
    let reserved = Arc::new(Semaphore::new(1));

    let budget = acquire_chunk_budget_from(&limiter, 100 * 1024, &reserved).await;

    assert_eq!(budget.map(|permit| permit.num_permits()), Some(100));
    assert_eq!(
        reserved.available_permits(),
        1,
        "reservation left untouched"
    );
}

#[tokio::test]
async fn chunk_budget_falls_back_to_the_reservation_behind_a_waiter() {
    let limiter = Arc::new(Semaphore::new(16));
    let reserved = Arc::new(Semaphore::new(1));
    let held = Arc::clone(&limiter)
        .try_acquire_many_owned(16)
        .expect("budget starts free");
    let mut waiter = Box::pin(Arc::clone(&limiter).acquire_many_owned(16));
    assert!(
        futures::poll!(&mut waiter).is_pending(),
        "waiter not queued"
    );
    drop(held);

    let budget = acquire_chunk_budget_from(&limiter, 1, &reserved).await;

    assert!(budget.is_some(), "no budget granted");
    assert_eq!(reserved.available_permits(), 0, "reservation not used");
}

/// With the reservation in use, the wait resolves from whichever side frees
/// first — here the reservation, since the limiter stays exhausted.
#[tokio::test]
async fn chunk_budget_waits_for_the_reservation_to_come_back() {
    let limiter = Arc::new(Semaphore::new(16));
    let reserved = Arc::new(Semaphore::new(1));
    let _held = Arc::clone(&limiter)
        .try_acquire_many_owned(16)
        .expect("budget starts free");
    let slot = Arc::clone(&reserved)
        .try_acquire_owned()
        .expect("reservation starts free");

    let mut pending = Box::pin(acquire_chunk_budget_from(&limiter, 1, &reserved));
    assert!(
        futures::poll!(&mut pending).is_pending(),
        "no budget and no reservation: must wait"
    );

    drop(slot);
    let budget = pending.await;

    assert!(budget.is_some(), "reservation not reclaimed");
    assert_eq!(reserved.available_permits(), 0, "reservation not accounted");
}

/// The reservation must come back when the *buffer* is released, not when the task that
/// acquired it ends. A chunk's write continues in a detached task, so releasing at
/// dispatch frees the reservation while those bytes are still resident — and frees it
/// immediately, letting one file hand its whole content to detached writes with nothing
/// charged against the limiter.
#[tokio::test]
async fn the_reservation_is_held_past_the_task_that_acquired_it() {
    let limiter = Arc::new(Semaphore::new(16));
    let reserved = Arc::new(Semaphore::new(1));
    let _held = Arc::clone(&limiter)
        .try_acquire_many_owned(16)
        .expect("budget starts free");

    let budget = acquire_chunk_budget_from(&limiter, 1, &reserved).await;
    assert_eq!(reserved.available_permits(), 0, "reservation not used");

    // The dispatching task ends and the budget travels on with the buffer, as it does
    // into a leader task.
    let budget = lore_base::lore_spawn!(async move { budget })
        .await
        .expect("dispatch task joins");
    assert_eq!(
        reserved.available_permits(),
        0,
        "reservation came back when the dispatching task ended"
    );

    drop(budget);
    assert_eq!(
        reserved.available_permits(),
        1,
        "reservation not released with the buffer"
    );
}

#[tokio::test]
async fn fragment_limiter_owned_shares_budget_with_borrowed() {
    // The two handles MUST reference the same underlying Semaphore so
    // permits acquired from one count against the other's budget. Assert
    // pointer equality directly instead of sampling the budget (which
    // other concurrent tests perturb).
    let borrowed: *const Semaphore = fragment_limiter();
    let owned_arc = fragment_limiter_owned();
    let owned: *const Semaphore = Arc::as_ptr(&owned_arc);
    assert_eq!(
        borrowed, owned,
        "fragment_limiter and fragment_limiter_owned must share the same semaphore"
    );
}
