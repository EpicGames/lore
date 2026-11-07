// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Sequential prefetch: leaves ahead of a sequential READ stream, loaded by a task of the stream's
//! own into the inode's state, where later READs find them.

use std::future::poll_fn;
use std::ptr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;

use bytes::Bytes;
use crossbeam_epoch as epoch;
use crossbeam_epoch::Shared;
use futures::task::AtomicWaker;
use lore_base::types::Address;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_base::types::typed_bytes::TypedBytes as _;
use lore_storage::StorageError;

use super::Cached;
use super::Cell;
use super::Fetched;
use super::FragmentCache;
use super::InodeSlot;
use super::Job;
use super::MAX_DEPTH;
use super::Span;
use super::Walk;
use super::cell_get;
use super::cell_swap;
use super::check_list;
use super::check_sublist;
use super::child_key;
use super::is_list;
use super::locate;
use crate::repository::RepositoryContext;

/// READs in a row starting where the stream ended before a READ that loads starts a stream.
const SEQUENTIAL_READS: u32 = 2;
/// The least distance from where the stream ended at which a READ breaks it: one stream's READs
/// are served by several workers, not always in order. A READ widens it to twice its length.
const SLACK: u64 = FRAGMENT_SIZE_THRESHOLD as u64;
/// Cells prefetched leaves go to. A leaf's cell is its [`child_key`] modulo this, so a READ finds
/// a prefetched leaf with one probe and consecutive leaves never share a cell.
pub(super) const AHEAD: usize = 64;
/// How far ahead of the stream the first batch reaches, and the next after the budget cut one
/// short.
const FIRST_WINDOW: u64 = 512 << 10;
/// The furthest ahead of the stream a batch reaches; each batch doubles the window up to it.
const MAX_WINDOW: u64 = 2 << 20;
/// Prefetched leaves hold at most this fraction of the inode budget together, and never evict:
/// speculation leaves other inodes' demand-loaded state alone.
const AHEAD_SHARE: usize = 4;

type Ring = [Cell; AHEAD];

/// A sequential READ stream over one inode, and the leaves prefetched ahead of it. Allocated on
/// the inode's first READ with prefetch on.
#[lore_macro::test_pub]
pub(super) struct Stream {
    /// Where the stream's furthest READ ended.
    cursor: AtomicU64,
    /// Twice the latest READ's length: how far behind where the stream ended its READs in flight
    /// may still be reading. The stream breaks only beyond the larger of this and [`SLACK`].
    reorder: AtomicU64,
    /// READs in a row starting where the stream ended.
    run: AtomicU32,
    /// Moves on when the stream breaks or its inode's state is released; a task serves only the
    /// generation it started under.
    generation: AtomicU32,
    /// Whether a task serves the stream.
    active: AtomicBool,
    /// Whether a task has kept leaves in `ahead`: set before the first, so a release that finds
    /// it clear knows the task will see the release and take its leaf back.
    streamed: AtomicBool,
    /// The cursor at which the waiting task wants to run; `u64::MAX` while none waits.
    wake_at: AtomicU64,
    waker: AtomicWaker,
    /// Bytes this stream and its ring count in the inode budget. Taken out whole, once, by the
    /// release of the inode's state or by whoever counted bytes in after it.
    counted: AtomicUsize,
    /// The prefetched leaves, allocated by the first task.
    ahead: OnceLock<Box<Ring>>,
}

impl Default for Stream {
    fn default() -> Self {
        Self {
            cursor: AtomicU64::new(0),
            reorder: AtomicU64::new(0),
            run: AtomicU32::new(0),
            generation: AtomicU32::new(0),
            active: AtomicBool::new(false),
            streamed: AtomicBool::new(false),
            wake_at: AtomicU64::new(u64::MAX),
            waker: AtomicWaker::new(),
            counted: AtomicUsize::new(0),
            ahead: OnceLock::new(),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(ahead) = self.ahead.get() {
            for cell in ahead.iter() {
                // SAFETY: the stream goes with its inode's state, which no reader holds any more.
                unsafe {
                    let old = cell.swap(Shared::null(), Ordering::Relaxed, epoch::unprotected());
                    if !old.is_null() {
                        drop(old.into_owned());
                    }
                }
            }
        }
    }
}

impl Stream {
    /// Records a READ of `offset..end`, waking the task once the stream reaches where it waits;
    /// answers whether the stream has run long enough to prefetch for. A READ starting where the
    /// stream ended extends the run, one within the slack of it keeps it, any other breaks it.
    /// Lock-free and allocation-free.
    #[lore_macro::test_pub]
    pub(super) fn note(&self, offset: u64, end: u64) -> bool {
        let reorder = 2 * (end - offset);
        if self.reorder.load(Ordering::Relaxed) != reorder {
            self.reorder.store(reorder, Ordering::Relaxed);
        }
        let previous = self.cursor.load(Ordering::Relaxed);
        if offset == previous {
            self.run.fetch_add(1, Ordering::Relaxed);
        } else if offset.abs_diff(previous) > reorder.max(SLACK) {
            self.cursor.store(end, Ordering::SeqCst);
            self.run.store(0, Ordering::Relaxed);
            if self.active.load(Ordering::Relaxed) {
                self.generation.fetch_add(1, Ordering::SeqCst);
                self.waker.wake();
            }
            return false;
        }
        let reached = self.cursor.fetch_max(end, Ordering::SeqCst).max(end);
        if reached >= self.wake_at.load(Ordering::SeqCst) {
            self.waker.wake();
        }
        self.run.load(Ordering::Relaxed) >= SEQUENTIAL_READS
    }

    /// How far behind where it ended the stream may still be reading: a READ moves the cursor
    /// to its end as it starts.
    fn behind(&self) -> u64 {
        self.reorder.load(Ordering::Relaxed)
    }

    /// Claims the stream for a new task; false while one serves it.
    #[lore_macro::test_pub]
    fn claim(&self) -> bool {
        !self.active.load(Ordering::Relaxed)
            && self
                .active
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
    }

    /// Stops the task, if any.
    #[lore_macro::test_pub]
    pub(super) fn stop(&self) {
        if self.active.load(Ordering::SeqCst) {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.waker.wake();
        }
    }

    /// The ahead cells, once a task has kept leaves in them.
    fn cells(&self) -> &[Cell] {
        match self.ahead.get() {
            Some(ahead) if self.streamed.load(Ordering::SeqCst) => &ahead[..],
            _ => &[],
        }
    }

    /// The prefetched leaf `hash` with [`child_key`] `key`: one probe of one cell.
    pub(super) fn find<'g>(
        &'g self,
        hash: Hash,
        key: usize,
        guard: &'g epoch::Guard,
    ) -> Option<&'g Cached> {
        cell_get(&self.ahead.get()?[key % AHEAD], hash, guard)
    }

    fn current(&self, generation: u32) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    /// Waits until the stream reaches `wake_at`, breaks, or `slot` is released.
    #[lore_macro::test_pub]
    async fn wait(&self, slot: &InodeSlot, generation: u32, wake_at: u64) {
        self.wake_at.store(wake_at, Ordering::SeqCst);
        poll_fn(|cx| {
            self.waker.register(cx.waker());
            if !self.current(generation)
                || !slot.live()
                || self.cursor.load(Ordering::SeqCst) >= wake_at
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        self.wake_at.store(u64::MAX, Ordering::SeqCst);
    }
}

impl InodeSlot {
    /// The inode's stream, once a READ with prefetch on allocated it.
    pub(super) fn stream(&self) -> Option<&Stream> {
        // SAFETY: the pointer is set once, to a stream freed only when this slot drops.
        unsafe { self.stream.load(Ordering::Acquire).as_ref() }
    }

    /// Frees the stream with the slot.
    pub(super) fn drop_stream(&mut self) {
        let stream = *self.stream.get_mut();
        if !stream.is_null() {
            // SAFETY: the slot is being dropped, so nothing borrows its stream any more.
            drop(unsafe { Box::from_raw(stream) });
        }
    }
}

/// What a prefetch task keeps between batches.
struct Plan {
    /// Where the leaf in each ahead cell ends; 0 for an empty cell.
    ends: [u64; AHEAD],
    /// The lists the task loaded, per depth. They stay the task's own, so the lists a READ keeps
    /// in the inode's state are never displaced by speculation.
    lists: [Option<(Hash, Bytes)>; MAX_DEPTH + 1],
}

/// What a batch did.
struct Batch {
    /// Where the leaves it loaded or found held reach.
    reached: u64,
    /// Where the leaf in a cell the batch needed ends, when that cell is not yet free.
    blocked: Option<u64>,
    /// Whether the budget refused a leaf.
    cut: bool,
    /// Whether a child taken for a leaf turned out to be a list, to walk at once.
    again: bool,
}

impl FragmentCache {
    /// The stream of `slot`, allocated and counted on first use.
    pub(super) fn stream_of<'s>(&self, slot: &'s InodeSlot) -> &'s Stream {
        match slot.stream() {
            Some(stream) => stream,
            None => self.install_stream(slot),
        }
    }

    #[cold]
    fn install_stream<'s>(&self, slot: &'s InodeSlot) -> &'s Stream {
        let bytes = size_of::<Stream>();
        self.hold_inode_bytes(bytes);
        let stream = Stream::default();
        stream.counted.store(bytes, Ordering::Relaxed);
        let fresh = Box::into_raw(Box::new(stream));
        match slot.stream.compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::SeqCst,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: `fresh` is now the slot's, freed only when the slot drops.
                let stream = unsafe { &*fresh };
                if !slot.live() {
                    self.release_counted(stream);
                }
                stream
            }
            Err(current) => {
                // SAFETY: `fresh` never left this function; `current` is the slot's, as above.
                drop(unsafe { Box::from_raw(fresh) });
                self.release_inode_bytes(bytes);
                unsafe { &*current }
            }
        }
    }

    /// The ring of `stream`, allocated and counted on first use; only a task calls this.
    fn ring<'s>(&self, slot: &InodeSlot, stream: &'s Stream) -> &'s Ring {
        if let Some(ring) = stream.ahead.get() {
            return ring;
        }
        let bytes = size_of::<Ring>();
        self.hold_inode_bytes(bytes);
        stream.counted.fetch_add(bytes, Ordering::SeqCst);
        let ring = stream
            .ahead
            .get_or_init(|| Box::new(std::array::from_fn(|_| Cell::default())));
        if !slot.live() {
            self.release_counted(stream);
        }
        ring
    }

    fn release_counted(&self, stream: &Stream) {
        self.release_inode_bytes(stream.counted.swap(0, Ordering::SeqCst));
    }

    fn release_ahead_bytes(&self, bytes: usize) {
        self.ahead_held.fetch_sub(bytes, Ordering::SeqCst);
        self.release_inode_bytes(bytes);
    }

    /// Stops `slot`'s task and takes out what its stream holds; `slot` is marked released.
    pub(super) fn release_stream(&self, slot: &InodeSlot, guard: &epoch::Guard) {
        if let Some(stream) = slot.stream() {
            stream.stop();
            let ahead = stream
                .cells()
                .iter()
                .map(|cell| cell_swap(cell, None, guard))
                .sum();
            self.release_ahead_bytes(ahead);
            self.release_counted(stream);
        }
    }

    /// Starts a task prefetching ahead of the stream over `slot`, whose READ ended at `end`
    /// having loaded all it needed locally.
    pub(super) fn start_stream(
        &self,
        slot: &Arc<InodeSlot>,
        repository: &Arc<RepositoryContext>,
        address: Address,
        size: u64,
        end: u64,
    ) {
        let Some(stream) = slot.stream() else {
            return;
        };
        if end >= size || self.this.strong_count() == 0 || !stream.claim() {
            return;
        }
        let generation = stream.generation.load(Ordering::SeqCst);
        #[cfg(feature = "test-util")]
        {
            self.counters.streams.fetch_add(1, Ordering::SeqCst);
            self.counters.running.fetch_add(1, Ordering::SeqCst);
        }
        let _task = lore_base::lore_spawn!(run(
            self.this.clone(),
            Arc::clone(slot),
            Arc::clone(repository),
            address,
            size,
            generation,
            end,
        ));
    }

    /// Empties the cell `cell` of the ring and forgets its leaf.
    fn free_cell(&self, stream: &Stream, plan: &mut Plan, cell: usize, guard: &epoch::Guard) {
        if let Some(ahead) = stream.ahead.get() {
            self.release_ahead_bytes(cell_swap(&ahead[cell], None, guard));
        }
        plan.ends[cell] = 0;
    }

    /// Empties the cells whose leaves end at or before `behind`, or every cell with `u64::MAX`.
    fn free_ahead(&self, stream: &Stream, plan: &mut Plan, behind: u64) {
        let guard = epoch::pin();
        for cell in 0..AHEAD {
            let end = plan.ends[cell];
            if behind == u64::MAX || (end != 0 && end <= behind) {
                self.free_cell(stream, plan, cell, &guard);
            }
        }
    }

    /// Keeps a prefetched leaf in its empty `cell` if it fits the inode budget and the share of
    /// it prefetched leaves may hold, evicting nothing.
    fn keep_ahead(&self, slot: &InodeSlot, stream: &Stream, cell: &Cell, cached: Cached) -> bool {
        let len = cached.payload.len();
        let ahead = self.ahead_held.fetch_add(len, Ordering::SeqCst) + len;
        if ahead > self.inode_budget / AHEAD_SHARE
            || self.inode_held.load(Ordering::SeqCst) + len > self.inode_budget
        {
            self.ahead_held.fetch_sub(len, Ordering::SeqCst);
            return false;
        }
        stream.streamed.store(true, Ordering::SeqCst);
        let out = self.swap_in(slot, cell, cached);
        self.release_ahead_bytes(out);
        #[cfg(feature = "test-util")]
        self.counters.prefetched.fetch_add(len, Ordering::SeqCst);
        true
    }

    /// Prefetches the leaves of `address` over `from..to` not already held into their cells.
    #[allow(clippy::too_many_arguments)]
    async fn prefetch(
        &self,
        repository: &Arc<RepositoryContext>,
        slot: &InodeSlot,
        stream: &Stream,
        address: Address,
        size: u64,
        from: u64,
        to: u64,
        plan: &mut Plan,
    ) -> Result<Batch, StorageError> {
        let mut options = crate::immutable::read_options_from_repository(repository);
        options.remote = false;
        let walk = Walk {
            cache: self,
            repository,
            store: repository.immutable_store_ref(),
            partition: repository.id,
            context: address.context,
            options,
            strategy: self.strategy(),
            ahead: false,
            prefetching: true,
        };
        let (jobs, cells, mut batch) = walk.plan(slot, plan, address.hash, size, from, to).await?;
        #[cfg(feature = "test-util")]
        {
            let cursor = stream.cursor.load(Ordering::SeqCst);
            let behind = jobs.iter().filter(|job| job.end <= cursor).count();
            self.counters.behind.fetch_add(behind, Ordering::SeqCst);
        }
        if jobs.is_empty() {
            return Ok(batch);
        }
        let ring = self.ring(slot, stream);
        for (job, fetched) in walk.fetch_all(&jobs).await? {
            let at = jobs
                .iter()
                .position(|queued| ptr::eq(queued, job))
                .unwrap_or_default();
            match fetched {
                Fetched::Leaf(fragment, payload) => {
                    let cell = cells[at];
                    let cached = Cached {
                        hash: job.hash,
                        fragment,
                        payload,
                    };
                    if batch.cut || !self.keep_ahead(slot, stream, &ring[cell], cached) {
                        batch.cut = true;
                        batch.reached = batch.reached.min(job.start);
                        continue;
                    }
                    plan.ends[cell] = job.end;
                }
                Fetched::List(fragment, list) => {
                    let parent = FragmentReference {
                        hash: job.hash,
                        offset_content: job.offset_content,
                    };
                    check_sublist(&list, &fragment, &parent, &job.span())?;
                    plan.lists[job.depth] = Some((job.hash, list));
                    batch.reached = batch.reached.min(job.start);
                    batch.again = true;
                }
            }
        }
        Ok(batch)
    }
}

/// A stream's prefetch task: loads batches of leaves ahead of the stream, each once the stream
/// has used half the window the last batch was planned with. It ends once the stream has read to
/// the end of the file, or when the stream breaks, a load fails, or its inode's state goes. It
/// then frees its leaves, since the page cache serves what the kernel reads again, and resets the
/// run, so the stream's later READs start no task until they run again. At the end of the file it
/// keeps the leaves the READs still in flight may read, within [`Stream::behind`] of the end; the
/// next stream's leaves displace them, or the release of the inode's state frees them. Holds the
/// cache only while loading, so a dropped cache ends it.
async fn run(
    cache: Weak<FragmentCache>,
    slot: Arc<InodeSlot>,
    repository: Arc<RepositoryContext>,
    address: Address,
    size: u64,
    generation: u32,
    from: u64,
) {
    if let Some(stream) = slot.stream() {
        serve_stream(
            &cache,
            &slot,
            stream,
            &repository,
            address,
            size,
            generation,
            from,
        )
        .await;
    }
    #[cfg(feature = "test-util")]
    if let Some(cache) = cache.upgrade() {
        cache.counters.running.fetch_sub(1, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_stream(
    cache: &Weak<FragmentCache>,
    slot: &InodeSlot,
    stream: &Stream,
    repository: &Arc<RepositoryContext>,
    address: Address,
    size: u64,
    generation: u32,
    from: u64,
) {
    let mut plan = Plan {
        ends: [0; AHEAD],
        lists: Default::default(),
    };
    let mut next = from;
    let mut window = FIRST_WINDOW;
    let mut keep_from = u64::MAX;
    while stream.current(generation) && slot.live() {
        let cursor = stream.cursor.load(Ordering::SeqCst);
        if cursor >= size {
            keep_from = cursor.saturating_sub(stream.behind());
            break;
        }
        let Some(strong) = cache.upgrade() else {
            break;
        };
        let retain = stream.behind();
        strong.free_ahead(stream, &mut plan, cursor.saturating_sub(retain));
        let from = next.max(cursor);
        let to = size.min(cursor.saturating_add(window));
        let mut wake_at = if next < size {
            next.saturating_sub(window / 2)
        } else {
            size
        };
        if from < to {
            let Ok(batch) = strong
                .prefetch(repository, slot, stream, address, size, from, to, &mut plan)
                .await
            else {
                break;
            };
            next = batch.reached;
            wake_at = match batch.blocked {
                Some(end) => end.saturating_add(retain),
                None if next < size => next.saturating_sub(window / 2),
                None => size,
            };
            window = if batch.cut {
                FIRST_WINDOW
            } else {
                (window * 2).min(MAX_WINDOW)
            };
            if batch.again {
                continue;
            }
        }
        drop(strong);
        stream
            .wait(slot, generation, wake_at.clamp(cursor + 1, size))
            .await;
    }
    if let Some(strong) = cache.upgrade() {
        strong.free_ahead(stream, &mut plan, keep_from);
    }
    stream.run.store(0, Ordering::Relaxed);
    stream.active.store(false, Ordering::SeqCst);
}

impl Walk<'_> {
    /// The leaves over `from..to` held nowhere yet, whole and in order, with the ahead cell each
    /// goes to, stopping at a cell [`FragmentCache::free_ahead`] has not emptied. A child spanning
    /// more than one fragment's content is a list.
    ///
    /// This walks the lists as [`Walk::serve`] does, apart from it: `serve` is synchronous and
    /// writes the reply in place, while this awaits the lists it loads and keeps them to itself.
    /// One step function for both would put a branch per level on the READ path.
    #[allow(clippy::too_many_arguments)]
    async fn plan(
        &self,
        slot: &InodeSlot,
        plan: &mut Plan,
        root: Hash,
        size: u64,
        from: u64,
        to: u64,
    ) -> Result<(Vec<Job>, Vec<usize>, Batch), StorageError> {
        let mut jobs: Vec<Job> = Vec::new();
        let mut cells: Vec<usize> = Vec::new();
        let whole = Span {
            from,
            to,
            start: 0,
            end: size,
            depth: 0,
            key: 0,
        };
        let root_list = self.list(slot, plan, root, &whole, None).await?;
        let mut pos = from;
        'leaves: while pos < to {
            let mut list = root_list.clone();
            let (mut node_start, mut node_end, mut depth, mut key) = (0, size, 0, 0);
            loop {
                let refs = list.as_type_slice::<FragmentReference>();
                let (index, child_start, child_end) = locate(refs, node_start, node_end, pos)
                    .ok_or_else(|| {
                        StorageError::internal("fragment list does not cover the read")
                    })?;
                let reference = refs[index];
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(StorageError::internal("fragment lists nest too deep"));
                }
                key = child_key(key, index);
                let known = held_list(slot, plan, depth, reference.hash);
                if known.is_some() || child_end - child_start > FRAGMENT_SIZE_THRESHOLD as u64 {
                    let span = Span {
                        from: pos,
                        to: to.min(child_end),
                        start: child_start,
                        end: child_end,
                        depth,
                        key,
                    };
                    list = match known {
                        Some(list) => list,
                        None => {
                            self.list(slot, plan, reference.hash, &span, Some(&reference))
                                .await?
                        }
                    };
                    (node_start, node_end) = (child_start, child_end);
                    continue;
                }
                let guard = epoch::pin();
                if slot
                    .find(depth, reference.hash, key, &guard, true)
                    .is_none()
                {
                    let cell = key % AHEAD;
                    let queued = cells.iter().position(|&taken| taken == cell);
                    let end = queued.map_or(plan.ends[cell], |at| jobs[at].end);
                    if end != 0 {
                        let batch = Batch {
                            reached: child_start,
                            blocked: Some(end),
                            cut: false,
                            again: false,
                        };
                        return Ok((jobs, cells, batch));
                    }
                    jobs.push(Job {
                        hash: reference.hash,
                        offset_content: reference.offset_content,
                        depth,
                        local: 0,
                        from: child_start,
                        to: child_end,
                        start: child_start,
                        end: child_end,
                        key,
                    });
                    cells.push(cell);
                }
                pos = child_end;
                continue 'leaves;
            }
        }
        let batch = Batch {
            reached: pos,
            blocked: None,
            cut: false,
            again: false,
        };
        Ok((jobs, cells, batch))
    }

    /// The list `hash` spanning `span`, held or loaded and kept by the task; `parent` names it
    /// in the list above, and is `None` for the root.
    async fn list(
        &self,
        slot: &InodeSlot,
        plan: &mut Plan,
        hash: Hash,
        span: &Span,
        parent: Option<&FragmentReference>,
    ) -> Result<Bytes, StorageError> {
        if let Some(list) = held_list(slot, plan, span.depth, hash) {
            return Ok(list);
        }
        #[cfg(feature = "test-util")]
        self.count_loads(1);
        let fetched = super::fetch(
            self.store.clone(),
            self.partition,
            self.address(hash),
            self.load_options(),
            None,
        )
        .await?;
        let Fetched::List(fragment, list) = fetched else {
            return Err(StorageError::internal(
                "a child wider than a fragment is not a list",
            ));
        };
        match parent {
            Some(parent) => check_sublist(&list, &fragment, parent, span)?,
            None => check_list(&list, &fragment, None, span.from..span.to)?,
        }
        plan.lists[span.depth] = Some((hash, list.clone()));
        Ok(list)
    }
}

/// The list `hash` at `depth`, if the inode's state or the task holds it.
fn held_list(slot: &InodeSlot, plan: &Plan, depth: usize, hash: Hash) -> Option<Bytes> {
    if let Some((held, list)) = &plan.lists[depth]
        && *held == hash
    {
        return Some(list.clone());
    }
    let guard = epoch::pin();
    let cell = slot.lists.get(depth)?;
    cell_get(cell, hash, &guard)
        .filter(|cached| is_list(&cached.fragment))
        .map(|cached| cached.payload.clone())
}

/// Prefetch counters, for tests.
#[cfg(feature = "test-util")]
#[derive(Default)]
pub struct Counters {
    streams: std::sync::atomic::AtomicUsize,
    pub running: std::sync::atomic::AtomicUsize,
    read_loads: std::sync::atomic::AtomicUsize,
    prefetch_loads: std::sync::atomic::AtomicUsize,
    prefetched: std::sync::atomic::AtomicUsize,
    used: std::sync::atomic::AtomicUsize,
    behind: std::sync::atomic::AtomicUsize,
}

/// A stream's position, for tests.
#[cfg(feature = "test-util")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamState {
    pub cursor: u64,
    pub run: u32,
    pub generation: u32,
    pub active: bool,
}

#[cfg(feature = "test-util")]
impl FragmentCache {
    pub fn prefetch_counts(&self) -> PrefetchCounts {
        let c = &self.counters;
        let load = |counter: &std::sync::atomic::AtomicUsize| counter.load(Ordering::SeqCst);
        PrefetchCounts {
            streams: load(&c.streams),
            read_loads: load(&c.read_loads),
            prefetch_loads: load(&c.prefetch_loads),
            prefetched: load(&c.prefetched),
            used: load(&c.used),
            behind: load(&c.behind),
        }
    }

    pub fn stream_state(&self, ino: u64) -> Option<StreamState> {
        let slot = self.inodes.get(&ino)?;
        let stream = slot.stream()?;
        Some(StreamState {
            cursor: stream.cursor.load(Ordering::SeqCst),
            run: stream.run.load(Ordering::SeqCst),
            generation: stream.generation.load(Ordering::SeqCst),
            active: stream.active.load(Ordering::SeqCst),
        })
    }

    /// Bytes `ino`'s stream state counts beyond its prefetched leaves.
    pub fn stream_bytes(&self, ino: u64) -> usize {
        self.inodes
            .get(&ino)
            .and_then(|slot| {
                slot.stream()
                    .map(|stream| stream.counted.load(Ordering::SeqCst))
            })
            .unwrap_or_default()
    }

    /// Bytes prefetched leaves hold.
    pub fn ahead_bytes(&self) -> usize {
        self.ahead_held.load(Ordering::SeqCst)
    }

    /// Whether every prefetch task waits for a READ, or has ended.
    pub fn prefetch_settled(&self) -> bool {
        let waiting = self
            .inodes
            .iter()
            .filter(|slot| {
                let Some(stream) = slot.stream() else {
                    return false;
                };
                let wake_at = stream.wake_at.load(Ordering::SeqCst);
                stream.active.load(Ordering::SeqCst)
                    && wake_at != u64::MAX
                    && stream.cursor.load(Ordering::SeqCst) < wake_at
            })
            .count();
        waiting == self.counters.running.load(Ordering::SeqCst)
    }
}

#[cfg(feature = "test-util")]
impl Walk<'_> {
    pub(super) fn count_loads(&self, loads: usize) {
        let counters = &self.cache.counters;
        let counter = if self.prefetching {
            &counters.prefetch_loads
        } else {
            &counters.read_loads
        };
        counter.fetch_add(loads, Ordering::SeqCst);
    }

    /// Counts `len` bytes a READ copied out of a prefetched leaf.
    pub(super) fn count_used(
        &self,
        slot: Option<&InodeSlot>,
        hash: Hash,
        key: usize,
        len: usize,
        guard: &epoch::Guard,
    ) {
        if self.ahead
            && let Some(stream) = slot.and_then(InodeSlot::stream)
            && stream.find(hash, key, guard).is_some()
        {
            self.cache.counters.used.fetch_add(len, Ordering::SeqCst);
        }
    }
}

/// What prefetching did, for tests.
#[cfg(feature = "test-util")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrefetchCounts {
    /// Prefetch tasks started.
    pub streams: usize,
    /// Fragments READs loaded themselves.
    pub read_loads: usize,
    /// Fragments prefetch tasks loaded.
    pub prefetch_loads: usize,
    /// Bytes prefetch tasks kept.
    pub prefetched: usize,
    /// Prefetched bytes READs copied out.
    pub used: usize,
    /// Leaves prefetch loaded that the stream had already passed.
    pub behind: usize,
}
