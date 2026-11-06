// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
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
use tokio::task::futures::TaskLocalFuture;

// ---------------------------------------------------------------------------
// Instruments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoreTaskLifecycleEvent {
    Started,
    Completed,
    Dropped,
}

/// What was true where a task was spawned, captured once as it is created.
///
/// Every event a task emits carries this same value, so an up/down counter keyed on these fields
/// pairs a task's increment with its decrement. Re-resolving a field per event would not pair
/// them: a task is created on its spawner's thread but polled and dropped on a worker's, under
/// whatever context is ambient there — which for
/// [`lore_spawn_net_nocontext!`](crate::lore_spawn_net_nocontext) is deliberately not the
/// spawner's, so the two ends would count against different series.
pub struct LoreTaskSpawn {
    pub file: &'static str,
    pub line: u32,
    /// Labels the `LORE_CONTEXT` ambient where the task was spawned.
    pub context_label: &'static str,
}

/// Observes the tasks spawned through the `lore_spawn*` macros.
pub trait TaskLifecycleObserver: Send + Sync {
    /// Labels the `LORE_CONTEXT` a task is being spawned under.
    ///
    /// Called once per task, on the spawning thread before the task reaches a runtime, so the
    /// ambient context is still the spawning caller's. The label is stored on the task and handed
    /// back to [`Self::on_event`] for every event it goes on to emit.
    fn context_label(&self) -> &'static str;

    /// Reports one event in a task's life, carrying the facts captured at its spawn.
    fn on_event(&self, event: LoreTaskLifecycleEvent, spawn: &LoreTaskSpawn);
}

static RUNTIME_TASK_OBSERVER: OnceLock<Box<dyn TaskLifecycleObserver>> = OnceLock::new();

/// Installs the process's task lifecycle observer, reporting whether it took.
///
/// Tasks already in flight stay unobserved rather than reporting only their end, so an observer
/// counting tasks never sees a decrement whose increment it missed.
pub fn set_task_lifecycle_observer(observer: Box<dyn TaskLifecycleObserver>) -> bool {
    RUNTIME_TASK_OBSERVER.set(observer).is_ok()
}

fn report_task_event(spawn: Option<&LoreTaskSpawn>, event: LoreTaskLifecycleEvent) {
    if let Some(spawn) = spawn
        && let Some(observer) = RUNTIME_TASK_OBSERVER.get()
    {
        observer.on_event(event, spawn);
    }
}

#[pin_project(PinnedDrop)]
pub struct ObservedTask<F> {
    #[pin]
    inner: F,
    /// `None` when no observer was installed as the task was created, which keeps such a task
    /// silent for its whole life rather than reporting an end with no matching start.
    spawn: Option<LoreTaskSpawn>,
    ran_to_completion: bool,
}

impl<F> ObservedTask<F> {
    /// Wraps a future so its lifecycle reaches the installed [`TaskLifecycleObserver`].
    ///
    /// Inert if no observer has been installed yet.
    #[track_caller]
    pub fn new(inner: F) -> Self {
        let caller = ::std::panic::Location::caller();

        let spawn = RUNTIME_TASK_OBSERVER.get().map(|observer| {
            let spawn = LoreTaskSpawn {
                file: caller.file(),
                line: caller.line(),
                context_label: observer.context_label(),
            };
            observer.on_event(LoreTaskLifecycleEvent::Started, &spawn);

            spawn
        });

        Self {
            inner,
            spawn,
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
            report_task_event(this.spawn.as_ref(), LoreTaskLifecycleEvent::Completed);
        }
        result
    }
}

#[pinned_drop]
impl<F> PinnedDrop for ObservedTask<F> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if !*this.ran_to_completion {
            report_task_event(this.spawn.as_ref(), LoreTaskLifecycleEvent::Dropped);
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

/// A spawned task scoped to the `LORE_CONTEXT` it was spawned under, or unscoped if there was none.
/// Both cases are one type, so the runtime's task machinery is compiled once for each spawned
/// future type.
#[pin_project(project = ContextTaskProjection)]
pub enum ContextTask<F> {
    Scoped(#[pin] TaskLocalFuture<Arc<dyn Any + Send + Sync>, ObservedTask<F>>),
    Unscoped(#[pin] ObservedTask<F>),
}

impl<F: Future> Future for ContextTask<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        match self.project() {
            ContextTaskProjection::Scoped(task) => task.poll(cx),
            ContextTaskProjection::Unscoped(task) => task.poll(cx),
        }
    }
}

/// `future` observed and scoped to the current `LORE_CONTEXT`, if one is set, as a [`ContextTask`].
#[track_caller]
pub fn spawned_task<F: Future>(future: F) -> ContextTask<F> {
    let task = ObservedTask::new(future);
    match try_lore_context() {
        Some(context) => ContextTask::Scoped(LORE_CONTEXT.scope(context, task)),
        None => ContextTask::Unscoped(task),
    }
}

/// `function`, to run within its caller's `LORE_CONTEXT`, if one is set. Both cases are one closure
/// type, so the runtime's blocking task machinery is compiled once for each spawned closure type.
pub fn spawned_blocking_task<F, T>(function: F) -> impl FnOnce() -> T + Send + 'static
where
    F: FnOnce() -> T + Send + 'static,
    T: 'static,
{
    let context = try_lore_context();
    move || match context {
        Some(context) => LORE_CONTEXT.sync_scope(context, function),
        None => function(),
    }
}

/// A [`JoinSet`] that detaches its tasks when dropped rather than aborting them, for tasks that
/// must not stop part way. A detached task runs to its end, and its output is dropped then.
pub struct DetachingJoinSet<T: 'static>(JoinSet<T>);

impl<T: 'static> Default for DetachingJoinSet<T> {
    fn default() -> Self {
        Self(JoinSet::new())
    }
}

impl<T: 'static> std::ops::Deref for DetachingJoinSet<T> {
    type Target = JoinSet<T>;

    fn deref(&self) -> &JoinSet<T> {
        &self.0
    }
}

impl<T: 'static> std::ops::DerefMut for DetachingJoinSet<T> {
    fn deref_mut(&mut self) -> &mut JoinSet<T> {
        &mut self.0
    }
}

impl<T: 'static> Drop for DetachingJoinSet<T> {
    fn drop(&mut self) {
        self.0.detach_all();
    }
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
            $joinset.spawn_on(
                $crate::runtime::spawned_task($expression),
                &$crate::runtime::runtime(),
            )
        }
    }};
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $joinset.spawn_on(
                $crate::runtime::spawned_task($expression),
                &$crate::runtime::runtime(),
            )
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::runtime().spawn($crate::runtime::spawned_task($expression))
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::runtime().spawn($crate::runtime::spawned_task($expression))
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
            $joinset.spawn_on(
                $crate::runtime::spawned_task($expression),
                &$crate::runtime::net_runtime(),
            )
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::net_runtime().spawn($crate::runtime::spawned_task($expression))
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
            $joinset.spawn_on(
                $crate::runtime::spawned_task($expression),
                &$crate::runtime::core_runtime(),
            )
        }
    }};
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $joinset.spawn_on(
                $crate::runtime::spawned_task($expression),
                &$crate::runtime::core_runtime(),
            )
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::core_runtime().spawn($crate::runtime::spawned_task($expression))
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::core_runtime().spawn($crate::runtime::spawned_task($expression))
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
            $joinset.spawn_blocking_on(
                $crate::runtime::spawned_blocking_task($expression),
                &$crate::runtime::core_runtime(),
            )
        }
    }};
    ($joinset:ident, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $joinset.spawn_blocking_on(
                $crate::runtime::spawned_blocking_task($expression),
                &$crate::runtime::core_runtime(),
            )
        }
    }};
    ($name:literal, $expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::core_runtime()
                .spawn_blocking($crate::runtime::spawned_blocking_task($expression))
        }
    }};
    ($expression:expr) => {{
        #[allow(clippy::disallowed_methods)]
        {
            $crate::runtime::core_runtime()
                .spawn_blocking($crate::runtime::spawned_blocking_task($expression))
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

/// Whether shutdown has begun. Set once and never cleared, because shutdown is
/// terminal for the process.
static SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);

pub fn claim_runtime_shutdown() -> bool {
    SHUTDOWN_STARTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

pub fn runtime_shutdown_started() -> bool {
    SHUTDOWN_STARTED.load(Ordering::Relaxed)
}

/// Network runtime — see [`net_runtime`].
static NET_RUNTIME: OnceLock<SharedRuntime> = OnceLock::new();

/// Process-configured net-runtime default thread count (the server sets one
/// per processor at startup; unset means the client default).
static NET_THREADS_DEFAULT: OnceLock<usize> = OnceLock::new();

/// Net runtime worker threads when nothing configures it: one user's worth
/// of QUIC streams and gRPC channels.
#[lore_macro::test_pub]
const DEFAULT_NET_THREADS: usize = 2;

/// Requests a process-default net-runtime thread count, as [`serving_runtime`] does
/// for one per processor. A thread limit still bounds the count granted. Must run before
/// either runtime is built, since both are sized from it; returns false if a
/// default was already set.
pub fn set_net_threads_default(count: usize) -> bool {
    NET_THREADS_DEFAULT.set(count.max(1)).is_ok()
}

/// The storage connections a process sizes its runtimes for — see [`threads_for_connections`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetConnections {
    /// A process that opens this many storage connections to a remote.
    Opened(usize),
    /// A process that serves commands which each open up to this many.
    Served(usize),
}

/// The storage connections the process sized its runtimes for — see
/// [`set_net_connections`]. `None` for a process that set none.
static NET_CONNECTIONS: OnceLock<Option<NetConnections>> = OnceLock::new();

/// Sizes the core and net runtimes for `connections`, as [`threads_for_connections`] divides
/// them. An explicit net request (`LORE_NET_THREADS`, the `net_threads` setting or
/// [`set_net_threads_default`]) takes precedence, and a thread limit bounds the total. A process
/// that sets none keeps one core thread per processor and [`DEFAULT_NET_THREADS`]. Returns false,
/// and changes nothing, once a runtime is built or the connections are set, so both runtimes are
/// sized for the same ones.
pub fn set_net_connections(connections: NetConnections) -> bool {
    NET_CONNECTIONS.set(Some(connections)).is_ok()
}

/// Settles the connections the runtimes are sized for, as none when none were set.
fn settle_net_connections() {
    NET_CONNECTIONS.get_or_init(|| None);
}

/// An explicit net-thread request: `LORE_NET_THREADS` if set, else the
/// process-configured default (e.g. the server's one-per-processor). `None`
/// leaves the net pool at [`DEFAULT_NET_THREADS`], or sized for the process's
/// storage connections.
fn requested_net_threads() -> Option<usize> {
    env_thread_override("LORE_NET_THREADS").or_else(|| NET_THREADS_DEFAULT.get().copied())
}

/// Net runtime worker threads: the net share of the thread budget, which an
/// explicit request or the storage connections raise (see [`thread_counts`]).
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
    settle_net_connections();
    let threads = default_net_threads();
    crate::lore_debug!("Net runtime: {threads} workers");
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .worker_threads(threads)
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

/// Returns the number of available processors, kept from the first read that succeeds; after a
/// failed read the next call reads again. On Linux each read of
/// `std::thread::available_parallelism` parses the cgroup files.
///
/// On Windows, takes the maximum of `std::thread::available_parallelism` and the Win32
/// `GetActiveProcessorCount` API, since the former underestimates on large machines.
/// On other platforms, returns `std::thread::available_parallelism` (falling back to 1).
pub fn processor_count() -> usize {
    static COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    if let Some(&count) = COUNT.get() {
        return count;
    }
    let read = std::thread::available_parallelism();
    let std_count = read.as_ref().map_or(1, |c| c.get());
    #[cfg(target_os = "windows")]
    let count = std::cmp::max(std_count, platform_processor_count());
    #[cfg(not(target_os = "windows"))]
    let count = std_count;
    if read.is_ok() {
        *COUNT.get_or_init(|| count)
    } else {
        count
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
    /// `LORE_NET_THREADS` set this pool's ideal — the server asks for one per
    /// processor — and [`set_net_connections`] raises it, but a thread limit
    /// bounds what it is granted.
    pub net: usize,
    /// The lore-io syscall pool, which every file operation dispatches through.
    /// `LORE_IO_POOL_THREADS` raises its ideal.
    pub io: usize,
}

/// Pools the budget divides between.
#[lore_macro::test_pub]
const POOL_COUNT: usize = 4;

impl ThreadCounts {
    /// Total threads across all pools.
    pub fn total(&self) -> usize {
        self.worker + self.blocking + self.net + self.io
    }

    /// The pools in apportionment order.
    #[lore_macro::test_pub]
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
#[lore_macro::test_pub]
const MIN_THREADS_PER_POOL: usize = 2;

/// The core runtime's blocking pool: file I/O runs on the lore-io driver, so
/// this serves only genuinely blocking OS APIs with no async form (OS keyring
/// access, AWS SDK initialization, service IPC pipe reads). Fixed and
/// processor-count-independent by design — see the async file I/O enhancement
/// proposal's thread budget.
#[lore_macro::test_pub]
const CORE_BLOCKING_THREADS: usize = 2;

/// The net runtime's blocking pool, pinned to one thread so that a stray
/// `spawn_blocking` there cannot grow it and starve the protocol timers.
/// Budgeted with the core runtime's pool rather than beside it, since both
/// hold threads for the same reason.
#[lore_macro::test_pub]
const NET_BLOCKING_THREADS: usize = 1;

/// Lore's unconstrained per-pool counts, used when no thread limit is set. `io`
/// is the syscall pool's own request, which `lore-io` derives from the core count
/// and `LORE_IO_POOL_THREADS`.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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

/// The core and net pools for a process sized for `connections`, from the `defaults` for its
/// processor count.
///
/// The core runtime runs one thread fewer than `defaults`, which leaves a processor for the net
/// runtime, and never fewer than [`MIN_THREADS_PER_POOL`]. The net runtime takes one thread for
/// each connection, since each connection's QUIC endpoint receives and decrypts on a task of its
/// own, and never fewer than its default. It takes at most half of the core and net threads
/// together, rounded down. A process that opens the connections takes these threads from the core
/// runtime, so it runs as many CPU-bound threads as with one connection: 10 connections on 8
/// processors run 5 core and 4 net threads. A process that serves them adds them to the core
/// runtime, since the commands it serves may open fewer: 7 core and 4 net threads.
#[lore_macro::test_pub]
fn threads_for_connections(defaults: ThreadCounts, connections: NetConnections) -> ThreadCounts {
    let worker = defaults.worker.saturating_sub(1).max(MIN_THREADS_PER_POOL);
    let shared = worker + defaults.net;
    let (NetConnections::Opened(count) | NetConnections::Served(count)) = connections;
    let net = count.clamp(defaults.net, (shared / 2).max(defaults.net));
    ThreadCounts {
        worker: match connections {
            NetConnections::Opened(_) => shared - net,
            NetConnections::Served(_) => worker,
        },
        net,
        ..defaults
    }
}

/// Per-pool sizes before any limit: `defaults`, with an explicit `net_request`
/// substituted, or else sized for the process's storage `connections` where it set any.
#[lore_macro::test_pub]
fn requested_counts(
    defaults: ThreadCounts,
    net_request: Option<usize>,
    connections: Option<NetConnections>,
) -> ThreadCounts {
    match (net_request, connections) {
        (Some(net), _) => ThreadCounts { net, ..defaults },
        (None, Some(connections)) => threads_for_connections(defaults, connections),
        (None, None) => defaults,
    }
}

/// Per-pool sizes before any limit, for this process (see [`requested_counts`]).
fn requested_thread_counts() -> ThreadCounts {
    requested_counts(
        default_thread_counts(processor_count(), lore_io::requested_max_threads()),
        requested_net_threads(),
        NET_CONNECTIONS.get().copied().flatten(),
    )
}

/// Per-pool counts from the core count, an explicit net request or the storage
/// connections, and the limit.
fn budget_thread_counts() -> ThreadCounts {
    apply_thread_limit(requested_thread_counts())
}

/// Per-pool thread counts Lore sizes its runtime for.
///
/// `LORE_NET_THREADS` and [`set_net_threads_default`] set the net pool's ideal,
/// with worker at the core count; otherwise the connections [`set_net_connections`]
/// sets size both, as [`threads_for_connections`] divides them, and a process that
/// sets none keeps the defaults. `LORE_IO_POOL_THREADS` raises the
/// syscall pool's ideal, and blocking is a small fixed count. Whatever the ideals,
/// a thread limit is a ceiling on their total rather than a starting point to
/// negotiate from: the pools are scaled to fit it, down to [`MIN_THREADS_PER_POOL`]
/// each.
pub fn thread_counts() -> ThreadCounts {
    budget_thread_counts()
}

/// Blocking threads across both runtimes: the blocking share of the budget
/// (see [`thread_counts`]).
pub fn default_blocking_threads() -> usize {
    budget_thread_counts().blocking
}

#[lore_macro::test_pub]
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
    /// Net runtime worker threads; `None` keeps the process default: 2, or one per
    /// storage connection [`set_net_connections`] sets, up to half the core and net
    /// threads together, or one per processor in a [`serving_runtime`].
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

impl TokioSettings {
    /// Settings for a process that relays its calls to the Lore service.
    ///
    /// A relaying process writes a request to a socket and reads events back,
    /// and the service does the work. Sizing its pools for work it will not do
    /// costs threads a whole machine's worth of clients pays for. Both pools sit
    /// at [`MIN_THREADS_PER_POOL`], which every pool keeps regardless, so none
    /// can starve another. The net pool is left at the process default: a
    /// relaying process makes no remote calls of its own, and that default is
    /// already small.
    pub fn relay_only() -> Self {
        TokioSettings {
            max_blocking_threads: MIN_THREADS_PER_POOL,
            thread_keep_alive_seconds: default_thread_keep_alive(),
            worker_threads: Some(MIN_THREADS_PER_POOL),
            net_threads: None,
        }
    }
}

/// Returns a handle to the core runtime of a process that serves, building it with `settings`
/// if none exists: one core thread and one net thread per processor, since serving thousands of
/// concurrent client connections is its normal case. `settings.net_threads` and
/// `LORE_NET_THREADS` set the net threads instead, and a thread limit bounds the total. The net
/// threads are set before the runtime is built, since both runtimes are sized from them once.
pub fn serving_runtime(settings: Option<TokioSettings>) -> Handle {
    let _ = set_net_threads_default(
        settings
            .as_ref()
            .and_then(|settings| settings.net_threads)
            .filter(|&count| count > 0)
            .unwrap_or_else(processor_count),
    );
    runtime_with_settings(settings)
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
    settle_net_connections();
    let counts = core_thread_counts(&settings);
    crate::lore_debug!(
        "Core runtime: {} workers, {} blocking; syscall pool: {}",
        counts.worker,
        core_blocking_threads(counts),
        counts.io
    );
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
///
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
