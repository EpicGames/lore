// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Reads that the callers of one server process share, per key, for a window after each starts.

use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use lore_base::lore_spawn;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Histogram;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use crate::util::core_hop::outliving_request;

/// How a caller of [`SharedReads::read`] was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// The caller started a new read.
    New,
    /// The caller joined a read still running.
    JoinedRunning,
    /// The caller got the result of a read that finished within the window.
    ReusedFinished,
    /// The caller got the error of a read that failed within the window.
    KeptError,
}

impl Served {
    /// The value of the `served` label of the callers counter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::JoinedRunning => "joined_running",
            Self::ReusedFinished => "reused_finished",
            Self::KeptError => "kept_error",
        }
    }
}

/// The longest window [`SharedReads::new`] accepts. A day, so that the end of a window is an
/// `Instant` for as long as a process can run.
pub const MAX_SHARED_READ_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// A window longer than [`MAX_SHARED_READ_WINDOW`].
#[derive(Debug, thiserror::Error)]
#[error(
    "a shared read window of {window:?} is longer than the maximum, {MAX_SHARED_READ_WINDOW:?}"
)]
pub struct WindowTooLong {
    pub window: Duration,
}

/// The task of a shared read ended without a result: it panicked, or the runtime shut down.
#[derive(Clone, Copy, Debug)]
pub struct ReadTaskFailed;

impl From<ReadTaskFailed> for tonic::Status {
    fn from(_failed: ReadTaskFailed) -> Self {
        tonic::Status::internal("The shared read's task failed")
    }
}

/// The instruments of a [`SharedReads`].
#[derive(Clone)]
pub struct SharedReadMetrics {
    started: Counter<u64>,
    callers: Counter<u64>,
    duration: Histogram<f64>,
}

impl SharedReadMetrics {
    /// The instruments `<name>.started` (reads started), `<name>.callers` (callers, labelled
    /// `served` by how they were served) and `<name>.duration` (milliseconds per read), in the
    /// namespace of `provider`.
    pub fn new(provider: &impl InstrumentProvider, name: &str) -> Self {
        Self {
            started: provider.counter(format!("{name}.started")),
            callers: provider.counter(format!("{name}.callers")),
            duration: provider.latency_histogram_ms(format!("{name}.duration")),
        }
    }
}

/// The result a read publishes: `None` while it runs.
type Published<T, E> = Option<Result<T, E>>;

/// The latest read of a key.
struct Entry<T, E> {
    id: Uuid,
    started: Instant,
    result: watch::Receiver<Published<T, E>>,
}

/// Reads shared per key between the callers of one process.
///
/// A caller joins the running read of its key, whatever its age, so a key has at most one read in
/// flight. A caller gets the result or the error of a finished read that started less than the
/// window ago, and starts a new read otherwise. With a zero window, only running reads are shared.
///
/// Each read runs in a task of its own, without the cancellation of the request that started it,
/// so it runs to its end whatever happens to its callers. The task removes the read's entry once the
/// window has passed since the read started, or at once if the task panics or is dropped, if the
/// entry is still the read's. A caller that holds the result keeps it until it drops it.
pub struct SharedReads<K, T, E> {
    window: Duration,
    entries: DashMap<K, Entry<T, E>>,
    metrics: SharedReadMetrics,
}

/// A caller's hold on a read of [`SharedReads`].
pub struct SharedRead<T, E> {
    id: Uuid,
    served: Served,
    result: watch::Receiver<Published<T, E>>,
}

/// A caller's hold on only the result of a read of [`SharedReads`]: a [`SharedRead`] without its id
/// and how the caller was served.
pub struct PendingRead<T, E> {
    result: watch::Receiver<Published<T, E>>,
}

impl<K, T, E> SharedReads<K, T, E>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    /// Reads shared for `window` after each starts, counted in `metrics`. Fails for a window
    /// longer than [`MAX_SHARED_READ_WINDOW`].
    pub fn new(window: Duration, metrics: SharedReadMetrics) -> Result<Arc<Self>, WindowTooLong> {
        if window > MAX_SHARED_READ_WINDOW {
            return Err(WindowTooLong { window });
        }
        Ok(Arc::new(Self {
            window,
            entries: DashMap::new(),
            metrics,
        }))
    }

    /// The read of `key` the caller uses. A new read is the future `start` returns for the read's
    /// id, which identifies the read to its callers.
    ///
    /// The shard write lock that `DashMap::entry` takes is held from the check of the key's entry to
    /// the insert of a new one, so a key never has two reads in flight. While it is held, the code
    /// reads the entry's watch channel, makes the new read's id and channel, and drops a replaced
    /// entry with its result, none of which touches `entries`. The lock is released before the
    /// read's task is spawned.
    #[allow(clippy::disallowed_methods)]
    pub fn read<F>(
        self: &Arc<Self>,
        key: K,
        start: impl FnOnce(Uuid) -> F + Send + 'static,
    ) -> SharedRead<T, E>
    where
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let slot = self.entries.entry(key.clone());
        if let dashmap::Entry::Occupied(occupied) = &slot
            && let Some(served) = self.serves(occupied.get())
        {
            let entry = occupied.get();
            let read = SharedRead {
                id: entry.id,
                served,
                result: entry.result.clone(),
            };
            drop(slot);
            self.count_caller(served);
            return read;
        }
        let id = Uuid::new_v4();
        let started = Instant::now();
        let (publish, result) = watch::channel(None);
        drop(slot.insert(Entry {
            id,
            started,
            result: result.clone(),
        }));
        self.metrics.started.add(1, &[]);
        self.count_caller(Served::New);
        lore_spawn!(outliving_request(run_read(
            self.clone(),
            key,
            id,
            started,
            start,
            publish,
        )));
        SharedRead {
            id,
            served: Served::New,
            result,
        }
    }

    /// How `entry` serves a caller, or `None` where the caller must start a new read: the read
    /// finished at least the window ago, or its task ended without a result.
    fn serves(&self, entry: &Entry<T, E>) -> Option<Served> {
        let finished_served = match &*entry.result.borrow() {
            None if entry.result.has_changed().is_ok() => return Some(Served::JoinedRunning),
            None => return None,
            Some(Ok(_)) => Served::ReusedFinished,
            Some(Err(_)) => Served::KeptError,
        };
        (entry.started.elapsed() < self.window).then_some(finished_served)
    }

    fn count_caller(&self, served: Served) {
        self.metrics
            .callers
            .add(1, &[KeyValue::new("served", served.as_str())]);
    }
}

impl<K: Eq + Hash, T, E> SharedReads<K, T, E> {
    /// Removes the entry of `key` if it is the read `id`'s.
    fn remove_own(&self, key: &K, id: Uuid) {
        self.entries.remove_if(key, |_, entry| entry.id == id);
    }

    /// Whether the registry holds an entry for `key`.
    #[cfg(feature = "test-util")]
    pub fn holds(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }
}

/// Run the read `id` of `key`, which started at `started`, publish its result, and remove its
/// entry from `reads` once the window has passed since it started. A read that panics, or whose
/// task is dropped, removes its entry at once.
///
/// Not an `async fn`, which would hold a second copy of its arguments. The read's future is built
/// in the block, so the block does not hold it beside its own state.
#[allow(clippy::manual_async_fn)]
fn run_read<K, T, E, F>(
    reads: Arc<SharedReads<K, T, E>>,
    key: K,
    id: Uuid,
    started: Instant,
    start: impl FnOnce(Uuid) -> F + Send + 'static,
    publish: watch::Sender<Published<T, E>>,
) -> impl Future<Output = ()> + Send + 'static
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    async move {
        let own = OwnEntry { reads, key, id };
        let result = start(id).await;
        own.reads
            .metrics
            .duration
            .record(started.elapsed().as_secs_f64() * 1000.0, &[]);
        publish.send_replace(Some(result));
        drop(publish);
        tokio::time::sleep_until(started + own.reads.window).await;
    }
}

/// Removes the entry of the read `id` of `key` from `reads` when dropped, if it is still the
/// read's.
struct OwnEntry<K: Eq + Hash, T, E> {
    reads: Arc<SharedReads<K, T, E>>,
    key: K,
    id: Uuid,
}

impl<K: Eq + Hash, T, E> Drop for OwnEntry<K, T, E> {
    fn drop(&mut self) {
        self.reads.remove_own(&self.key, self.id);
    }
}

impl<T, E> SharedRead<T, E>
where
    T: Clone,
    E: Clone + From<ReadTaskFailed>,
{
    /// The id of the read.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// How the caller was served.
    pub fn served(&self) -> Served {
        self.served
    }

    /// The [`PendingRead`] of this read: the hold on its result only.
    pub fn into_pending(self) -> PendingRead<T, E> {
        PendingRead {
            result: self.result,
        }
    }

    /// The result of the read, once it has finished.
    pub fn result(self) -> impl Future<Output = Result<T, E>> {
        self.into_pending().result()
    }
}

impl<T, E> PendingRead<T, E>
where
    T: Clone,
    E: Clone + From<ReadTaskFailed>,
{
    /// The result of the read, once it has finished.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    pub fn result(mut self) -> impl Future<Output = Result<T, E>> {
        async move {
            match self.result.wait_for(Option::is_some).await {
                Ok(published) => published
                    .clone()
                    .unwrap_or_else(|| Err(E::from(ReadTaskFailed))),
                Err(_closed) => Err(E::from(ReadTaskFailed)),
            }
        }
    }
}
