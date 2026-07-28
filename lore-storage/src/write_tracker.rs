// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Per-operation tracker for background fragment-write tasks.
//!
//! A `WriteTracker` collects leader and follower tasks produced during a
//! commit-like operation. Every spawned task self-reports its result into
//! shared atomic/lock state when it finishes, so nothing accumulates
//! per-task in the tracker — memory stays O(in-flight) regardless of total
//! spawns. Leaders and followers use the same dispatch machinery; the names
//! reflect the caller's role (leader owns the permit/buffer, follower
//! shadows an existing in-flight leader) and carry no tracker-level
//! difference.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::error::StorageError;
use crate::types::Address;
use crate::types::Fragment;

/// Result type every leader / follower task yields.
pub type TrackedResult = Result<(Address, Fragment), StorageError>;

/// Callback invoked per stored fragment with its header and dedup status.
pub type FragmentObserver = Arc<dyn Fn(&Fragment, bool) + Send + Sync>;

/// Per-operation background-write tracker.
///
/// Construct one per top-level operation (commit, import, …). Dispatch is
/// lock-free: [`spawn_leader`](Self::spawn_leader) and
/// [`register_follower`](Self::register_follower) each take one atomic
/// increment plus a `tokio::spawn`. Shared behind `Arc<WriteTracker>` — all
/// methods take `&self`, so many concurrent dispatchers can drive the same
/// tracker without blocking each other.
///
/// [`await_all`](Self::await_all) consumes the `Arc` and waits for every
/// spawned task to finish. No task is left dangling when it returns.
pub struct WriteTracker {
    /// Shared with every in-flight task. The `Arc` keeps it alive past the
    /// tracker's own lifetime if a task outlives `await_all` for any reason
    /// (shouldn't happen, but is sound).
    state: Arc<SharedState>,
    on_fragment: Option<FragmentObserver>,
}

/// State shared between the tracker and every running task.
struct SharedState {
    /// Number of tasks that have been spawned but not yet finished.
    /// Decremented by the `DecGuard` inside each task body.
    in_flight: AtomicUsize,
    /// First error observed by any task. Later errors are discarded.
    /// Mutex is only touched on the error path (rare) and once at drain.
    first_error: Mutex<Option<StorageError>>,
    /// Notified when `in_flight` transitions to 0 so `await_all` can proceed.
    idle: Notify,
}

/// Decrements `in_flight` in its destructor so the counter returns to zero
/// even if a task panics under `panic = "unwind"`.
struct DecGuard(Arc<SharedState>);

impl Drop for DecGuard {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_one();
        }
    }
}

impl WriteTracker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(SharedState {
                in_flight: AtomicUsize::new(0),
                first_error: Mutex::new(None),
                idle: Notify::new(),
            }),
            on_fragment: None,
        }
    }

    /// Construct a tracker that invokes `observer` for each stored fragment.
    pub fn with_observer(observer: FragmentObserver) -> Self {
        Self {
            on_fragment: Some(observer),
            ..Self::new()
        }
    }

    /// Construct a fresh tracker carrying the same observer as `self`.
    pub fn new_like(&self) -> Self {
        Self {
            on_fragment: self.on_fragment.clone(),
            ..Self::new()
        }
    }

    /// Invoke the observer, if one is installed.
    pub fn notify_fragment(&self, fragment: &Fragment, deduplicated: bool) {
        if let Some(observer) = &self.on_fragment {
            observer(fragment, deduplicated);
        }
    }

    /// Spawn a leader task onto the shared runtime. Returns immediately.
    ///
    /// The task self-reports its result into the shared state when it
    /// finishes — the tracker does not retain a `JoinHandle` or any per-task
    /// bookkeeping, so memory is bounded by in-flight count rather than total
    /// spawns.
    pub fn spawn_leader<F>(&self, future: F)
    where
        F: Future<Output = TrackedResult> + Send + 'static,
    {
        self.spawn(future);
    }

    /// Spawn a follower task onto the shared runtime. Followers wait for an
    /// existing in-flight leader (potentially from another tracker) to
    /// finish, then query the store; they hold no permit or buffer.
    ///
    /// Followers are spawned rather than queued so they progress in parallel
    /// with leaders during dispatch instead of stacking up until `await_all`.
    pub fn register_follower<F>(&self, future: F)
    where
        F: Future<Output = TrackedResult> + Send + 'static,
    {
        self.spawn(future);
    }

    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = TrackedResult> + Send + 'static,
    {
        self.state.in_flight.fetch_add(1, Ordering::Relaxed);
        let state = Arc::clone(&self.state);
        let body = async move {
            let guard = DecGuard(Arc::clone(&state));
            if let Err(e) = future.await {
                let mut slot = state.first_error.lock();
                if slot.is_none() {
                    *slot = Some(e);
                }
            }
            drop(guard);
        };
        lore_base::lore_spawn!(body);
    }

    /// Wait for every outstanding task to finish and return the first error
    /// observed by any of them (if any).
    ///
    /// # Ownership invariant
    ///
    /// `await_all` consumes the `Arc<WriteTracker>` and requires it to be the
    /// sole outstanding tracker handle — any extra clone held by a concurrent
    /// dispatcher would race with the drain. The uniqueness check is
    /// enforced via `Arc::into_inner`: if any other clone is live,
    /// `await_all` returns an `Err` rather than draining a partial view.
    ///
    /// Acceptable patterns:
    ///
    /// - Dispatch all work, then `await_all` (today's commit pattern).
    /// - Construct a fresh `WriteTracker` per operation; never share across
    ///   operations that overlap in time.
    ///
    /// Unsafe patterns (will return `Err`):
    ///
    /// - Sharing one `Arc<WriteTracker>` between two commits running
    ///   concurrently on different tasks.
    /// - A background task that dispatches into the tracker from a
    ///   `tokio::spawn` whose lifetime isn't joined before `await_all`.
    pub async fn await_all(self: Arc<Self>) -> Result<(), StorageError> {
        let tracker = Arc::into_inner(self).ok_or_else(|| {
            StorageError::internal(
                "WriteTracker::await_all called while another Arc handle is live",
            )
        })?;

        // Wait for every task to finish. `Notify::notified()` must be
        // created before the predicate check so a notification between the
        // check and the await isn't lost.
        loop {
            let notified = tracker.state.idle.notified();
            if tracker.state.in_flight.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }

        tracker.state.first_error.lock().take().map_or(Ok(()), Err)
    }
}

impl Default for WriteTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use zerocopy::FromZeros;

    use super::*;
    use crate::types::Context;
    use crate::types::Hash;

    fn ok_result() -> TrackedResult {
        Ok((
            Address {
                context: Context::new_zeroed(),
                hash: Hash::new_zeroed(),
            },
            Fragment {
                flags: 0,
                size_payload: 1,
                size_content: 1,
            },
        ))
    }

    #[tokio::test]
    async fn await_all_empty_returns_ok() {
        let tracker = Arc::new(WriteTracker::new());
        assert!(tracker.await_all().await.is_ok());
    }

    #[tokio::test]
    async fn await_all_leader_and_follower_success() {
        let tracker = Arc::new(WriteTracker::new());
        tracker.spawn_leader(async { ok_result() });
        tracker.register_follower(async { ok_result() });
        assert!(tracker.await_all().await.is_ok());
    }

    #[tokio::test]
    async fn await_all_drains_all_leaders_even_when_one_fails() {
        let tracker = Arc::new(WriteTracker::new());
        let slow_done = Arc::new(AtomicUsize::new(0));

        tracker.spawn_leader(async { Err(StorageError::internal("boom")) });
        for _ in 0..3 {
            let slow_done = Arc::clone(&slow_done);
            tracker.spawn_leader(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                slow_done.fetch_add(1, Ordering::SeqCst);
                ok_result()
            });
        }

        let result = tracker.await_all().await;
        assert!(result.is_err(), "failing leader should surface as error");
        assert_eq!(
            slow_done.load(Ordering::SeqCst),
            3,
            "all slow leaders must run to completion even after a sibling fails"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_flight_counter_returns_to_zero() {
        let tracker = Arc::new(WriteTracker::new());
        for _ in 0..1000 {
            tracker.spawn_leader(async { ok_result() });
        }
        assert!(tracker.await_all().await.is_ok());
    }

    #[tokio::test]
    async fn await_all_returns_first_error_drops_later() {
        let tracker = Arc::new(WriteTracker::new());
        tracker.spawn_leader(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Err::<(Address, Fragment), _>(StorageError::internal("first"))
        });
        tracker.spawn_leader(async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Err::<(Address, Fragment), _>(StorageError::internal("second"))
        });
        let err = tracker
            .await_all()
            .await
            .expect_err("at least one leader failed");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("first"),
            "expected first error preserved, got: {msg}"
        );
        assert!(
            !msg.contains("second"),
            "expected later errors dropped, got: {msg}"
        );
    }

    #[tokio::test]
    async fn arc_into_inner_uniqueness_check() {
        let tracker = Arc::new(WriteTracker::new());
        let stray = Arc::clone(&tracker);
        let err = tracker
            .await_all()
            .await
            .expect_err("second live Arc must make await_all fail");
        let msg = format!("{err:?}");
        assert!(msg.contains("Arc handle is live"));
        drop(stray);
    }
}
