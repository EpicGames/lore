// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Counters, served remote payloads and hooks the read and write paths consult under
//! `test-util`, so a test can count the decodes, hashes and fetches a read costs, answer a remote
//! fetch without a remote, order what concurrent reads, stores and compactions do, fail packfile
//! opens, writes, syncs and metadata reads and store directory stats or keep writes in flight,
//! interrupt a payload gate hold, hold a read's memory past the read, and count the tasks the store
//! runs of its own accord.

use std::cell::Cell;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::Notify;

use crate::types::Address;
use crate::types::Fragment;
use crate::types::Hash;

thread_local! {
    static DECODES: Cell<usize> = const { Cell::new(0) };
    static HASHES: Cell<usize> = const { Cell::new(0) };
    static FETCHES: Cell<usize> = const { Cell::new(0) };
    static REMOTE: RefCell<Vec<(Address, Fragment, Bytes)>> = const { RefCell::new(Vec::new()) };
    static RESOLVED: RefCell<Vec<(Hash, Hash, Fragment, Bytes)>> =
        const { RefCell::new(Vec::new()) };
    static GATE: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
    static FOLLOWERS: Cell<usize> = const { Cell::new(0) };
    static MIDPOINT: RefCell<Option<Midpoint>> = const { RefCell::new(None) };
    static PAUSES: RefCell<Vec<(PausePoint, Arc<Pause>)>> = const { RefCell::new(Vec::new()) };
    static FAILED_WRITES: Cell<usize> = const { Cell::new(0) };
    static FAILED_SYNCS: Cell<usize> = const { Cell::new(0) };
    static FAILED_STATS: Cell<usize> = const { Cell::new(0) };
    static FAILED_OPENS: Cell<usize> = const { Cell::new(0) };
    static FAILED_DIRECTORY_STATS: Cell<usize> = const { Cell::new(0) };
    static PACKFILE_IN_FLIGHT: RefCell<Option<Arc<InFlight>>> = const { RefCell::new(None) };
    static STORE_WRITE_IN_FLIGHT: RefCell<Option<Arc<InFlight>>> = const { RefCell::new(None) };
    static HOLD_INTERRUPT: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static PACKFILE_SIZE_LIMIT: Cell<u64> = const { Cell::new(0) };
}

tokio::task_local! {
    static UNVALIDATED_PUTS: ();
}

/// A point where [`pause_at`] holds a store, a read, a list, a flush or a compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausePoint {
    /// A packfile write has reserved its range and not yet written it.
    Reserved,
    /// A store has written its payload and not yet taken the bucket to insert its entry.
    Written,
    /// A read has its payload's location and has not yet loaded it.
    Located,
    /// Compaction has chosen the packfiles to rewrite and not yet stopped writes to them.
    Selected,
    /// Compaction has stopped writes to the packfiles it rewrites and not yet rewritten one.
    Stopped,
    /// A bucket write has taken the bucket's contents and not yet written its file.
    Serializing,
    /// A sync of every packfile of a packstore has begun and synced none yet.
    Syncing,
    /// A fan-out commit's `level.pending` is on disk, and the write has not yet returned to the
    /// commit.
    Committing,
    /// A size eviction is about to check for a stop or a fan-out and evict from its next bucket.
    Evicting,
    /// An epoch advance has written the epoch and not yet recorded it as the directory's current
    /// one.
    Advanced,
    /// A flush is about to try a group's flush lock, under which it reads whether the group has
    /// anything to write.
    Scanning,
    /// A mutable store list has handed its stream back, and one of its producers, which read its
    /// group's buckets into memory before, has sent none of their keys yet, and holds no lock of
    /// its group.
    Listing,
    /// A mutable store list producer walking its group under the group's layout has sent the keys
    /// of a bucket and not yet read the next.
    Walking,
    /// A measurement of the packstores' size holds the claim it runs on and has measured none
    /// yet.
    Measuring,
    /// A put that verifies its writes has stored its entry and not yet found it again to read its
    /// payload back.
    Verifying,
    /// A verification of the immutable store holds a group's flush lock, has read the group's
    /// bucket count, and has checked none of its buckets.
    Checking,
}

/// A task the store runs of its own accord rather than for a caller, see [`count_store_tasks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreTask {
    /// A group's delayed flush in the immutable store.
    DelayedFlush,
    /// A group's delayed flush in the mutable store.
    MutableDelayedFlush,
    /// A pass of the periodic evictor.
    Evictor,
    /// A step of the periodic compactor.
    Compactor,
    /// An eviction then compaction pass run as one, as a load crossing a cap fires.
    GcPass,
}

impl StoreTask {
    /// Every task, in the order [`count_store_tasks`] counts them, with its name.
    pub const ALL: [(StoreTask, &'static str); 5] = [
        (StoreTask::DelayedFlush, "delayed-flush"),
        (StoreTask::MutableDelayedFlush, "mutable-delayed-flush"),
        (StoreTask::Evictor, "evictor"),
        (StoreTask::Compactor, "compactor"),
        (StoreTask::GcPass, "gc-pass"),
    ];
}

static STORE_TASKS: OnceLock<&'static [AtomicUsize]> = OnceLock::new();

static EVICTOR_FLOOR: AtomicUsize = AtomicUsize::new(0);

/// Makes the periodic evictor enforce capacities down to `floor` fragments in this process,
/// rather than raising any capacity below its floor to the floor, so that a test's small stores
/// meet its passes.
pub fn lower_evictor_floor(floor: usize) {
    EVICTOR_FLOOR.store(floor.max(1), Ordering::Relaxed);
}

/// The floor the periodic evictor raises a capacity to: the one [`lower_evictor_floor`] set, or
/// `default`.
pub(crate) fn evictor_floor(default: usize) -> usize {
    match EVICTOR_FLOOR.load(Ordering::Relaxed) {
        0 => default,
        floor => floor,
    }
}

/// Counts each [`StoreTask`] running in `counters`, one per task in the order of
/// [`StoreTask::ALL`], from now on in this process.
pub fn count_store_tasks(counters: &'static [AtomicUsize]) {
    let _ = STORE_TASKS.set(counters);
}

/// Counts `task` as running until the returned guard drops, where [`count_store_tasks`] asked.
pub(crate) fn store_task(task: StoreTask) -> Option<StoreTaskRunning> {
    let counter = STORE_TASKS.get()?.get(task as usize)?;
    counter.fetch_add(1, Ordering::Relaxed);
    Some(StoreTaskRunning(counter))
}

/// A [`StoreTask`] counted as running, until dropped.
pub(crate) struct StoreTaskRunning(&'static AtomicUsize);

impl Drop for StoreTaskRunning {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A hold armed at a [`PausePoint`]: `reached` is notified when a task arrives there, which then
/// waits for `resume`.
#[derive(Default)]
pub struct Pause {
    pub reached: Notify,
    pub resume: Notify,
}

/// Holds the next task on this thread to reach `point` there until the returned pause is resumed.
/// Only that one task is held. Pauses armed at one point are taken in the order they were armed.
pub fn pause_at(point: PausePoint) -> Arc<Pause> {
    let pause = Arc::new(Pause::default());
    PAUSES.with(|pauses| pauses.borrow_mut().push((point, pause.clone())));
    pause
}

/// Holds this task at `point` if [`pause_at`] armed it on this thread.
pub(crate) async fn pause_point(point: PausePoint) {
    let pause = PAUSES.with(|pauses| {
        let mut pauses = pauses.borrow_mut();
        let position = pauses.iter().position(|(armed, _)| *armed == point)?;
        Some(pauses.remove(position).1)
    });
    if let Some(pause) = pause {
        pause.reached.notify_one();
        pause.resume.notified().await;
    }
}

/// Makes the next `count` packfile writes on this thread fail.
pub fn fail_packfile_writes(count: usize) {
    FAILED_WRITES.with(|failed| failed.set(count));
}

/// Whether this packfile write fails, as [`fail_packfile_writes`] asked.
pub(crate) fn take_packfile_write_failure() -> bool {
    take_failure(&FAILED_WRITES)
}

/// Makes the next `count` packfile syncs on this thread fail, each once it has synced.
pub fn fail_packfile_syncs(count: usize) {
    FAILED_SYNCS.with(|failed| failed.set(count));
}

/// The result of this packfile sync, a failure if [`fail_packfile_syncs`] asked for one.
pub(crate) fn packfile_sync_result() -> std::io::Result<()> {
    if take_failure(&FAILED_SYNCS) {
        return Err(std::io::Error::other("packfile sync failed as asked"));
    }
    Ok(())
}

/// Makes the next `count` reads of a packfile's metadata on this thread fail, as when the packfiles
/// load.
pub fn fail_packfile_stats(count: usize) {
    FAILED_STATS.with(|failed| failed.set(count));
}

/// Whether this read of a packfile's metadata fails, as [`fail_packfile_stats`] asked.
pub(crate) fn take_packfile_stat_failure() -> bool {
    take_failure(&FAILED_STATS)
}

/// Makes the next `count` packfile opens on this thread fail, as when no descriptor is left.
pub fn fail_packfile_opens(count: usize) {
    FAILED_OPENS.with(|failed| failed.set(count));
}

/// Whether this packfile open fails, as [`fail_packfile_opens`] asked.
pub(crate) fn take_packfile_open_failure() -> bool {
    take_failure(&FAILED_OPENS)
}

/// Makes the next `count` checks on this thread of whether a store directory exists fail, as
/// when a stat meets a transient I/O error.
pub fn fail_directory_stats(count: usize) {
    FAILED_DIRECTORY_STATS.with(|failed| failed.set(count));
}

/// Whether this check of a store directory fails, as [`fail_directory_stats`] asked.
pub(crate) fn take_directory_stat_failure() -> bool {
    take_failure(&FAILED_DIRECTORY_STATS)
}

/// Whether this operation fails, taking one of the failures `failures` counts.
fn take_failure(failures: &'static std::thread::LocalKey<Cell<usize>>) -> bool {
    failures.with(|failed| {
        let count = failed.get();
        failed.set(count.saturating_sub(1));
        count > 0
    })
}

/// Makes packfile writes on this thread treat `limit` bytes as the size limit, so that a test
/// reaches the limit without writing gigabytes. 0 restores the real limit.
pub fn limit_packfile_size(limit: u64) {
    PACKFILE_SIZE_LIMIT.with(|size| size.set(limit));
}

/// The size limit [`limit_packfile_size`] set on this thread, if any.
pub(crate) fn packfile_size_limit() -> Option<u64> {
    PACKFILE_SIZE_LIMIT.with(|size| Some(size.get()).filter(|limit| *limit != 0))
}

/// A write [`hold_packfile_write`] or [`hold_store_write`] keeps in flight: it lands only at
/// [`InFlight::complete`], and until then what it was issued with, a packfile write's buffer or
/// another write's completion, is kept, as a driver keeps it until the kernel is done with the
/// write. A task still awaiting the write then goes on with it, as it would once the driver answers;
/// one dropped before it goes on leaves what was kept to be dropped once the write has landed, as
/// the driver drops it. `issued` is notified once the write is issued.
#[derive(Default)]
pub struct InFlight {
    pub issued: Notify,
    landed: Notify,
    parked: parking_lot::Mutex<Parked>,
}

/// What an [`InFlight`] write was issued with, and whether it has landed and is awaited, under one
/// lock, so that whichever of its landing and the drop of the task awaiting it comes second drops
/// what was kept.
#[derive(Default)]
struct Parked {
    kept: Option<Box<dyn std::any::Any + Send>>,
    landed: bool,
    awaited: bool,
}

impl InFlight {
    /// Lands the write: a task awaiting it goes on with what it was issued with, and with none,
    /// that is dropped as the driver drops it once the write completes.
    pub fn complete(&self) {
        let mut parked = self.parked.lock();
        parked.landed = true;
        if parked.awaited {
            self.landed.notify_one();
        } else {
            let kept = parked.kept.take();
            drop(parked);
            drop(kept);
        }
    }

    /// Keeps `kept` and parks the write until it lands, handing `kept` back if it is a `T`.
    async fn park<T: 'static>(&self, kept: Box<dyn std::any::Any + Send>) -> Option<T> {
        struct Awaiting<'a>(&'a parking_lot::Mutex<Parked>);
        impl Drop for Awaiting<'_> {
            fn drop(&mut self) {
                let mut parked = self.0.lock();
                parked.awaited = false;
                let kept = if parked.landed {
                    parked.kept.take()
                } else {
                    None
                };
                drop(parked);
                drop(kept);
            }
        }
        {
            let mut parked = self.parked.lock();
            parked.kept = Some(kept);
            parked.awaited = true;
        }
        let _awaiting = Awaiting(&self.parked);
        self.issued.notify_one();
        self.landed.notified().await;
        let kept = self.parked.lock().kept.take()?;
        kept.downcast().ok().map(|kept| *kept)
    }
}

/// Keeps the next packfile write on this thread in flight, see [`InFlight`].
pub fn hold_packfile_write() -> Arc<InFlight> {
    hold_write(&PACKFILE_IN_FLIGHT)
}

/// Keeps the next bucket, level file, truncate or epoch write on this thread in flight, see
/// [`InFlight`].
pub fn hold_store_write() -> Arc<InFlight> {
    hold_write(&STORE_WRITE_IN_FLIGHT)
}

/// Arms `held` on this thread with a new [`InFlight`].
fn hold_write(
    held: &'static std::thread::LocalKey<RefCell<Option<Arc<InFlight>>>>,
) -> Arc<InFlight> {
    let in_flight = Arc::new(InFlight::default());
    held.with(|held| *held.borrow_mut() = Some(in_flight.clone()));
    in_flight
}

/// Parks this packfile write until it lands, keeping a copy of `buffer` in the [`InFlight`] that
/// [`hold_packfile_write`] armed on this thread, if any.
pub(crate) async fn park_packfile_write<B: Clone + Send + 'static>(buffer: &B) {
    if let Some(in_flight) = PACKFILE_IN_FLIGHT.with(|held| held.borrow_mut().take()) {
        in_flight.park::<()>(Box::new(buffer.clone())).await;
    }
}

/// Parks this write until it lands, keeping `completion` in the [`InFlight`] that
/// [`hold_store_write`] armed on this thread, if any, and hands it back.
pub(crate) async fn park_store_write<C: Send + 'static>(completion: C) -> C {
    match STORE_WRITE_IN_FLIGHT.with(|held| held.borrow_mut().take()) {
        Some(in_flight) => in_flight
            .park(Box::new(completion))
            .await
            .expect("a landed write's completion is handed back"),
        None => completion,
    }
}

/// Runs `puts`, letting the puts it makes store fragments that pass only the size checks, for
/// tests of the paths that read such data. The allowance is the future's, whichever thread polls
/// it, and ends with it.
pub async fn accepting_unvalidated_puts<F: Future>(puts: F) -> F::Output {
    UNVALIDATED_PUTS.scope((), puts).await
}

/// Whether the running task is inside [`accepting_unvalidated_puts`].
pub(crate) fn unvalidated_puts_accepted() -> bool {
    UNVALIDATED_PUTS.try_with(|()| ()).is_ok()
}

/// Runs `hook` once, in the next payload gate hold taken on this thread, after the hold reads the
/// epoch and before it counts itself.
pub fn interrupt_next_hold(hook: impl FnOnce() + 'static) {
    HOLD_INTERRUPT.with(|interrupt| *interrupt.borrow_mut() = Some(Box::new(hook)));
}

/// Runs the hook [`interrupt_next_hold`] set on this thread, if any.
pub(crate) fn hold_interrupted() {
    let hook = HOLD_INTERRUPT.with(|interrupt| interrupt.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

type Midpoint = Box<dyn Fn(Address, bool)>;

/// Records one payload decode on this thread.
pub fn count_decode() {
    DECODES.with(|count| count.set(count.get() + 1));
}

/// Records one content hash on this thread.
pub fn count_hash() {
    HASHES.with(|count| count.set(count.get() + 1));
}

/// Remote fetches, decodes, and hashes recorded on this thread since the last [`reset_counts`].
pub fn counts() -> (usize, usize, usize) {
    (
        FETCHES.with(Cell::get),
        DECODES.with(Cell::get),
        HASHES.with(Cell::get),
    )
}

/// Zeroes the counts [`counts`] reports on this thread.
pub fn reset_counts() {
    FETCHES.with(|count| count.set(0));
    DECODES.with(|count| count.set(0));
    HASHES.with(|count| count.set(0));
}

/// Makes a remote fetch of `address` on this thread answer with `fragment` and `payload`.
pub fn serve_remote(address: Address, fragment: Fragment, payload: Bytes) {
    REMOTE.with(|remote| remote.borrow_mut().push((address, fragment, payload)));
}

/// What [`serve_remote`] last set up for `address` on this thread, counted as one remote fetch.
pub fn remote_payload(address: Address) -> Option<(Fragment, Bytes)> {
    FETCHES.with(|count| count.set(count.get() + 1));
    REMOTE.with(|remote| {
        remote
            .borrow()
            .iter()
            .rfind(|(served, _, _)| *served == address)
            .map(|(_, fragment, payload)| (*fragment, payload.clone()))
    })
}

/// Makes every served remote fetch on this thread wait for one notification of `gate` before it
/// answers.
pub fn hold_remote_fetches(gate: Arc<Notify>) {
    GATE.with(|held| *held.borrow_mut() = Some(gate));
}

/// Waits for the gate [`hold_remote_fetches`] set on this thread, if any.
pub(crate) async fn remote_gate() {
    let gate = GATE.with(|held| held.borrow().clone());
    if let Some(gate) = gate {
        gate.notified().await;
    }
}

/// Records one caller joining a read flight as a follower on this thread.
pub(crate) fn count_follower() {
    FOLLOWERS.with(|count| count.set(count.get() + 1));
}

/// Callers that joined a read flight as followers on this thread.
pub fn followers() -> usize {
    FOLLOWERS.with(Cell::get)
}

/// Runs `hook` inside every store in-flight guard this thread drops, between its landed count and
/// the removal of its entry, with the guard's address and whether its entry is still in flight,
/// until [`clear_guard_midpoint`].
pub fn set_guard_midpoint(hook: impl Fn(Address, bool) + 'static) {
    MIDPOINT.with(|midpoint| *midpoint.borrow_mut() = Some(Box::new(hook)));
}

/// Stops running the hook [`set_guard_midpoint`] set on this thread.
pub fn clear_guard_midpoint() {
    MIDPOINT.with(|midpoint| *midpoint.borrow_mut() = None);
}

/// Runs the hook [`set_guard_midpoint`] set on this thread for a guard of `address`, telling it
/// whether the entry is still in flight as `in_flight` answers.
pub(crate) fn guard_midpoint(address: Address, in_flight: impl FnOnce() -> bool) {
    MIDPOINT.with(|midpoint| {
        if let Some(hook) = midpoint.borrow().as_ref() {
            hook(address, in_flight());
        }
    });
}

/// Makes a remote resolve of `key` on this thread answer with `resolved` and its root.
pub fn serve_remote_resolved(key: Hash, resolved: Hash, fragment: Fragment, payload: Bytes) {
    RESOLVED.with(|remote| {
        remote.borrow_mut().push((key, resolved, fragment, payload));
    });
}

/// What [`serve_remote_resolved`] last set up for `key` on this thread, counted as one remote
/// fetch.
pub fn remote_resolved(key: Hash) -> Option<(Hash, Fragment, Bytes)> {
    FETCHES.with(|count| count.set(count.get() + 1));
    RESOLVED.with(|remote| {
        remote
            .borrow()
            .iter()
            .rfind(|(served, ..)| *served == key)
            .map(|(_, resolved, fragment, payload)| (*resolved, *fragment, payload.clone()))
    })
}

/// How a [`HoldingStore`] answers a `get_into` whose memory it holds.
#[derive(Clone, Copy)]
pub enum HeldAnswer {
    /// Never, as a read dropped while an operation it started is in flight never sees it end.
    Never,
    /// That it placed the payload, leaving the holder of the memory to write it.
    Placed,
}

/// Delegating store holding the memory of every `get_into` of one address, as an I/O operation
/// holds a read's memory past the read's future: the memory goes to the receiver
/// [`HoldingStore::wrap`] returns, and the read is answered as its [`HeldAnswer`] says.
pub struct HoldingStore {
    inner: Arc<dyn crate::immutable_store::ImmutableStore>,
    address: Address,
    answer: HeldAnswer,
    held: tokio::sync::mpsc::UnboundedSender<crate::CallerBuffer>,
}

impl HoldingStore {
    /// `inner`, holding the memory of every `get_into` of `address`, and where that memory goes.
    pub fn wrap(
        inner: Arc<dyn crate::immutable_store::ImmutableStore>,
        address: Address,
        answer: HeldAnswer,
    ) -> (
        Arc<dyn crate::immutable_store::ImmutableStore>,
        tokio::sync::mpsc::UnboundedReceiver<crate::CallerBuffer>,
    ) {
        let (held, receiver) = tokio::sync::mpsc::unbounded_channel();
        let store = Arc::new(HoldingStore {
            inner,
            address,
            answer,
            held,
        });
        (store, receiver)
    }
}

#[async_trait::async_trait]
impl crate::immutable_store::ImmutableStore for HoldingStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<crate::local::store_lock::StoreGuard>, crate::immutable_store::StoreError>
    {
        self.inner.clone().hold_for_command().await
    }

    fn store_lock(&self) -> Option<Arc<crate::local::store_lock::StoreLock>> {
        self.inner.store_lock()
    }

    /// Joins the task claim of the store this one wraps.
    async fn hold_for_task(
        self: Arc<Self>,
    ) -> Result<Option<crate::local::store_lock::StoreGuard>, crate::immutable_store::StoreError>
    {
        self.inner.clone().hold_for_task().await
    }

    async fn get(
        self: Arc<Self>,
        partition: crate::types::Partition,
        address: Address,
    ) -> Result<crate::store_types::StoreGetData, crate::immutable_store::StoreError> {
        self.inner.clone().get(partition, address).await
    }

    async fn get_into(
        self: Arc<Self>,
        partition: crate::types::Partition,
        address: Address,
        dst: &mut crate::CallerBuffer,
    ) -> Result<(Fragment, crate::store_types::PayloadRead), crate::immutable_store::StoreError>
    {
        if address != self.address {
            return self.inner.clone().get_into(partition, address, dst).await;
        }
        match self.answer {
            HeldAnswer::Never => {
                let _ = self.held.send(dst.split_off(0));
                std::future::pending().await
            }
            HeldAnswer::Placed => {
                let stored = self.inner.clone().get_metadata(partition, address).await?;
                let _ = self.held.send(dst.split_off(0));
                Ok((stored.fragment, crate::store_types::PayloadRead::IntoBuffer))
            }
        }
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: crate::types::Partition,
        address: Address,
    ) -> Result<crate::store_types::StoreGetData, crate::immutable_store::StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: crate::types::Partition,
        addresses: &[Address],
        results: &mut [crate::store_types::StoreMatchResult],
    ) -> Result<(), crate::immutable_store::StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn put(
        self: Arc<Self>,
        partition: crate::types::Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), crate::immutable_store::StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: crate::types::Partition,
        address: Address,
        stats: Arc<crate::store_types::StoreObliterateStats>,
    ) -> Result<(), crate::immutable_store::StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<usize, crate::immutable_store::StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, crate::immutable_store::StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(
        self: Arc<Self>,
        sync_data: bool,
    ) -> Result<(), crate::immutable_store::StoreError> {
        self.inner.clone().flush(sync_data).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), crate::immutable_store::StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: crate::types::Partition,
        source_address: Address,
        destination_partition: crate::types::Partition,
        destination_context: crate::types::Context,
        behavior: crate::immutable_store::CopyBehavior,
    ) -> Result<(), crate::immutable_store::StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}
