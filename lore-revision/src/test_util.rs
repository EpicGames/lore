// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Probes a test reads and arms inside the library, points where it stops a branch write, and
//! fixtures tests build on, built with the `test-util` feature.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use tokio::sync::Notify;

use crate::fs::filesystem_provider::DirectoryEntry;
use crate::fs::filesystem_provider::FsError;
use crate::repository::RemoteState;
use crate::repository::RepositoryContext;

/// A path-less context on fresh in-memory stores whose remote is in `state`, so everything but
/// the remote is valid.
pub async fn context_with_state(state: RemoteState) -> Arc<RepositoryContext> {
    context_built(state, |context| context).await
}

/// [`context_with_state`] with `build` applied, for a context set up through its builders.
pub async fn context_built(
    state: RemoteState,
    build: impl FnOnce(RepositoryContext) -> RepositoryContext,
) -> Arc<RepositoryContext> {
    let (immutable, mutable) = crate::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    Arc::new(build(RepositoryContext::new_with_state(
        None,
        immutable,
        mutable,
        crate::lore::RepositoryId::default(),
        crate::instance::InstanceId::default(),
        state,
        Arc::default(),
        None,
    )))
}

/// The tasks one walk spawns: counts those live, for a test to read the peak, holds each one
/// before its work until released, so a test can see the tasks pile up, fails a listing where a
/// test arms it, and sizes the walk's task budget. A walk holds one in a `static`, shared by the
/// whole process, so the tests using one run alone in their binary, one after another.
pub struct TaskProbe {
    live: AtomicUsize,
    peak: AtomicUsize,
    spawned: AtomicUsize,
    held: AtomicBool,
    released: Notify,
    fail_after: AtomicUsize,
    budget: AtomicUsize,
}

impl TaskProbe {
    /// A probe with nothing counted, held or armed.
    pub(crate) const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            spawned: AtomicUsize::new(0),
            held: AtomicBool::new(false),
            released: Notify::const_new(),
            fail_after: AtomicUsize::new(usize::MAX),
            budget: AtomicUsize::new(crate::MAX_CONCURRENT_TREE_TASKS),
        }
    }

    /// Zeroes the counts, disarms the failure, restores the budget and releases a hold.
    pub fn reset(&self) {
        self.live.store(0, Ordering::Relaxed);
        self.peak.store(0, Ordering::Relaxed);
        self.spawned.store(0, Ordering::Release);
        self.fail_after.store(usize::MAX, Ordering::Release);
        self.budget
            .store(crate::MAX_CONCURRENT_TREE_TASKS, Ordering::Relaxed);
        self.release();
    }

    /// Sizes the budget the walk's next run spawns under, so a test can exceed it with a small
    /// tree. [`crate::MAX_CONCURRENT_TREE_TASKS`] until set.
    pub fn set_budget(&self, permits: usize) {
        self.budget.store(permits, Ordering::Relaxed);
    }

    /// The permits the walk's budget holds.
    pub(crate) fn budget(&self) -> usize {
        self.budget.load(Ordering::Relaxed)
    }

    /// Fails the first entry the walk lists once `spawned` tasks have been spawned, as an entry
    /// the listing cannot read fails.
    pub fn fail_entry_after(&self, spawned: usize) {
        self.fail_after.store(spawned, Ordering::Release);
    }

    /// `entry`, or the failure [`Self::fail_entry_after`] armed in its place, which it answers
    /// once.
    pub(crate) fn entry(
        &self,
        entry: Result<DirectoryEntry, FsError>,
    ) -> Result<DirectoryEntry, FsError> {
        let armed = self.fail_after.load(Ordering::Acquire);
        if self.spawned.load(Ordering::Acquire) >= armed
            && self
                .fail_after
                .compare_exchange(armed, usize::MAX, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return Err(FsError::internal("The listing failed"));
        }
        entry
    }

    /// Stops each task about to do its work until [`Self::release`] is called.
    pub fn hold(&self) {
        self.held.store(true, Ordering::Release);
    }

    /// Lets the tasks [`Self::hold`] stopped go on.
    pub fn release(&self) {
        self.held.store(false, Ordering::Release);
        self.released.notify_waiters();
    }

    /// Waits while a hold is on. The wakeup is registered before the flag is read, so a release
    /// between the two is not missed.
    pub(crate) async fn wait_if_held(&self) {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !self.held.load(Ordering::Acquire) {
                return;
            }
            released.await;
        }
    }

    /// The tasks live now.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// The most tasks live at once since the last reset.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// The tasks spawned since the last reset.
    pub fn spawned(&self) -> usize {
        self.spawned.load(Ordering::Acquire)
    }

    /// Counts a task as spawned, and as live until the guard is dropped.
    pub(crate) fn enter(&self) -> Live<'_> {
        self.spawned.fetch_add(1, Ordering::AcqRel);
        let live = self.live.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(live, Ordering::AcqRel);
        Live(self)
    }
}

/// A task [`TaskProbe::enter`] counts as live until this is dropped.
pub(crate) struct Live<'a>(&'a TaskProbe);

impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A point between two writes or reads of a branch create, restore, archive or protect, where a
/// task run under [`step_branch_writes`] stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchStep {
    /// A create has found no metadata to restore and read the mapping of its name, and not yet
    /// checked its parent.
    CreateNameRead,
    /// A create has found no metadata to restore and not yet swapped in the branch's metadata.
    CreateUnrestored,
    /// A create has swapped in the branch's metadata and not yet claimed its name.
    CreateMetadataStored,
    /// A create has claimed its name and not yet stored the latest revision.
    CreateNameClaimed,
    /// A restore has read the branch's metadata and not yet made its checks.
    RestoreMetadataRead,
    /// A restore has made its checks and not yet recorded itself in the metadata.
    RestoreChecked,
    /// A restore has recorded the name it claims in the metadata and not yet claimed it.
    RestoreRecorded,
    /// A restore has claimed its name and not yet made its final swap.
    RestoreNameClaimed,
    /// A restore, an archive or a name lookup that caches the remote's metadata has swapped a
    /// pending restore's record out of the metadata and not yet released the name it named.
    PendingRestoreSettled,
    /// An archive has read the branch's metadata and not yet looked up its name.
    ArchiveMetadataRead,
    /// An archive has looked up the branch's name and not yet swapped the metadata.
    ArchiveLookedUp,
    /// An archive has swapped the metadata and not yet released the name.
    ArchiveReleasing,
    /// A protect without a remote has read the branch's metadata and not yet swapped it.
    ProtectMetadataRead,
}

/// Where a stepped task reports each step it reaches, and what it waits for to go on.
#[derive(Clone)]
struct Stepped {
    reached: tokio::sync::mpsc::UnboundedSender<BranchStep>,
    resume: Arc<Notify>,
}

tokio::task_local! {
    static STEPPED: Stepped;
}

/// Runs `future`, stopping at each [`BranchStep`] it reaches: the step is sent on `reached`, and
/// the task waits for a notification of `resume` before it goes on. Tasks `future` spawns don't
/// stop.
pub fn step_branch_writes<F: Future>(
    reached: tokio::sync::mpsc::UnboundedSender<BranchStep>,
    resume: Arc<Notify>,
    future: F,
) -> impl Future<Output = F::Output> {
    STEPPED.scope(Stepped { reached, resume }, future)
}

/// Stops this task at `step`, where it runs under [`step_branch_writes`].
pub(crate) async fn branch_step(step: BranchStep) {
    let Ok(stepped) = STEPPED.try_with(Stepped::clone) else {
        return;
    };
    if stepped.reached.send(step).is_ok() {
        stepped.resume.notified().await;
    }
}

/// Where a task run under [`hold_unmount_drains`] reports that a FUSE unmount's drain found the
/// inode table counting no lookups, and what it waits for to go on.
#[derive(Clone)]
struct DrainHold {
    reached: tokio::sync::mpsc::UnboundedSender<()>,
    resume: Arc<Notify>,
}

tokio::task_local! {
    static DRAIN_HOLD: DrainHold;
}

/// Runs `future`, stopping each FUSE unmount's drain it runs wherever the drain finds the inode
/// table counting no lookups, before it waits for the count to settle: a `()` is sent on
/// `reached`, and the drain waits for a notification of `resume` before it goes on. Tasks
/// `future` spawns don't stop.
pub fn hold_unmount_drains<F: Future>(
    reached: tokio::sync::mpsc::UnboundedSender<()>,
    resume: Arc<Notify>,
    future: F,
) -> impl Future<Output = F::Output> {
    DRAIN_HOLD.scope(DrainHold { reached, resume }, future)
}

/// Stops this task where a FUSE unmount's drain has found the inode table counting no lookups,
/// where it runs under [`hold_unmount_drains`].
pub(crate) async fn unmount_drain_emptied() {
    let Ok(hold) = DRAIN_HOLD.try_with(DrainHold::clone) else {
        return;
    };
    if hold.reached.send(()).is_ok() {
        hold.resume.notified().await;
    }
}
