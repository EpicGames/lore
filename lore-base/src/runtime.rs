// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicPtr;
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
    // Trailing-comma forms, so a multi-line call formats like any other.
    ($joinset:ident, $name:literal, $expression:expr,) => {
        $crate::lore_spawn!($joinset, $name, $expression)
    };
    ($joinset:ident, $expression:expr,) => {
        $crate::lore_spawn!($joinset, $expression)
    };
    ($name_lit:literal, $expression:expr,) => {
        $crate::lore_spawn!($name_lit, $expression)
    };
    ($expression:expr,) => {
        $crate::lore_spawn!($expression)
    };
}

/// Spawns a task on the dedicated network runtime with `LORE_CONTEXT`
/// propagated — same variants and semantics as [`lore_spawn!`], different
/// runtime. Use for quinn/tonic construction, transport loops and stream
/// multiplexers; never for compute or file I/O.
#[macro_export]
macro_rules! lore_spawn_net {
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $joinset.spawn_on(
                    $crate::runtime::LORE_CONTEXT.scope(__ctx, __task),
                    &$crate::runtime::net_runtime(),
                )
            } else {
                $joinset.spawn_on(__task, &$crate::runtime::net_runtime())
            }
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::net_runtime()
                    .spawn($crate::runtime::LORE_CONTEXT.scope(__ctx, __task))
            } else {
                $crate::runtime::net_runtime().spawn(__task)
            }
        }
    }};
    // Trailing-comma forms, so a multi-line call formats like any other.
    ($joinset:ident, $expression:expr,) => {
        $crate::lore_spawn_net!($joinset, $expression)
    };
    ($expression:expr,) => {
        $crate::lore_spawn_net!($expression)
    };
}

/// Spawns a task on the network runtime **without** propagating `LORE_CONTEXT`.
///
/// For transport tasks that outlive the command which happens to create them. A `LORE_CONTEXT`
/// belongs to one command, so a connection-lifetime task that captured one would go on
/// reporting that command's correlation id while serving every later command — misattribution
/// rather than attribution. Use [`lore_spawn_net!`] for anything scoped to the command that
/// spawns it.
#[macro_export]
macro_rules! lore_spawn_net_nocontext {
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::net_runtime().spawn($crate::runtime::ObservedTask::new($expression))
        }
    }};
    ($expression:expr,) => {
        $crate::lore_spawn_net_nocontext!($expression)
    };
}

/// Spawns a task on the core runtime with `LORE_CONTEXT` propagated — same
/// variants and semantics as [`lore_spawn!`], but pinned rather than following
/// the current runtime.
///
/// Use this at a transport-to-handler boundary: a task spawned with
/// [`lore_spawn!`] from code already running on net stays on net, and so does
/// everything it spawns in turn, which would put compute and file I/O on net's
/// single blocking thread. Inside handler code reached through such a boundary,
/// plain [`lore_spawn!`] is correct — it inherits core from its caller.
#[macro_export]
macro_rules! lore_spawn_core {
    ($joinset:ident, $name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $joinset.spawn_on(
                    $crate::runtime::LORE_CONTEXT.scope(__ctx, __task),
                    &$crate::runtime::core_runtime(),
                )
            } else {
                $joinset.spawn_on(__task, &$crate::runtime::core_runtime())
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
                    &$crate::runtime::core_runtime(),
                )
            } else {
                $joinset.spawn_on(__task, &$crate::runtime::core_runtime())
            }
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::core_runtime()
                    .spawn($crate::runtime::LORE_CONTEXT.scope(__ctx, __task))
            } else {
                $crate::runtime::core_runtime().spawn(__task)
            }
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            let __task = $crate::runtime::ObservedTask::new($expression);
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::core_runtime()
                    .spawn($crate::runtime::LORE_CONTEXT.scope(__ctx, __task))
            } else {
                $crate::runtime::core_runtime().spawn(__task)
            }
        }
    }};
    // Trailing-comma forms, so a multi-line call formats like any other.
    ($joinset:ident, $name:literal, $expression:expr,) => {
        $crate::lore_spawn_core!($joinset, $name, $expression)
    };
    ($joinset:ident, $expression:expr,) => {
        $crate::lore_spawn_core!($joinset, $expression)
    };
    ($name_lit:literal, $expression:expr,) => {
        $crate::lore_spawn_core!($name_lit, $expression)
    };
    ($expression:expr,) => {
        $crate::lore_spawn_core!($expression)
    };
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
                    &$crate::runtime::core_runtime(),
                )
            } else {
                $joinset.spawn_blocking_on($expression, &$crate::runtime::core_runtime())
            }
        }
    }};
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $joinset.spawn_blocking_on(
                    move || $crate::runtime::LORE_CONTEXT.sync_scope(__ctx, $expression),
                    &$crate::runtime::core_runtime(),
                )
            } else {
                $joinset.spawn_blocking_on($expression, &$crate::runtime::core_runtime())
            }
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::core_runtime().spawn_blocking(move || {
                    $crate::runtime::LORE_CONTEXT.sync_scope(__ctx, $expression)
                })
            } else {
                $crate::runtime::core_runtime().spawn_blocking($expression)
            }
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            if let Some(__ctx) = $crate::runtime::try_lore_context() {
                $crate::runtime::core_runtime().spawn_blocking(move || {
                    $crate::runtime::LORE_CONTEXT.sync_scope(__ctx, $expression)
                })
            } else {
                $crate::runtime::core_runtime().spawn_blocking($expression)
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

/// A process-wide tokio runtime, built once on first use.
///
/// The handle is cached because the pinned spawn macros read it on request
/// paths — `lore_spawn_core!` once per inbound gRPC and HTTP request,
/// `lore_spawn_net!` once per QUIC stream and once per AWS SDK request — so
/// resolving a runtime is a load rather than a process-global lock.
struct SharedRuntime {
    handle: Handle,
    /// The runtime, owned through a pointer because `shutdown_timeout`
    /// consumes it and a `&'static Self` cannot give up ownership.
    /// [`SharedRuntime::take`] swaps it out for shutdown.
    runtime: AtomicPtr<tokio::runtime::Runtime>,
}

impl SharedRuntime {
    fn new(runtime: tokio::runtime::Runtime) -> Self {
        Self {
            handle: runtime.handle().clone(),
            runtime: AtomicPtr::new(Box::into_raw(Box::new(runtime))),
        }
    }

    /// Claims the runtime for shutdown, leaving the cached handle behind.
    /// Returns `None` once any caller has claimed it.
    fn take(&self) -> Option<tokio::runtime::Runtime> {
        let runtime = self.runtime.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if runtime.is_null() {
            return None;
        }
        // SAFETY: The pointer comes from `Box::into_raw` in `new`, and the swap
        // hands it to exactly one caller, so this reclaims the box once.
        Some(*unsafe { Box::from_raw(runtime) })
    }
}

/// Core runtime — see [`core_runtime`].
static CORE_RUNTIME: OnceLock<SharedRuntime> = OnceLock::new();

/// Network runtime — see [`net_runtime`].
static NET_RUNTIME: OnceLock<SharedRuntime> = OnceLock::new();

/// Process-configured net-runtime default thread count (the server sets one
/// per processor at startup; unset means the client default).
static NET_THREADS_DEFAULT: OnceLock<usize> = OnceLock::new();

/// Net runtime worker threads when nothing configures it: one user's worth
/// of QUIC streams and gRPC channels.
const DEFAULT_NET_THREADS: usize = 2;

/// Requests a process-default net-runtime thread count — the server asks for one
/// per processor, since serving thousands of concurrent client connections is its
/// normal case. A thread limit still bounds the count granted. Must run before
/// the net runtime is first used; returns false if a default was already set.
pub fn set_net_threads_default(count: usize) -> bool {
    NET_THREADS_DEFAULT.set(count.max(1)).is_ok()
}

/// An explicit net-thread request: `LORE_NET_THREADS` if set, else the
/// process-configured default (e.g. the server's one-per-processor). `None`
/// leaves the net pool at [`DEFAULT_NET_THREADS`].
fn requested_net_threads() -> Option<usize> {
    env_thread_override("LORE_NET_THREADS").or_else(|| NET_THREADS_DEFAULT.get().copied())
}

/// Net runtime worker threads: the net share of the thread budget, which an
/// explicit request raises (see [`thread_counts`]).
pub fn default_net_threads() -> usize {
    budget_thread_counts().net
}

/// Handle to the dedicated network runtime, created lazily on first use.
///
/// quinn/tonic driver tasks and transport loops live here so QUIC packet
/// processing, TLS, HTTP/2 framing and protocol timers are never delayed
/// by compute or file-I/O continuations saturating the core runtime;
/// per-request futures remain waker-only and are awaited directly from
/// core tasks. No blocking work belongs on this runtime — its blocking
/// pool is pinned to a single thread so a stray `spawn_blocking` cannot
/// grow it.
///
/// Built once per process: after [`runtime_shutdown_timeout`] this keeps
/// returning the shut-down handle, so late spawns are dropped rather than
/// resurrecting a runtime with fresh threads during teardown.
pub fn net_runtime() -> Handle {
    NET_RUNTIME
        .get_or_init(|| SharedRuntime::new(build_net_runtime()))
        .handle
        .clone()
}

fn build_net_runtime() -> tokio::runtime::Runtime {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .worker_threads(default_net_threads())
        .max_blocking_threads(NET_BLOCKING_THREADS)
        .thread_keep_alive(Duration::from_secs(default_thread_keep_alive()))
        .thread_name_fn(|| {
            static ID: AtomicUsize = AtomicUsize::new(0);
            format!("lore-net-{}", ID.fetch_add(1, Ordering::Relaxed))
        });
    builder.build().expect("Failed to create net runtime")
}
static DEFAULT_THREAD_KEEP_ALIVE_SECONDS: u64 = 10;

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

/// Optional ceiling on the *sum* of the worker, blocking, and net pool sizes.
/// Set once via [`set_thread_limit`]; `0` means "no limit". The
/// `LORE_MAX_THREADS` env var overrides it when set above zero. See
/// [`thread_limit`] and [`thread_counts`].
static THREAD_LIMIT: OnceLock<usize> = OnceLock::new();

/// Caps the total threads Lore sizes its pools for; the per-pool split is
/// derived from this and the processor count (see [`thread_counts`]). Pass `0`
/// for "no limit". Must be called before the runtime is first constructed.
/// Overridden by `LORE_MAX_THREADS` when that is set above zero.
///
/// A set limit binds every pool: environment and configuration knobs raise a
/// pool's ideal, never the granted total. The one exception is
/// [`MIN_THREADS_PER_POOL`], which a pool keeps even under a smaller limit, so
/// the least this can achieve is `3 * MIN_THREADS_PER_POOL`.
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
    /// Tokio blocking (`spawn_blocking`) threads, across both runtimes: the core
    /// runtime's pool plus the net runtime's [`NET_BLOCKING_THREADS`].
    pub blocking: usize,
    /// Net runtime worker threads. [`set_net_threads_default`] and
    /// `LORE_NET_THREADS` raise this pool's ideal — the server asks for one per
    /// processor — but a thread limit still bounds what it is granted.
    pub net: usize,
    /// The lore-io syscall pool, which every file operation dispatches through.
    /// `LORE_IO_POOL_THREADS` raises its ideal.
    pub io: usize,
}

/// Pools the budget divides between.
const POOL_COUNT: usize = 4;

impl ThreadCounts {
    /// Total threads across all pools.
    pub fn total(&self) -> usize {
        self.worker + self.blocking + self.net + self.io
    }

    /// The pools in apportionment order.
    fn as_array(&self) -> [usize; POOL_COUNT] {
        [self.worker, self.blocking, self.net, self.io]
    }

    fn from_array(pools: [usize; POOL_COUNT]) -> ThreadCounts {
        ThreadCounts {
            worker: pools[0],
            blocking: pools[1],
            net: pools[2],
            io: pools[3],
        }
    }
}

/// Minimum threads per pool, even under a tight limit — a starved pool can
/// deadlock work another pool blocks on (e.g. blocking calls awaited by
/// workers). Takes precedence over the limit, so the smallest achievable
/// total is `POOL_COUNT * MIN`.
const MIN_THREADS_PER_POOL: usize = 2;

/// The core runtime's blocking pool: file I/O runs on the lore-io driver, so
/// this serves only genuinely blocking OS APIs with no async form (OS keyring
/// access, AWS SDK initialization, service IPC pipe reads). Fixed and
/// processor-count-independent by design — see the async file I/O enhancement
/// proposal's thread budget.
const CORE_BLOCKING_THREADS: usize = 2;

/// The net runtime's blocking pool, pinned to one thread so that a stray
/// `spawn_blocking` there cannot grow it and starve the protocol timers.
/// Budgeted with the core runtime's pool rather than beside it, since both
/// hold threads for the same reason.
const NET_BLOCKING_THREADS: usize = 1;

/// Lore's unconstrained per-pool counts, used when no thread limit is set. `io`
/// is the syscall pool's own request, which `lore-io` derives from the core count
/// and `LORE_IO_POOL_THREADS`.
fn default_thread_counts(cores: usize, io: usize) -> ThreadCounts {
    ThreadCounts {
        worker: cores.max(MIN_THREADS_PER_POOL),
        blocking: CORE_BLOCKING_THREADS + NET_BLOCKING_THREADS,
        net: DEFAULT_NET_THREADS.max(MIN_THREADS_PER_POOL),
        io: io.max(MIN_THREADS_PER_POOL),
    }
}

/// Scales the per-pool defaults down to fit a total `limit`, preserving their
/// relative shape via the largest-remainder method. Each pool keeps at least
/// [`MIN_THREADS_PER_POOL`], so a `limit` below `POOL_COUNT * MIN_THREADS_PER_POOL`
/// floors there. Defaults that already fit are returned unchanged.
fn apportion_thread_counts(defaults: ThreadCounts, limit: usize) -> ThreadCounts {
    let total = defaults.total();
    if total <= limit {
        return defaults;
    }

    let ideal = defaults.as_array();
    let mut alloc = [0usize; POOL_COUNT];
    let mut remainder = [0usize; POOL_COUNT];
    for i in 0..POOL_COUNT {
        let scaled = ideal[i] * limit;
        alloc[i] = std::cmp::max(scaled / total, MIN_THREADS_PER_POOL);
        remainder[i] = scaled % total;
    }

    let mut sum: usize = alloc.iter().sum();
    while sum > limit {
        let Some(i) = (0..POOL_COUNT)
            .filter(|&i| alloc[i] > MIN_THREADS_PER_POOL)
            .max_by_key(|&i| alloc[i])
        else {
            break;
        };
        alloc[i] -= 1;
        sum -= 1;
    }
    while sum < limit {
        let i = (0..POOL_COUNT).max_by_key(|&i| remainder[i]).unwrap();
        if remainder[i] == 0 {
            break;
        }
        alloc[i] += 1;
        remainder[i] = 0;
        sum += 1;
    }

    ThreadCounts::from_array(alloc)
}

/// Reads a positive-integer per-pool thread override from `var`, returning
/// `None` when unset, unparsable or zero.
fn env_thread_override(var: &str) -> Option<usize> {
    std::env::var(var)
        .ok()
        .and_then(|val| str::parse::<usize>(val.as_str()).ok())
        .filter(|&val| val > 0)
}

/// Applies the total limit to per-pool requests, returning them unchanged when
/// none is set.
///
/// Every path that sizes a pool ends here, so that a limit bounds the process
/// whatever asked for the threads. A request is an ideal, not a final count.
fn apply_thread_limit(requested: ThreadCounts) -> ThreadCounts {
    match thread_limit() {
        Some(limit) => apportion_thread_counts(requested, limit),
        None => requested,
    }
}

/// Per-pool sizes before any limit: the core-count defaults with an explicit net
/// request substituted.
fn requested_thread_counts() -> ThreadCounts {
    let mut counts = default_thread_counts(processor_count(), lore_io::requested_max_threads());
    if let Some(net) = requested_net_threads() {
        counts.net = net;
    }
    counts
}

/// Per-pool counts from the core count, an explicit net request, and the limit.
fn budget_thread_counts() -> ThreadCounts {
    apply_thread_limit(requested_thread_counts())
}

/// Per-pool thread counts Lore sizes its runtime for.
///
/// `LORE_NET_THREADS` and [`set_net_threads_default`] raise the net pool's ideal,
/// and `LORE_IO_POOL_THREADS` the syscall pool's; worker follows the core count
/// and blocking is a small fixed count. Whatever the ideals, a thread limit is a
/// ceiling on their total rather than a starting point to negotiate from: the
/// pools are scaled to fit it, down to [`MIN_THREADS_PER_POOL`] each.
pub fn thread_counts() -> ThreadCounts {
    budget_thread_counts()
}

/// Blocking threads across both runtimes: the blocking share of the budget
/// (see [`thread_counts`]).
pub fn default_blocking_threads() -> usize {
    budget_thread_counts().blocking
}

fn default_thread_keep_alive() -> u64 {
    DEFAULT_THREAD_KEEP_ALIVE_SECONDS
}

/// The blocking pool a configuration that sets no size asks for.
///
/// Deliberately the pre-limit ideal rather than [`default_blocking_threads`]:
/// the configured size and this default reach [`core_thread_counts`] as the same
/// kind of value, so the limit is applied to them exactly once.
fn requested_blocking_threads() -> usize {
    CORE_BLOCKING_THREADS
}

/// Configuration for the tokio runtime.
///
/// Controls the number of blocking threads, thread keep-alive duration,
/// and optionally the number of worker threads.
///
/// The thread counts here are requests. A thread limit scales them to fit,
/// so configuring a pool cannot raise the process above a ceiling an embedder
/// asked for.
#[derive(Clone, Debug, Deserialize)]
pub struct TokioSettings {
    #[serde(default = "requested_blocking_threads")]
    pub max_blocking_threads: usize,
    #[serde(default = "default_thread_keep_alive")]
    pub thread_keep_alive_seconds: u64,
    pub worker_threads: Option<usize>,
    /// Net runtime worker threads; `None` keeps the process default (2 on
    /// clients, one per processor where the server sets it).
    #[serde(default)]
    pub net_threads: Option<usize>,
}

impl Default for TokioSettings {
    fn default() -> Self {
        TokioSettings {
            max_blocking_threads: requested_blocking_threads(),
            thread_keep_alive_seconds: default_thread_keep_alive(),
            worker_threads: None,
            net_threads: None,
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
/// Worker count comes from `settings` when it sets one, else from the thread budget.
pub fn runtime_with_settings(settings: Option<TokioSettings>) -> Handle {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle
    } else {
        core_runtime_with_settings(settings)
    }
}

/// Handle to the shared core runtime, created lazily.
///
/// Unlike [`runtime`] this never substitutes the caller's current runtime. Blocking work
/// must land on core's pool wherever it is issued from: the net runtime is built with
/// `max_blocking_threads(1)`, so one long blocking call dispatched there starves every
/// later net-side blocking call, and with it the QUIC and HTTP/2 timers.
///
/// Built once per process, with the same post-shutdown behaviour as
/// [`net_runtime`].
pub fn core_runtime() -> Handle {
    core_runtime_with_settings(None)
}

fn core_runtime_with_settings(settings: Option<TokioSettings>) -> Handle {
    CORE_RUNTIME
        .get_or_init(|| SharedRuntime::new(build_core_runtime(settings)))
        .handle
        .clone()
}

/// Warns once for a retired per-pool thread variable that is still set.
///
/// Removing the knob silently would lose whatever tuning a caller had configured without telling
/// them; `LORE_MAX_THREADS` is what replaces it.
fn warn_retired_thread_vars() {
    for var in [
        "LORE_WORKER_THREADS",
        "LORE_BLOCKING_THREADS",
        "LORE_COMPUTE_THREADS",
    ] {
        if std::env::var_os(var).is_some() {
            crate::lore_warn!("{var} is no longer used, size the runtime with LORE_MAX_THREADS");
        }
    }
}

fn build_core_runtime(settings: Option<TokioSettings>) -> tokio::runtime::Runtime {
    warn_retired_thread_vars();
    let settings = settings.unwrap_or_default();
    if let Some(net_threads) = settings.net_threads.filter(|&count| count > 0) {
        let _ = NET_THREADS_DEFAULT.set(net_threads);
    }
    let counts = core_thread_counts(&settings);
    // The syscall pool builds itself on the first file operation, which in every Lore process
    // happens on this runtime and so after this point.
    lore_io::set_max_threads(counts.io);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .max_blocking_threads(core_blocking_threads(counts))
        .thread_keep_alive(Duration::from_secs(settings.thread_keep_alive_seconds))
        .thread_name_fn(|| {
            static ID: AtomicUsize = AtomicUsize::new(0);
            format!("lore-tokio-{}", ID.fetch_add(1, Ordering::Relaxed))
        });
    // Tokio would otherwise default to the raw core count and ignore the thread limit.
    builder.worker_threads(counts.worker);
    builder.build().expect("Failed to create runtime")
}

/// The counts the core runtime is built with: the configured sizes over the
/// defaults, then scaled to fit any thread limit.
///
/// Configuration reaches the pools through here rather than the builder directly,
/// so a configured pool cannot push the process past a limit either. A configured
/// `max_blocking_threads` sizes the core runtime's own pool, so the net runtime's
/// thread is added to reach the budgeted total.
fn core_thread_counts(settings: &TokioSettings) -> ThreadCounts {
    let mut requested = requested_thread_counts();
    if let Some(worker) = settings.worker_threads.filter(|&count| count > 0) {
        requested.worker = worker;
    }
    if settings.max_blocking_threads > 0 {
        requested.blocking = settings.max_blocking_threads + NET_BLOCKING_THREADS;
    }
    apply_thread_limit(requested)
}

/// The core runtime's blocking pool: the budgeted blocking threads less the one
/// the net runtime holds, and never zero — a pool of none runs nothing.
fn core_blocking_threads(counts: ThreadCounts) -> usize {
    counts.blocking.saturating_sub(NET_BLOCKING_THREADS).max(1)
}

/// Tokio worker threads: the worker share of the budget (see [`thread_counts`]). Exposed so
/// callers that size per-worker data structures (e.g. compression scratch pools) can use the same
/// bound the runtime uses.
///
/// There is no per-pool environment override. `LORE_MAX_THREADS` is the one knob, and a var that
/// bypassed it could raise the total above the ceiling an embedder asked for. A configuration that
/// sets `worker_threads` moves the runtime alone, since this is read before any is supplied.
pub fn default_worker_threads() -> usize {
    budget_thread_counts().worker
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

/// Drives `future` to completion from a synchronous caller, wherever that caller
/// runs, and gives up after `wait_timeout` instead of hanging. Returns whether it
/// completed.
///
/// Shutdown paths need this: their signatures are synchronous — FFI entry points,
/// `Drop` — but the work they must finish before the runtime goes away is async.
/// Three contexts are possible and each needs different handling:
///
/// 1. **No runtime on the calling thread**, the usual FFI entry from C: drive it
///    on core directly.
/// 2. **A multi-thread runtime is current:** `block_in_place` hands this worker
///    over while the runtime keeps its other workers running, so tasks the future
///    depends on still progress.
/// 3. **A `current_thread` runtime is current** (`#[tokio::test]`, embedders):
///    there is no way to block this thread *and* let this runtime run, because
///    this thread **is** the runtime. The future is driven on core from a separate
///    thread, which covers everything except what the caller's own runtime would
///    have had to drive — and that is why the timeout is not optional here.
///
/// The distinction is the whole point: `block_in_place` panics on a
/// `current_thread` runtime, and `Handle::block_on` cannot drive a
/// `current_thread` runtime's I/O or timers from a foreign thread, so bouncing
/// the future onto the caller's own handle hangs.
pub fn shutdown_block_on<F>(future: F, wait_timeout: Duration) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    let bounded = async move { tokio::time::timeout(wait_timeout, future).await.is_ok() };

    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            // The caller is shutting the process down, so a core it hands off has nothing left to
            // run, and `wait_timeout` bounds how long the handoff can last either way.
            #[allow(clippy::disallowed_methods)]
            tokio::task::block_in_place(move || handle.block_on(bounded))
        }
        Ok(_) => {
            let core = core_runtime();
            let worker = std::thread::Builder::new()
                .name("lore-shutdown-wait".to_string())
                .spawn(move || core.block_on(bounded))
                .expect("Failed to spawn shutdown wait thread");
            match worker.join() {
                Ok(completed) => completed,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
        Err(_) => core_runtime().block_on(bounded),
    }
}

/// Gracefully shuts down the tokio runtimes: flushes guarded tasks, then shuts
/// down tokio with a timeout.
///
/// Terminal for the process — neither runtime is rebuilt afterwards, and the
/// accessors keep handing out the shut-down handles. Concurrent callers past
/// this point are not blocked by the shutdown; their spawns are dropped.
///
/// The work runs on a dedicated thread rather than the caller's, because
/// `Runtime::block_on` and `Runtime::shutdown_timeout` both panic inside an
/// async context and the C `lore_shutdown()` can be called from one. The two tokio
/// shutdowns are bounded by `wait_timeout`; the guarded flush is not, because those
/// tasks are the work that has to finish before the runtime goes away. A guarded
/// task that never completes therefore holds shutdown open.
/// Calling this from a task *on* one of these runtimes is still a poor idea:
/// it cannot panic any more, but that runtime cannot finish shutting down
/// while the caller is parked in it, so it costs the full timeout.
pub fn runtime_shutdown_timeout(wait_timeout: Duration) {
    let core = CORE_RUNTIME.get().and_then(SharedRuntime::take);
    let net = NET_RUNTIME.get().and_then(SharedRuntime::take);
    if core.is_none() && net.is_none() {
        return;
    }

    let shutdown = std::thread::Builder::new()
        .name("lore-shutdown".to_string())
        .spawn(move || {
            if let Some(runtime) = core {
                // Unbounded on purpose: guarded tasks are the work that must finish before the
                // runtime goes away, so cutting the flush short abandons it. The timeouts below
                // bound the shutdown itself.
                runtime.block_on(runtime_flush_guarded());
                runtime.shutdown_timeout(wait_timeout);
            }
            // The net runtime goes second: guarded core tasks may still be
            // flushing writes over the network.
            if let Some(runtime) = net {
                runtime.shutdown_timeout(wait_timeout);
            }
        })
        .expect("Failed to spawn runtime shutdown thread");

    if let Err(panic) = shutdown.join() {
        std::panic::resume_unwind(panic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The net runtime has `max_blocking_threads(1)`, so blocking work dispatched there
    /// starves every later net-side blocking call. Thread names distinguish the pools:
    /// core spawns `lore-tokio-*`, net spawns `lore-net-*`.
    #[test]
    fn blocking_macros_target_core_even_from_a_net_task() {
        let thread_name = net_runtime().block_on(async {
            crate::lore_spawn_net!(async {
                crate::lore_spawn_blocking!(|| std::thread::current()
                    .name()
                    .unwrap_or_default()
                    .to_string())
                .await
                .expect("blocking task joins")
            })
            .await
            .expect("net task joins")
        });

        assert!(
            thread_name.starts_with("lore-tokio-"),
            "blocking work issued from a net task ran on {thread_name:?}, \
             which is not a core blocking thread"
        );
    }

    /// The accessors sit on request paths, so they must resolve to the one
    /// runtime built for the process — never rebuild, never serialise.
    #[test]
    fn accessors_resolve_to_one_runtime_per_process() {
        let expected = (core_runtime().id(), net_runtime().id());
        assert_ne!(expected.0, expected.1, "core and net share a runtime");

        let threads: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (core_runtime().id(), net_runtime().id())))
            .collect();
        for thread in threads {
            assert_eq!(thread.join().expect("thread joins"), expected);
        }
    }

    /// Driving the work on a `current_thread` caller's own handle from a foreign thread
    /// cannot work — `Handle::block_on` does not drive a `current_thread` runtime's tasks,
    /// and the thread that would have is parked waiting for this one — so it hangs. The
    /// spawned task completing is the proof that the work runs on core instead: it is what
    /// the storage close does.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_block_on_runs_spawns_from_a_current_thread_caller() {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&ran);

        let completed = shutdown_block_on(
            async move {
                crate::lore_spawn!(async move { flag.store(true, Ordering::SeqCst) })
                    .await
                    .expect("spawned task joins");
            },
            Duration::from_secs(10),
        );

        assert!(completed, "shutdown work timed out instead of completing");
        assert!(
            ran.load(Ordering::SeqCst),
            "the task the shutdown work spawned never ran"
        );
    }

    /// The same from a multi-thread caller, which takes the `block_in_place` path — the one
    /// that panics outright on a `current_thread` runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_block_on_runs_spawns_from_a_multi_thread_caller() {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&ran);

        let completed = shutdown_block_on(
            async move {
                crate::lore_spawn!(async move { flag.store(true, Ordering::SeqCst) })
                    .await
                    .expect("spawned task joins");
            },
            Duration::from_secs(10),
        );

        assert!(completed, "shutdown work timed out instead of completing");
        assert!(ran.load(Ordering::SeqCst), "the spawned task never ran");
    }

    #[test]
    fn shutdown_block_on_runs_without_a_runtime() {
        let completed = shutdown_block_on(
            async {
                tokio::task::yield_now().await;
            },
            Duration::from_secs(10),
        );

        assert!(completed, "shutdown work timed out instead of completing");
    }

    /// Bounded, not best-effort: a `current_thread` caller may hold the only thread that
    /// could drive part of the work, so shutdown has to be able to give up on it.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_block_on_gives_up_instead_of_hanging() {
        let completed = shutdown_block_on(std::future::pending::<()>(), Duration::from_millis(50));

        assert!(
            !completed,
            "a future that cannot finish must report timeout"
        );
    }

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
            net_threads: None,
        };
        let handle = runtime_with_settings(Some(settings));
        handle.block_on(async {
            tokio::task::yield_now().await;
        });
    }

    /// The syscall pool's request on a host with this many cores, so the cases
    /// below do not depend on the machine running them.
    const IO_REQUEST: usize = 16;

    #[test]
    fn default_thread_counts_match_design_formulas() {
        let counts = default_thread_counts(8, IO_REQUEST);
        assert_eq!(counts.worker, 8);
        assert_eq!(
            counts.blocking,
            CORE_BLOCKING_THREADS + NET_BLOCKING_THREADS
        );
        assert_eq!(counts.net, DEFAULT_NET_THREADS);
        assert_eq!(counts.io, IO_REQUEST);
    }

    #[test]
    fn apportion_returns_defaults_when_within_limit() {
        let defaults = default_thread_counts(8, IO_REQUEST);
        let total = defaults.total();
        assert_eq!(apportion_thread_counts(defaults, total), defaults);
        assert_eq!(apportion_thread_counts(defaults, total + 100), defaults);
    }

    #[test]
    fn apportion_fills_budget_exactly_above_the_floor() {
        let defaults = default_thread_counts(64, IO_REQUEST);
        for limit in (POOL_COUNT * MIN_THREADS_PER_POOL)..=defaults.total() {
            let counts = apportion_thread_counts(defaults, limit);
            assert_eq!(counts.total(), limit, "limit {limit} not used exactly");
            for pool in counts.as_array() {
                assert!(pool >= MIN_THREADS_PER_POOL);
            }
        }
    }

    #[test]
    fn apportion_floors_below_min_total() {
        let counts = apportion_thread_counts(default_thread_counts(64, IO_REQUEST), 1);
        assert_eq!(counts.as_array(), [MIN_THREADS_PER_POOL; POOL_COUNT]);
    }

    #[test]
    fn apportion_at_limit_64_on_64_core_host() {
        let defaults = default_thread_counts(64, IO_REQUEST);
        assert_eq!(defaults.total(), 85);
        let counts = apportion_thread_counts(defaults, 64);
        assert_eq!(counts.worker, 48);
        assert_eq!(counts.blocking, MIN_THREADS_PER_POOL);
        assert_eq!(counts.net, MIN_THREADS_PER_POOL);
        assert_eq!(counts.io, 12);
        assert_eq!(counts.total(), 64);
    }

    /// A net request raises the pool's ideal, and the limit still binds the total —
    /// asking for 64 net threads under a 64-thread ceiling cannot buy 64 of them
    /// on top of the worker and blocking pools.
    #[test]
    fn a_net_request_is_scaled_by_the_limit() {
        let requested = ThreadCounts {
            worker: 20,
            blocking: CORE_BLOCKING_THREADS + NET_BLOCKING_THREADS,
            net: 64,
            io: IO_REQUEST,
        };
        let counts = apportion_thread_counts(requested, 64);
        assert_eq!(counts.total(), 64);
        assert!(counts.net < requested.net);
        assert!(
            counts.net > counts.worker,
            "the largest request keeps the largest share"
        );
    }

    /// The limit is a ceiling on the total whatever the per-pool requests are, so
    /// no knob can raise the process above what an embedder asked for. The
    /// per-pool floor is a property of scaling down rather than of every result:
    /// a request that already fits is honoured as written, including one below it.
    #[test]
    fn the_limit_bounds_the_total_for_any_request() {
        for worker in [2, 8, 64, 256] {
            for blocking in [1, 2, 128] {
                for net in [1, 2, 64, 512] {
                    for io in [1, 16, 128] {
                        let requested = ThreadCounts {
                            worker,
                            blocking,
                            net,
                            io,
                        };
                        for limit in [POOL_COUNT * MIN_THREADS_PER_POOL, 16, 64, 1024] {
                            let counts = apportion_thread_counts(requested, limit);
                            assert!(
                                counts.total() <= limit,
                                "{requested:?} at limit {limit} gave {counts:?}"
                            );
                            if requested.total() <= limit {
                                assert_eq!(counts, requested, "a request that fits is untouched");
                                continue;
                            }
                            for pool in counts.as_array() {
                                assert!(
                                    pool >= MIN_THREADS_PER_POOL,
                                    "{requested:?} at limit {limit} starved a pool: {counts:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
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
}
