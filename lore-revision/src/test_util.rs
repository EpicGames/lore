// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Probes a test reads and arms inside the library, points where it stops a branch write, and
//! fixtures tests build on, built with the `test-util` feature.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::error::NoRemote;
use lore_base::types::Context;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::WriteOptions;
use lore_transport::ProtocolError;
use tokio::sync::Notify;

use crate::fs::filesystem_provider::FsError;
use crate::instance;
use crate::instance::InstanceId;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::repository::DOT_LORE;
use crate::repository::INSTANCE;
use crate::repository::RemoteState;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryContextCreationArgs;
use crate::repository::RepositoryWriteToken;
use crate::state::State;
use crate::state::StateData;

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

/// Registers a new instance of `repository` in the stores `immutable` and `mutable` at `dir`, with
/// `branch` and `revision` as its current branch and revision, and writes `<dir>/.lore/instance`
/// holding its id, so the registration reads as live. Returns the instance's id. The writes run
/// in the caller's execution context.
pub async fn register_live_instance(
    immutable: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    repository: RepositoryId,
    branch: BranchId,
    revision: Hash,
    dir: &Path,
) -> InstanceId {
    let instance_id = InstanceId::generate();
    let context = Arc::new(
        RepositoryContext::new(RepositoryContextCreationArgs {
            paths: None,
            immutable_store: immutable,
            mutable_store: mutable,
            id: repository,
            instance_id,
            remote: Err(ProtocolError::from(NoRemote)),
            filter: Arc::default(),
            filesystem_provider: None,
        })
        .with_write_token(RepositoryWriteToken::acquire(dir).await),
    );
    instance::register_instance(&context, instance_id, &dir.display().to_string())
        .await
        .expect("registers the instance");
    instance::store_current_anchor_branch(&context, branch)
        .await
        .expect("stores the current branch");
    instance::store_current_anchor(&context, revision)
        .await
        .expect("stores the current revision");
    let dot_lore = dir.join(DOT_LORE);
    lore_io::IoDriver::global()
        .create_dir_all(&dot_lore)
        .await
        .expect("creates the Lore directory");
    instance_id
        .write_to_file(dot_lore.join(INSTANCE))
        .await
        .expect("writes the instance file");
    instance_id
}

/// Writes through `repository` a revision numbered `number` whose self parent is `parent` and whose
/// metadata hash is `metadata`, and returns its hash.
pub async fn write_state_revision(
    repository: &Arc<RepositoryContext>,
    parent: Hash,
    number: u64,
    metadata: Hash,
) -> Hash {
    let token = repository
        .try_write_token()
        .expect("the context carries a write token");
    let state = State::new();
    state.set_parent_self(parent);
    state.set_revision_number(number);
    state.set_metadata_hash(metadata);
    state
        .serialize(repository.clone(), token)
        .await
        .expect("writes the revision state")
}

/// Writes through `repository` a blob of a revision state's size that is not a revision state:
/// zeroed, so its header lacks the state magic. Read as a state, it is a revision numbered 0
/// without parents. Returns its hash.
pub async fn write_not_a_state(repository: &Arc<RepositoryContext>) -> Hash {
    crate::immutable::write(
        repository.clone(),
        Context::default(),
        Bytes::from(vec![0u8; size_of::<StateData>()]),
        WriteOptions::default(),
    )
    .await
    .expect("writes the blob")
    .hash
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
    pub(crate) fn entry<T>(&self, entry: Result<T, FsError>) -> Result<T, FsError> {
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
#[cfg(target_os = "linux")]
#[derive(Clone)]
struct DrainHold {
    reached: tokio::sync::mpsc::UnboundedSender<()>,
    resume: Arc<Notify>,
}

tokio::task_local! {
    #[cfg(target_os = "linux")]
    static DRAIN_HOLD: DrainHold;
}

/// Runs `future`, stopping each FUSE unmount's drain it runs wherever the drain finds the inode
/// table counting no lookups, before it waits for the count to settle: a `()` is sent on
/// `reached`, and the drain waits for a notification of `resume` before it goes on. Tasks
/// `future` spawns don't stop.
#[cfg(target_os = "linux")]
pub fn hold_unmount_drains<F: Future>(
    reached: tokio::sync::mpsc::UnboundedSender<()>,
    resume: Arc<Notify>,
    future: F,
) -> impl Future<Output = F::Output> {
    DRAIN_HOLD.scope(DrainHold { reached, resume }, future)
}

/// Stops this task where a FUSE unmount's drain has found the inode table counting no lookups,
/// where it runs under [`hold_unmount_drains`].
#[cfg(target_os = "linux")]
pub(crate) async fn unmount_drain_emptied() {
    let Ok(hold) = DRAIN_HOLD.try_with(DrainHold::clone) else {
        return;
    };
    if hold.reached.send(()).is_ok() {
        hold.resume.notified().await;
    }
}
