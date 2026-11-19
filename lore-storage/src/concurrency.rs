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

/// Least permits, in KiB, a fragment buffer costs against the fragment budget, which otherwise
/// charges `ceil(content_size / 1024)`, so tiny fragments bound the fragment count as well as
/// the bytes: about 262k of them under the default budget.
pub const FRAGMENT_MINIMUM_COST_KIB: u32 = 4;

/// Environment variable that sets the process-wide fragment budget in MiB, which bounds the
/// fragment buffers of writes, streamed reads and reads into a file. Read once per process, on
/// first use.
const FRAGMENT_BUDGET_VAR: &str = "LORE_FRAGMENT_BUDGET_MIB";

/// Fragment budget when [`FRAGMENT_BUDGET_VAR`] is unset, blank or does not parse (1 GiB).
#[lore_macro::test_pub]
const FRAGMENT_BUDGET_DEFAULT_MIB: usize = 1024;

/// Least fragment budget [`FRAGMENT_BUDGET_VAR`] sets: room for the reservations of three
/// files' chunkers at once.
#[lore_macro::test_pub]
const FRAGMENT_BUDGET_MINIMUM_MIB: usize = 4;

/// Greatest fragment budget [`FRAGMENT_BUDGET_VAR`] sets: the most whole MiB within
/// `u32::MAX >> 3` KiB, the most permits a tokio semaphore holds on a 32-bit target.
#[lore_macro::test_pub]
const FRAGMENT_BUDGET_MAXIMUM_MIB: usize = (u32::MAX >> 3) as usize / 1024;

static FILE_COUNT_LIMITER: OnceLock<Semaphore> = OnceLock::<Semaphore>::new();
static FRAGMENT_BUDGET_KIB: OnceLock<usize> = OnceLock::new();
static FRAGMENT_LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();
static COMPRESS_LIMITER: OnceLock<Option<Arc<Semaphore>>> = OnceLock::new();

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
    FRAGMENT_LIMITER.get_or_init(|| Arc::new(Semaphore::new(fragment_budget_kib())))
}

/// Total permits of the fragment limiter, in KiB, read from [`FRAGMENT_BUDGET_VAR`] on first use.
#[lore_macro::test_pub]
pub(crate) fn fragment_budget_kib() -> usize {
    *FRAGMENT_BUDGET_KIB.get_or_init(|| {
        let budget = fragment_budget_kib_from(std::env::var_os(FRAGMENT_BUDGET_VAR).as_deref());
        lore_base::lore_debug!("Fragment budget set to {budget} KiB");
        budget
    })
}

/// The fragment budget in KiB that a value of [`FRAGMENT_BUDGET_VAR`] sets, where `None` or a
/// blank value counts as unset.
///
/// A value that does not parse as a whole number of MiB yields the default, and one outside
/// [`FRAGMENT_BUDGET_MINIMUM_MIB`]..=[`FRAGMENT_BUDGET_MAXIMUM_MIB`], however large, is clamped
/// into it. Each logs a warning, since the variable is read where no error can reach the
/// operator.
#[lore_macro::test_pub]
fn fragment_budget_kib_from(value: Option<&std::ffi::OsStr>) -> usize {
    let Some(value) = value else {
        return FRAGMENT_BUDGET_DEFAULT_MIB * 1024;
    };
    let text = value.to_str().map(str::trim);
    if text == Some("") {
        return FRAGMENT_BUDGET_DEFAULT_MIB * 1024;
    }
    let parsed = text.and_then(|text| match text.parse::<usize>() {
        Ok(mib) => Some(mib),
        Err(error) if *error.kind() == std::num::IntErrorKind::PosOverflow => Some(usize::MAX),
        Err(_) => None,
    });
    let Some(mib) = parsed else {
        lore_base::lore_warn!(
            "{FRAGMENT_BUDGET_VAR} of {value:?} is not a whole number of MiB, using the default \
             {FRAGMENT_BUDGET_DEFAULT_MIB} MiB"
        );
        return FRAGMENT_BUDGET_DEFAULT_MIB * 1024;
    };
    let clamped = mib.clamp(FRAGMENT_BUDGET_MINIMUM_MIB, FRAGMENT_BUDGET_MAXIMUM_MIB);
    if clamped != mib {
        lore_base::lore_warn!(
            "{FRAGMENT_BUDGET_VAR} of {value:?} is outside {FRAGMENT_BUDGET_MINIMUM_MIB} to \
             {FRAGMENT_BUDGET_MAXIMUM_MIB} MiB, using {clamped} MiB"
        );
    }
    clamped * 1024
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

#[lore_macro::test_pub]
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
/// Capped at the limiter's own total, [`fragment_budget_kib`]: a larger request could never be
/// satisfied, and [`FRAGMENT_BUDGET_MAXIMUM_MIB`] keeps the total within what tokio acquires.
/// Not capped at one fragment — the chunker charges whole windows here. A size taken from a
/// fragment list is bounded before it arrives, by the tier check in `walk_leaf_level`.
pub fn fragment_permit_count(content_size: usize) -> u32 {
    content_size
        .div_ceil(1024)
        .clamp(FRAGMENT_MINIMUM_COST_KIB as usize, fragment_budget_kib()) as u32
}

#[error_set]
pub enum SemaphoreError {}
