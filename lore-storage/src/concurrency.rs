// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic;

use lore_error_set::prelude::*;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;

/// Minimum fragment size for chunking (32 KiB).
pub const FRAGMENT_SIZE_MINIMUM: usize = 32 * 1024;

pub use lore_base::types::FRAGMENT_SIZE_EXPECTED;

/// Default file count concurrency limit when not configured.
pub const FILE_COUNT_LIMIT_DEFAULT: usize = 10000;

// Fragment concurrency is budgeted in KiB units. The total budget allows ~4000
// maximum-size (256 KiB) fragments in flight simultaneously (1 GiB total).
// Each fragment acquires ceil(content_size / 1024) permits, floored at
// FRAGMENT_MINIMUM_COST_KIB so tiny fragments bound the fragment count as well as
// the bytes (~265k of them).
pub const FRAGMENT_BUDGET_KIB: usize = 1024 * 1024; // 1 GiB
pub const FRAGMENT_MINIMUM_COST_KIB: u32 = 4;

static FILE_COUNT_LIMITER: OnceLock<Semaphore> = OnceLock::<Semaphore>::new();
static FRAGMENT_LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();
static COMPRESS_LIMITER: OnceLock<Option<Arc<Semaphore>>> = OnceLock::new();

/// When true, load operations enforce repository isolation.
pub static LOCAL_ISOLATION: atomic::AtomicBool = atomic::AtomicBool::new(false);

/// Configured file count limit. Set via [`configure`] before first use.
static FILE_COUNT_LIMIT_CONFIG: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

/// Configured compress task limit. Set via [`configure_compress_limiter`] before first use.
static COMPRESS_LIMIT_CONFIG: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

/// Configure the file count concurrency limit.
///
/// Must be called before the first call to [`file_count_limiter`] or
/// [`file_count_limit_acquire`]; later calls have no effect because the
/// semaphore is initialised on first use.
pub fn configure(file_count_limit: usize) {
    FILE_COUNT_LIMIT_CONFIG.store(file_count_limit, atomic::Ordering::Relaxed);
}

/// Configure the compress task concurrency limit.
///
/// A limit of 0 (default) disables the limiter. Must be called before the
/// first call to [`compress_limit_acquire`]; later calls have no effect.
pub fn configure_compress_limiter(limit: usize) {
    COMPRESS_LIMIT_CONFIG.store(limit, atomic::Ordering::Relaxed);
}

/// Return the global compress limiter, creating it on first access.
/// Returns `None` if no limit was configured (limit == 0).
fn compress_limiter() -> &'static Option<Arc<Semaphore>> {
    COMPRESS_LIMITER.get_or_init(|| {
        let limit = COMPRESS_LIMIT_CONFIG.load(atomic::Ordering::Relaxed);
        if limit > 0 {
            lore_base::lore_debug!("Compress task limit set to {limit}");
            Some(Arc::new(Semaphore::new(limit)))
        } else {
            None
        }
    })
}

/// Acquire a permit from the compress limiter if one is configured.
/// Returns `None` if no compress limit is active.
///
/// No limit is the default and not a missing bound: compression is synchronous on a core worker,
/// so at most one runs per worker regardless, each holding one output buffer. The limiter reserves
/// CPU for other work — a limit at the worker count would bound nothing.
pub async fn compress_limit_acquire() -> Option<SemaphorePermit<'static>> {
    if let Some(semaphore) = compress_limiter().as_deref() {
        semaphore.acquire().await.ok()
    } else {
        None
    }
}

/// Return the global file-count semaphore, creating it on first access.
pub fn file_count_limiter() -> &'static Semaphore {
    FILE_COUNT_LIMITER.get_or_init(|| {
        Semaphore::new({
            let mut limit = FILE_COUNT_LIMIT_CONFIG.load(atomic::Ordering::Relaxed);
            if limit == 0 {
                limit = FILE_COUNT_LIMIT_DEFAULT;
            }
            lore_base::lore_debug!("File parallel count limit set to {limit}");
            limit
        })
    })
}

/// Acquire a permit from the file-count limiter.
pub async fn file_count_limit_acquire() -> Result<SemaphorePermit<'static>, SemaphoreError> {
    file_count_limiter()
        .acquire()
        .await
        .internal("Failed to acquire file limit permit")
        .map_err(SemaphoreError::from)
}

/// Return the global fragment-budget semaphore, creating it on first access.
pub fn fragment_limiter() -> &'static Semaphore {
    fragment_limiter_arc()
}

/// Return a cloneable owning handle to the global fragment-budget semaphore.
///
/// Permits acquired from this handle via [`Semaphore::acquire_many_owned`] share
/// the same budget as permits acquired from [`fragment_limiter`]; they can be
/// moved into spawned tasks and released independently.
pub fn fragment_limiter_owned() -> Arc<Semaphore> {
    Arc::clone(fragment_limiter_arc())
}

fn fragment_limiter_arc() -> &'static Arc<Semaphore> {
    FRAGMENT_LIMITER.get_or_init(|| Arc::new(Semaphore::new(FRAGMENT_BUDGET_KIB)))
}

/// Acquire an owned memory permit sized for a fragment buffer of `buffer_len`
/// bytes. The permit can be moved into a spawned task and is released when
/// dropped. Returns `None` if the fragment limiter has been closed.
pub async fn acquire_fragment_memory_permit(
    buffer_len: usize,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    fragment_limiter_owned()
        .acquire_many_owned(fragment_permit_count(buffer_len))
        .await
        .ok()
}

/// Budget for a chunk buffer of `buffer_len` bytes, preferring a fresh permit from the
/// fragment limiter and falling back to `reserved` — the one chunk the caller's chunker
/// already paid for. `None` only if the limiter has been closed.
///
/// The two sources are one return value because they have one lifetime: whichever it is
/// must be dropped where the buffer is, and the buffer's write may continue in a detached
/// task long after the caller that acquired this has returned. Handing back a pair let a
/// caller move the permit into that task and leave the reservation behind, which released
/// it at dispatch and stopped accounting for bytes that were still resident — and since the
/// reservation was then free again immediately, a file could stream its whole content into
/// detached writes without the limiter charging any of it.
///
/// A failed `try_acquire` does **not** mean the budget is exhausted: tokio assigns
/// released permits straight to queued waiters and only returns the remainder to the
/// semaphore once the queue empties, so one waiter anywhere keeps `try_acquire`
/// failing for every caller. Reading that as saturation is what made large files
/// degrade to one chunk at a time whenever any other file is waiting.
///
/// If `reserved` is in use too this waits for whichever frees first, which cannot
/// stall: the chunk holding `reserved` needs no budget to finish. What the caller
/// must not do is wait on the limiter *alone* while holding a chunker window, since
/// that is budget only it could release.
pub async fn acquire_chunk_budget(
    buffer_len: usize,
    reserved: &Arc<Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    acquire_chunk_budget_from(fragment_limiter_arc(), buffer_len, reserved).await
}

async fn acquire_chunk_budget_from(
    limiter: &Arc<Semaphore>,
    buffer_len: usize,
    reserved: &Arc<Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let permits = fragment_permit_count(buffer_len);
    if let Ok(permit) = Arc::clone(limiter).try_acquire_many_owned(permits) {
        return Some(permit);
    }
    if let Ok(slot) = Arc::clone(reserved).try_acquire_owned() {
        return Some(slot);
    }
    tokio::select! {
        permit = Arc::clone(limiter).acquire_many_owned(permits) => permit.ok(),
        slot = Arc::clone(reserved).acquire_owned() => slot.ok(),
    }
}

/// Permits a buffer of `content_size` bytes costs against the fragment limiter.
///
/// Capped at the limiter's own total: a larger request could never be satisfied, and one past
/// `u32::MAX >> 3` panics inside tokio. Not capped at one fragment — the chunker charges whole
/// windows here. A size taken from a fragment list is bounded before it arrives, by the tier
/// check in `walk_leaf_level`.
pub fn fragment_permit_count(content_size: usize) -> u32 {
    content_size
        .div_ceil(1024)
        .clamp(FRAGMENT_MINIMUM_COST_KIB as usize, FRAGMENT_BUDGET_KIB) as u32
}

#[error_set]
pub enum SemaphoreError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::FRAGMENT_SIZE_THRESHOLD;

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
            FRAGMENT_BUDGET_KIB as u32
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
        let limiter = Arc::new(Semaphore::new(FRAGMENT_BUDGET_KIB));
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
}
