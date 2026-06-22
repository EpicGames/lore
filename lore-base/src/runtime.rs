// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use parking_lot::Mutex;
use pin_project::pin_project;
use pin_project::pinned_drop;
use serde::Deserialize;
use tokio::runtime::Handle;
use tokio::task::JoinSet;

// ---------------------------------------------------------------------------
// Instruments
// ---------------------------------------------------------------------------

pub enum LoreTaskLifecycleEvent {
    Started,
    Completed,
    Dropped,
}

pub type RuntimeTaskEventCallback =
    Box<dyn Fn(LoreTaskLifecycleEvent, &LoreTaskSpawnLocation) + Send + Sync>;

static RUNTIME_TASK_EVENTS: OnceLock<RuntimeTaskEventCallback> = OnceLock::new();

pub fn set_task_lifecycle_callback(callback: RuntimeTaskEventCallback) -> bool {
    let result = RUNTIME_TASK_EVENTS.set(callback);

    result.is_ok()
}

pub struct LoreTaskSpawnLocation {
    pub file: &'static str,
    pub line: u32,
}

#[pin_project(PinnedDrop)]
pub struct ObservedTask<F> {
    #[pin]
    inner: F,
    location: LoreTaskSpawnLocation,
    ran_to_completion: bool,
}

impl<F> ObservedTask<F> {
    /// Wraps a future with state events.
    ///
    /// If runtime callback has not been initialised yet, the wrapper is
    /// inert
    #[track_caller]
    pub fn new(inner: F) -> Self {
        let caller = ::std::panic::Location::caller();
        let location = LoreTaskSpawnLocation {
            file: caller.file(),
            line: caller.line(),
        };

        if let Some(callback) = RUNTIME_TASK_EVENTS.get() {
            callback(LoreTaskLifecycleEvent::Started, &location);
        }

        Self {
            inner,
            location,
            ran_to_completion: false,
        }
    }
}

impl<F: Future> Future for ObservedTask<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = this.inner.poll(cx);
        if result.is_ready() {
            *this.ran_to_completion = true;
            if let Some(callback) = RUNTIME_TASK_EVENTS.get() {
                callback(LoreTaskLifecycleEvent::Completed, this.location);
            }
        }
        result
    }
}

#[pinned_drop]
impl<F> PinnedDrop for ObservedTask<F> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if !*this.ran_to_completion
            && let Some(callback) = RUNTIME_TASK_EVENTS.get()
        {
            callback(LoreTaskLifecycleEvent::Dropped, this.location);
        }
    }
}

// ---------------------------------------------------------------------------
// Opaque task-local context
// ---------------------------------------------------------------------------

tokio::task_local! {
    /// Opaque task-local context propagated by `lore_spawn!`.
    /// `lore` sets this to `Arc<ExecutionContext>`. Transport and storage
    /// code propagate it without knowing the concrete type.
    pub static LORE_CONTEXT: Arc<dyn Any + Send + Sync>;
}

/// Get the current task-local context. Panics if not set.
pub fn lore_context() -> Arc<dyn Any + Send + Sync> {
    LORE_CONTEXT.get()
}

/// Get the current task-local context, or `None` if not set.
pub fn try_lore_context() -> Option<Arc<dyn Any + Send + Sync>> {
    LORE_CONTEXT.try_with(|ctx| ctx.clone()).ok()
}

// ---------------------------------------------------------------------------
// Spawn macros — propagate LORE_CONTEXT to spawned tasks
// ---------------------------------------------------------------------------

/// Spawns a task with `LORE_CONTEXT` propagated.
///
/// Variants:
/// - `lore_spawn!(future)` — spawn on default runtime
/// - `lore_spawn!("name", future)` — named spawn
/// - `lore_spawn!(joinset, future)` — spawn into `JoinSet`
/// - `lore_spawn!(joinset, "name", future)` — named spawn into `JoinSet`
///
/// If no `LORE_CONTEXT` is set, spawns without context scoping.
#[macro_export]
macro_rules! lore_spawn {
    ($joinset:ident, $name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $joinset.spawn_on(
                    $crate::runtime::LORE_CONTEXT.scope(__ctx, __task),
                    &$crate::runtime::runtime(),
                )
            } else {
                $joinset.spawn_on(__task, &$crate::runtime::runtime())
            }
        }
    }};
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $joinset.spawn_on(
                    $crate::runtime::LORE_CONTEXT.scope(__ctx, __task),
                    &$crate::runtime::runtime(),
                )
            } else {
                $joinset.spawn_on(__task, &$crate::runtime::runtime())
            }
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::runtime().spawn($crate::runtime::LORE_CONTEXT.scope(__ctx, __task))
            } else {
                $crate::runtime::runtime().spawn(__task)
            }
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::runtime().spawn($crate::runtime::LORE_CONTEXT.scope(__ctx, __task))
            } else {
                $crate::runtime::runtime().spawn(__task)
            }
        }
    }};
}

/// Spawns a blocking task with `LORE_CONTEXT` set.
///
/// Uses `sync_scope` to make the context available in the blocking closure.
#[macro_export]
macro_rules! lore_spawn_blocking {
    ($joinset:ident, $name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $joinset.spawn_blocking_on(
                    move || $crate::runtime::LORE_CONTEXT.sync_scope(__ctx, $expression),
                    &$crate::runtime::runtime(),
                )
            } else {
                $joinset.spawn_blocking_on($expression, &$crate::runtime::runtime())
            }
        }
    }};
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $joinset.spawn_blocking_on(
                    move || $crate::runtime::LORE_CONTEXT.sync_scope(__ctx, $expression),
                    &$crate::runtime::runtime(),
                )
            } else {
                $joinset.spawn_blocking_on($expression, &$crate::runtime::runtime())
            }
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::runtime().spawn_blocking(move || {
                    $crate::runtime::LORE_CONTEXT.sync_scope(__ctx, $expression)
                })
            } else {
                $crate::runtime::runtime().spawn_blocking($expression)
            }
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::runtime().spawn_blocking(move || {
                    $crate::runtime::LORE_CONTEXT.sync_scope(__ctx, $expression)
                })
            } else {
                $crate::runtime::runtime().spawn_blocking($expression)
            }
        }
    }};
}

/// Spawns a blocking task without context propagation.
#[macro_export]
macro_rules! lore_spawn_blocking_nocontext {
    ($joinset:ident, $name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $joinset.spawn_blocking_on($expression, &$crate::runtime::runtime())
        }
    }};
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $joinset.spawn_blocking_on($expression, &$crate::runtime::runtime())
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::runtime().spawn_blocking($expression)
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::runtime().spawn_blocking($expression)
        }
    }};
}

/// Drains a set of tasks to completion and collect the first encountered error.
#[macro_export]
macro_rules! lore_drain_tasks {
    ($tasks:expr, $join_err:expr) => {{
        {
            let mut __failure = None;
            while let Some(__res) = $tasks.join_next().await {
                __failure = __failure.or(__res.map_err(|_| $join_err).flatten().err());
            }
            match __failure {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }};
}

#[macro_export]
macro_rules! lore_limit_drain_tasks {
    ($tasks:expr, $max_count:expr, $join_err:expr) => {{
        {
            let mut __failure = None;
            while let Some(__res) = $tasks.try_join_next() {
                __failure = __failure.or(__res.map_err(|_| $join_err).flatten().err());
            }
            while $tasks.len() > $max_count
                && let Some(__res) = $tasks.join_next().await
            {
                __failure = __failure.or(__res.map_err(|_| $join_err).flatten().err());
            }
            match __failure {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }};
}

/// Spawns a guarded task with `LORE_CONTEXT` propagation.
/// The task is awaited during `runtime_flush_guarded()` or `runtime_shutdown_timeout()`.
#[macro_export]
macro_rules! lore_spawn_guarded {
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let mut __tasks = $crate::runtime::RUNTIME_GUARD
                .get_or_init(|| parking_lot::Mutex::new(tokio::task::JoinSet::new()))
                .lock();
            while __tasks.try_join_next().is_some() {}
            $crate::lore_spawn!(__tasks, $expression);
        }
    }};
}

static DEFAULT_RUNTIME: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);
static DEFAULT_THREAD_KEEP_ALIVE_SECONDS: u64 = 10;

/// Shared compute thread pool used by CPU-bound work (compression, hashing,
/// etc.) that needs isolation from rayon's global pool and from the tokio
/// runtime. `OnceLock` gives a lock-free reference on the hot path; the
/// pool is eagerly built alongside the tokio runtime in
/// [`runtime_with_settings`] so the first dispatch does not pay init cost.
/// Not dropped at shutdown — tokio drain ensures no work is in flight, and
/// process exit terminates the worker threads.
static COMPUTE_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// Stack size for compute-pool worker threads. Compression (zstd/oodle/lz4),
/// hashing and similar CPU-bound work hold their state in heap-allocated
/// contexts and scratch buffers, not on the stack. 256 KiB leaves generous
/// headroom compared with the few-KiB the worker hot path actually uses,
/// while saving ~1.75 MiB of virtual memory per worker vs the Rust default
/// (2 MiB on Linux).
const COMPUTE_POOL_STACK_SIZE: usize = 256 * 1024;

fn build_compute_pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(compute_pool_thread_count())
        .thread_name(|i| format!("lore-compute-{i}"))
        .stack_size(COMPUTE_POOL_STACK_SIZE)
        .build()
        .expect("Failed to build compute pool")
}

#[cfg(target_os = "windows")]
fn platform_processor_count() -> usize {
    // std::thread::available_parallelism underestimates number of cores on a 128-core threadripper
    // Use the Win32 API to get total processor count of all processor groups
    unsafe extern "system" {
        fn GetActiveProcessorCount(groups: u16) -> u32;
    }
    unsafe { GetActiveProcessorCount(0xFFFF) as usize }
}

/// Returns the number of available processors.
///
/// On Windows, takes the maximum of `std::thread::available_parallelism` and the Win32
/// `GetActiveProcessorCount` API, since the former underestimates on large machines.
/// On other platforms, returns `std::thread::available_parallelism` (falling back to 1).
pub fn processor_count() -> usize {
    let std_count = std::thread::available_parallelism().map_or(1, |c| c.get());
    #[cfg(target_os = "windows")]
    {
        std::cmp::max(std_count, platform_processor_count())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std_count
    }
}

/// Optional ceiling on the *sum* of the worker, blocking and compute pool
/// sizes. Set once via [`set_thread_limit`]; `0` means "no limit". The
/// `LORE_MAX_THREADS` env var overrides it when set above zero. See
/// [`thread_limit`] and [`thread_counts`].
static THREAD_LIMIT: OnceLock<usize> = OnceLock::new();

/// Caps the total threads Lore sizes its pools for; the per-pool split is
/// derived from this and the processor count (see [`thread_counts`]). Pass `0`
/// for "no limit". Must be called before the runtime is first constructed.
/// Overridden by `LORE_MAX_THREADS` when that is set above zero.
///
/// Returns `true` if applied, `false` if a limit was already set.
pub fn set_thread_limit(count: usize) -> bool {
    THREAD_LIMIT.set(count).is_ok()
}

/// The effective total thread limit: `LORE_MAX_THREADS` if set above zero,
/// otherwise the value from [`set_thread_limit`]. Returns `None` for "no limit".
fn thread_limit() -> Option<usize> {
    env_thread_override("LORE_MAX_THREADS")
        .or_else(|| THREAD_LIMIT.get().copied())
        .filter(|&limit| limit > 0)
}

/// Per-pool thread counts Lore sizes its runtime for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadCounts {
    /// Tokio async worker threads.
    pub worker: usize,
    /// Tokio blocking (`spawn_blocking`) threads.
    pub blocking: usize,
    /// Rayon compute-pool threads (compression, hashing, …).
    pub compute: usize,
}

impl ThreadCounts {
    /// Total threads across all three pools.
    pub fn total(&self) -> usize {
        self.worker + self.blocking + self.compute
    }
}

/// Minimum threads per pool, even under a tight limit — a starved pool can
/// deadlock work another pool blocks on (e.g. compute awaited by workers). Takes
/// precedence over the limit, so the smallest achievable total is `3 * MIN`.
const MIN_THREADS_PER_POOL: usize = 2;

/// Lore's unconstrained per-pool counts, used when no thread limit is set.
fn default_thread_counts(cores: usize) -> ThreadCounts {
    ThreadCounts {
        worker: cores.max(MIN_THREADS_PER_POOL),
        blocking: std::cmp::min(2 * (cores + 1), 128).max(MIN_THREADS_PER_POOL),
        compute: cores.saturating_sub(1).max(MIN_THREADS_PER_POOL),
    }
}

/// Scales the per-pool defaults down to fit a total `limit`, preserving their
/// relative shape via the largest-remainder method. Each pool keeps at least
/// [`MIN_THREADS_PER_POOL`], so a `limit` below `3 * MIN_THREADS_PER_POOL`
/// floors there. Defaults that already fit are returned unchanged.
fn apportion_thread_counts(defaults: ThreadCounts, limit: usize) -> ThreadCounts {
    let total = defaults.total();
    if total <= limit {
        return defaults;
    }

    let ideal = [defaults.worker, defaults.blocking, defaults.compute];
    let mut alloc = [0usize; 3];
    let mut remainder = [0usize; 3];
    for i in 0..3 {
        let scaled = ideal[i] * limit;
        alloc[i] = std::cmp::max(scaled / total, MIN_THREADS_PER_POOL);
        remainder[i] = scaled % total;
    }

    let mut sum: usize = alloc.iter().sum();
    while sum > limit {
        let Some(i) = (0..3)
            .filter(|&i| alloc[i] > MIN_THREADS_PER_POOL)
            .max_by_key(|&i| alloc[i])
        else {
            break;
        };
        alloc[i] -= 1;
        sum -= 1;
    }
    while sum < limit {
        let i = (0..3).max_by_key(|&i| remainder[i]).unwrap();
        if remainder[i] == 0 {
            break;
        }
        alloc[i] += 1;
        remainder[i] = 0;
        sum += 1;
    }

    ThreadCounts {
        worker: alloc[0],
        blocking: alloc[1],
        compute: alloc[2],
    }
}

/// Reads a positive-integer per-pool thread override from `var`, returning
/// `None` when unset, unparsable or zero.
fn env_thread_override(var: &str) -> Option<usize> {
    std::env::var(var)
        .ok()
        .and_then(|val| str::parse::<usize>(val.as_str()).ok())
        .filter(|&val| val > 0)
}

/// Per-pool counts from the core count and optional limit, before env overrides.
fn budget_thread_counts() -> ThreadCounts {
    let defaults = default_thread_counts(processor_count());
    match thread_limit() {
        Some(limit) => apportion_thread_counts(defaults, limit),
        None => defaults,
    }
}

/// Per-pool thread counts Lore sizes its runtime for: the budget-derived counts
/// with per-pool env overrides applied. The `LORE_*_THREADS` overrides are
/// absolute and bypass the limit, so they can push the total back above it.
pub fn thread_counts() -> ThreadCounts {
    ThreadCounts {
        worker: default_worker_threads(),
        blocking: default_blocking_threads(),
        compute: compute_pool_thread_count(),
    }
}

/// Blocking threads for the tokio runtime: `LORE_BLOCKING_THREADS` if set,
/// else the blocking share of the budget (see [`thread_counts`]).
pub fn default_blocking_threads() -> usize {
    env_thread_override("LORE_BLOCKING_THREADS").unwrap_or_else(|| budget_thread_counts().blocking)
}

fn default_thread_keep_alive() -> u64 {
    DEFAULT_THREAD_KEEP_ALIVE_SECONDS
}

/// Configuration for the tokio runtime.
///
/// Controls the number of blocking threads, thread keep-alive duration,
/// and optionally the number of worker threads.
#[derive(Clone, Debug, Deserialize)]
pub struct TokioSettings {
    #[serde(default = "default_blocking_threads")]
    pub max_blocking_threads: usize,
    #[serde(default = "default_thread_keep_alive")]
    pub thread_keep_alive_seconds: u64,
    pub worker_threads: Option<usize>,
}

impl Default for TokioSettings {
    fn default() -> Self {
        Self {
            max_blocking_threads: default_blocking_threads(),
            thread_keep_alive_seconds: default_thread_keep_alive(),
            worker_threads: None,
        }
    }
}

/// Returns a handle to the shared tokio runtime, creating it lazily with default settings.
pub fn runtime() -> Handle {
    runtime_with_settings(None)
}

/// Returns a handle to the shared tokio runtime.
///
/// If no runtime exists yet, creates one with the provided settings (or defaults if `None`).
/// If a tokio runtime is already active on the current thread, returns its handle instead.
/// Respects the `LORE_WORKER_THREADS` environment variable for overriding worker thread count.
pub fn runtime_with_settings(settings: Option<TokioSettings>) -> Handle {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle
    } else {
        let mut default_runtime = DEFAULT_RUNTIME.lock();
        if let Some(runtime) = default_runtime.as_ref() {
            runtime.handle().clone()
        } else {
            let settings = settings.unwrap_or_default();
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder
                .enable_all()
                .max_blocking_threads(settings.max_blocking_threads)
                .thread_keep_alive(Duration::from_secs(settings.thread_keep_alive_seconds))
                .thread_name_fn(|| {
                    static ID: AtomicUsize = AtomicUsize::new(0);
                    format!("lore-tokio-{}", ID.fetch_add(1, Ordering::Relaxed))
                });
            // Always set an explicit count, else tokio would default to the raw
            // core count and ignore the thread limit. Precedence: env override,
            // explicit setting, budget-derived default.
            let worker_threads = match (
                env_thread_override("LORE_WORKER_THREADS"),
                settings.worker_threads,
            ) {
                (Some(val), _) => val,
                (None, Some(val)) if val > 0 => val,
                _ => default_worker_threads(),
            };
            builder.worker_threads(worker_threads);
            let runtime = builder.build().expect("Failed to create runtime");
            let handle = runtime.handle().clone();
            *default_runtime = Some(runtime);

            // Build the compute pool off-thread so runtime creation isn't
            // blocked on spawning N rayon workers. No LORE_CONTEXT is active
            // yet, so Handle::spawn directly rather than lore_spawn!.
            #[allow(clippy::disallowed_methods)]
            handle.spawn(async {
                let _ = COMPUTE_POOL.get_or_init(build_compute_pool);
            });

            handle
        }
    }
}

/// Threads for the shared compute pool: `LORE_COMPUTE_THREADS` if set, else the
/// compute share of the budget (see [`thread_counts`]). Exposed so callers that
/// size per-worker data structures can use the same bound the pool uses.
pub fn compute_pool_thread_count() -> usize {
    env_thread_override("LORE_COMPUTE_THREADS").unwrap_or_else(|| budget_thread_counts().compute)
}

/// Tokio worker threads: `LORE_WORKER_THREADS` if set, else the worker share of
/// the budget (see [`thread_counts`]).
pub fn default_worker_threads() -> usize {
    env_thread_override("LORE_WORKER_THREADS").unwrap_or_else(|| budget_thread_counts().worker)
}

/// Returns a reference to the shared compute thread pool. The pool is
/// eagerly built by [`runtime_with_settings`] and on first access here if
/// the runtime has not been constructed yet. Access is lock-free after
/// initialization. Use for CPU-bound work (compression, hashing, etc).
pub fn compute_pool() -> &'static rayon::ThreadPool {
    COMPUTE_POOL.get_or_init(build_compute_pool)
}

/// Guarded task set — tasks added here are awaited during `runtime_flush_guarded()`
/// and `runtime_shutdown_timeout()`. Public so that higher-level crates (e.g. `urc-core`)
/// can spawn guarded tasks with their own context-scoping logic.
pub static RUNTIME_GUARD: OnceLock<Mutex<JoinSet<()>>> = OnceLock::new();

/// Spawns a future that must complete before runtime shutdown.
///
/// The spawned task is tracked in a guarded set and will be awaited
/// during `runtime_flush_guarded()` or `runtime_shutdown_timeout()`.
pub fn runtime_spawn_guarded<T>(task: T)
where
    T: Future<Output = ()> + Send + 'static,
{
    let mut tasks = RUNTIME_GUARD
        .get_or_init(|| Mutex::new(JoinSet::new()))
        .lock();
    while tasks.try_join_next().is_some() {}
    // Internal runtime plumbing — LORE_CONTEXT is intentionally not captured
    // here; callers that want it propagated must use `lore_spawn_guarded!`.
    #[allow(clippy::disallowed_methods)]
    tasks.spawn_on(task, &runtime());
}

/// Awaits all guarded tasks to completion.
pub async fn runtime_flush_guarded() {
    if let Some(tasks) = RUNTIME_GUARD.get() {
        let mut tasks = {
            let mut lock = tasks.lock();
            std::mem::take(&mut *lock)
        };
        while tasks.join_next().await.is_some() {}
    }
}

/// Gracefully shuts down the tokio runtime: flushes guarded tasks, then shuts
/// down tokio with a timeout.
pub fn runtime_shutdown_timeout(wait_timeout: Duration) {
    let mut default_runtime = DEFAULT_RUNTIME.lock();
    if let Some(runtime) = default_runtime.take() {
        runtime.block_on(runtime_flush_guarded());
        runtime.shutdown_timeout(wait_timeout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_returns_valid_handle() {
        let handle = runtime();
        handle.block_on(async {
            tokio::task::yield_now().await;
        });
    }

    #[test]
    fn runtime_with_settings_returns_valid_handle() {
        let settings = TokioSettings {
            max_blocking_threads: 4,
            thread_keep_alive_seconds: 5,
            worker_threads: Some(2),
        };
        let handle = runtime_with_settings(Some(settings));
        handle.block_on(async {
            tokio::task::yield_now().await;
        });
    }

    #[test]
    fn default_thread_counts_match_historic_formulas() {
        let counts = default_thread_counts(8);
        assert_eq!(counts.worker, 8);
        assert_eq!(counts.blocking, std::cmp::min(2 * (8 + 1), 128));
        assert_eq!(counts.compute, 7);
    }

    #[test]
    fn apportion_returns_defaults_when_within_limit() {
        let defaults = default_thread_counts(8);
        let total = defaults.total();
        assert_eq!(apportion_thread_counts(defaults, total), defaults);
        assert_eq!(apportion_thread_counts(defaults, total + 100), defaults);
    }

    #[test]
    fn apportion_fills_budget_exactly_above_the_floor() {
        let defaults = default_thread_counts(64);
        for limit in (3 * MIN_THREADS_PER_POOL)..=defaults.total() {
            let counts = apportion_thread_counts(defaults, limit);
            assert_eq!(counts.total(), limit, "limit {limit} not used exactly");
            assert!(counts.worker >= MIN_THREADS_PER_POOL);
            assert!(counts.blocking >= MIN_THREADS_PER_POOL);
            assert!(counts.compute >= MIN_THREADS_PER_POOL);
        }
    }

    #[test]
    fn apportion_floors_below_three_times_min() {
        let counts = apportion_thread_counts(default_thread_counts(64), 1);
        assert_eq!(counts.worker, MIN_THREADS_PER_POOL);
        assert_eq!(counts.blocking, MIN_THREADS_PER_POOL);
        assert_eq!(counts.compute, MIN_THREADS_PER_POOL);
    }

    #[test]
    fn apportion_at_limit_64_on_64_core_host() {
        let defaults = default_thread_counts(64);
        assert_eq!(defaults.total(), 255);
        let counts = apportion_thread_counts(defaults, 64);
        assert_eq!(counts.worker, 16);
        assert_eq!(counts.blocking, 32);
        assert_eq!(counts.compute, 16);
    }

    #[tokio::test]
    async fn guarded_task_completes() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        runtime_spawn_guarded(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            completed_clone.store(true, Ordering::Release);
        });

        runtime_flush_guarded().await;
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn compute_pool_runs_work() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);
        compute_pool().spawn(move || {
            done_clone.store(true, Ordering::Release);
        });

        // Spin briefly; in CI the spawn + execute is sub-millisecond.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "compute_pool task did not run"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
