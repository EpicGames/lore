// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Fragment caching for file READs: which fragment lists and leaves a READ may reuse from an
//! earlier one, selected by [`FragmentCacheStrategy`].
//!
//! Content is immutable per root address, so nothing cached under a root goes stale. State is
//! keyed by inode, checked against the inode's current root address, and dropped on FORGET or when
//! the budget needs the room.

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use crossbeam_epoch as epoch;
use crossbeam_epoch::Atomic;
use crossbeam_epoch::Owned;
use crossbeam_epoch::Shared;
use dashmap::DashMap;
use dashmap::mapref::one::Ref;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Fragment;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::typed_bytes::TypedBytes as _;
use lore_error_set::prelude::*;
use lore_storage::FragmentFlags;
use lore_storage::ImmutableStore;
use lore_storage::ReadOptions;
use lore_storage::StorageError;
use lore_transport::StorageSession;
use smallvec::SmallVec;
use tokio::task::JoinSet;

use crate::fs::fuse::clock;
use crate::fs::fuse::clock::Clock;
use crate::fs::fuse::clock::Visit;
use crate::repository::RepositoryContext;

#[cfg(not(feature = "test-util"))]
mod prefetch;
#[cfg(feature = "test-util")]
pub mod prefetch;

/// The strategy a provider starts with.
pub const DEFAULT_FRAGMENT_CACHE: FragmentCacheStrategy =
    FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(4));

/// List levels kept per inode: the root and the sub-lists below it.
const LEVELS: usize = 4;
/// Most leaves any [`LeafPolicy`] keeps per inode.
pub const MAX_LEAVES: usize = 4;
/// What all per-inode state may hold together, by default.
pub const INODE_BUDGET: usize = 64 << 20;
/// Deeper lists than this are corrupt.
const MAX_DEPTH: usize = 16;
/// A displaced payload this large is handed to the epoch collector at once: a thread's deferred
/// frees otherwise wait in a bag of 64 until it fills, outside the budget that no longer counts
/// them.
const FLUSH_DISPLACED: usize = 64 << 10;

/// How the leaves a READ touches are kept for the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafPolicy {
    /// Every READ loads the leaves it touches.
    Load,
    /// The last `n` leaves per inode, decoded; `n` is clamped to [`MAX_LEAVES`].
    Decoded(u8),
    /// The last `n` leaves per inode as stored; each READ decodes a compressed leaf whole and
    /// copies its range into the reply.
    /// `n` is clamped to [`MAX_LEAVES`].
    Stored(u8),
}

/// What a READ may reuse from an earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentCacheStrategy {
    /// The storage walk into an intermediate buffer, reloading every list and leaf.
    Storage,
    /// The provider's walk, keeping each inode's current list per level and leaves per the
    /// policy.
    PerInode(LeafPolicy),
    /// The provider's walk over one byte-budgeted cache of lists and leaves keyed by address.
    Global { budget: usize, decoded: bool },
}

impl FragmentCacheStrategy {
    /// Whether leaves load decoded rather than as stored.
    fn decodes_on_load(self) -> bool {
        match self {
            FragmentCacheStrategy::PerInode(LeafPolicy::Stored(_)) => false,
            FragmentCacheStrategy::Global { decoded, .. } => decoded,
            _ => true,
        }
    }

    /// Leaves kept per inode, clamped to [`MAX_LEAVES`].
    #[lore_macro::test_pub]
    fn leaves_kept(self) -> usize {
        match self {
            FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(n) | LeafPolicy::Stored(n)) => {
                usize::from(n).min(MAX_LEAVES)
            }
            _ => 0,
        }
    }
}

/// A list or leaf as a READ reuses it: shared, so a hit is a borrow or a refcount, never a copy.
#[lore_macro::test_pub]
struct Cached {
    hash: Hash,
    fragment: Fragment,
    payload: Bytes,
}

#[lore_macro::test_pub]
fn is_list(fragment: &Fragment) -> bool {
    fragment.flags & FragmentFlags::PayloadFragmented != 0
}

/// One cached entry, swapped whole: a hit pins the epoch and loads the pointer, taking no lock
/// and writing nothing shared; a replaced entry is freed once no pinned reader can see it.
type Cell = Atomic<Cached>;

/// `cell`'s entry if it holds `hash`, borrowed while `guard` pins it.
fn cell_get<'g>(cell: &'g Cell, hash: Hash, guard: &'g epoch::Guard) -> Option<&'g Cached> {
    // SAFETY: the pointer is loaded under `guard`, and entries are only freed through
    // `defer_destroy`, after every guard that could see them is gone.
    let cached = unsafe { cell.load(Ordering::Acquire, guard).as_ref() }?;
    (cached.hash == hash).then_some(cached)
}

/// Swaps `cached` into `cell` (or empties it) under `guard`, answering the payload bytes
/// displaced. A large displaced payload is handed to the collector at once rather than left in
/// this thread's bag.
fn cell_swap(cell: &Cell, cached: Option<Cached>, guard: &epoch::Guard) -> usize {
    let new = match cached {
        Some(cached) => Owned::new(cached).into_shared(guard),
        None => Shared::null(),
    };
    let old = cell.swap(new, Ordering::AcqRel, guard);
    // SAFETY: `old` was just unlinked, so no later load returns it; pinned readers keep it alive
    // until they unpin.
    match unsafe { old.as_ref() } {
        Some(displaced) => {
            let len = displaced.payload.len();
            unsafe { guard.defer_destroy(old) };
            if len >= FLUSH_DISPLACED {
                guard.flush();
            }
            len
        }
        None => 0,
    }
}

/// A cached list or leaf: borrowed from per-inode state while the epoch is pinned, or shared out
/// of the global cache.
enum Hit<'g> {
    Borrowed(&'g Cached),
    Shared(Fragment, Bytes),
}

impl Hit<'_> {
    fn fragment(&self) -> Fragment {
        match self {
            Hit::Borrowed(cached) => cached.fragment,
            Hit::Shared(fragment, _) => *fragment,
        }
    }

    fn payload(&self) -> &Bytes {
        match self {
            Hit::Borrowed(cached) => &cached.payload,
            Hit::Shared(_, payload) => payload,
        }
    }
}

#[lore_macro::test_pub]
struct InodeSlot {
    ino: u64,
    root: Hash,
    /// Read since the clock hand last passed.
    referenced: AtomicBool,
    evicted: AtomicBool,
    lists: [Cell; LEVELS],
    leaves: [Cell; MAX_LEAVES],
    next_leaf: AtomicUsize,
    /// The sequential READ stream, allocated on the first READ with prefetch on.
    stream: AtomicPtr<prefetch::Stream>,
}

impl InodeSlot {
    #[lore_macro::test_pub]
    fn new(ino: u64, root: Hash) -> Self {
        Self {
            ino,
            root,
            referenced: AtomicBool::new(false),
            evicted: AtomicBool::new(false),
            lists: Default::default(),
            leaves: Default::default(),
            next_leaf: AtomicUsize::new(0),
            stream: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// The list or leaf `hash` at `depth` with [`child_key`] `key`, searching the prefetched
    /// leaves too when `ahead`.
    fn find<'g>(
        &'g self,
        depth: usize,
        hash: Hash,
        key: usize,
        guard: &'g epoch::Guard,
        ahead: bool,
    ) -> Option<&'g Cached> {
        self.lists
            .get(depth)
            .and_then(|cell| cell_get(cell, hash, guard))
            .or_else(|| {
                self.leaves
                    .iter()
                    .find_map(|cell| cell_get(cell, hash, guard))
            })
            .or_else(|| {
                ahead
                    .then(|| self.stream()?.find(hash, key, guard))
                    .flatten()
            })
    }

    fn cells(&self) -> impl Iterator<Item = &Cell> {
        self.lists.iter().chain(self.leaves.iter())
    }

    fn live(&self) -> bool {
        !self.evicted.load(Ordering::SeqCst)
    }
}

impl Drop for InodeSlot {
    fn drop(&mut self) {
        for cell in self.cells() {
            // SAFETY: the last reference is being dropped, so no reader holds this slot.
            unsafe {
                let old = cell.swap(Shared::null(), Ordering::Relaxed, epoch::unprotected());
                if !old.is_null() {
                    drop(old.into_owned());
                }
            }
        }
        self.drop_stream();
    }
}

/// What a slot costs beyond the payloads it holds; a stream and its ring are counted as they are
/// allocated.
#[lore_macro::test_pub]
const SLOT_OVERHEAD: usize = size_of::<InodeSlot>() + 64;

#[lore_macro::test_pub]
struct GlobalEntry {
    fragment: Fragment,
    payload: Bytes,
    referenced: AtomicBool,
}

/// Lists and leaves by hash, evicted by CLOCK. A hit reads one shard and marks the entry; the
/// clock hand is a lock-free queue only inserts touch.
#[lore_macro::test_pub]
#[derive(Default)]
struct GlobalCache {
    entries: DashMap<Hash, GlobalEntry>,
    clock: Clock<Hash>,
    bytes: AtomicUsize,
}

impl GlobalCache {
    fn get(&self, hash: &Hash) -> Option<(Fragment, Bytes)> {
        let entry = self.entries.get(hash)?;
        clock::mark(&entry.referenced);
        Some((entry.fragment, entry.payload.clone()))
    }

    /// Inserts within `budget`, answering the bytes added and freed. Nothing is added when the
    /// hand cannot make room: another thread is evicting, or every entry was read since it last
    /// passed.
    fn insert(
        &self,
        hash: Hash,
        fragment: Fragment,
        payload: Bytes,
        budget: usize,
    ) -> (usize, usize) {
        let len = payload.len();
        if len > budget || self.entries.contains_key(&hash) {
            return (0, 0);
        }
        let over = || self.bytes.load(Ordering::Relaxed) + len > budget;
        let mut freed = 0;
        self.clock.evict(over, |victim| {
            clock::visit_keyed(
                &self.entries,
                victim,
                |entry| &entry.referenced,
                |entry| {
                    self.bytes.fetch_sub(entry.payload.len(), Ordering::Relaxed);
                    freed += entry.payload.len();
                },
            )
        });
        if over() {
            return (0, freed);
        }
        let displaced = self.entries.insert(
            hash,
            GlobalEntry {
                fragment,
                payload,
                referenced: AtomicBool::new(false),
            },
        );
        self.bytes.fetch_add(len, Ordering::Relaxed);
        // A concurrent insert of the same hash: one of the two is counted out again.
        if let Some(old) = displaced {
            self.bytes.fetch_sub(old.payload.len(), Ordering::Relaxed);
            return (len, freed + old.payload.len());
        }
        self.clock.push(hash);
        (len, freed)
    }

    fn clear(&self) -> usize {
        self.entries.clear();
        self.clock.clear();
        self.bytes.swap(0, Ordering::Relaxed)
    }
}

/// A [`FragmentCacheStrategy`] a READ reads with two atomic loads.
#[lore_macro::test_pub]
struct StrategyCell {
    code: AtomicU32,
    budget: AtomicUsize,
}

impl StrategyCell {
    fn new(strategy: FragmentCacheStrategy) -> Self {
        let cell = Self {
            code: AtomicU32::new(0),
            budget: AtomicUsize::new(0),
        };
        cell.store(strategy);
        cell
    }

    fn store(&self, strategy: FragmentCacheStrategy) {
        let (code, budget) = match strategy {
            FragmentCacheStrategy::Storage => (0, 0),
            FragmentCacheStrategy::PerInode(policy) => {
                let (kind, n) = match policy {
                    LeafPolicy::Load => (0, 0),
                    LeafPolicy::Decoded(n) => (1, n),
                    LeafPolicy::Stored(n) => (2, n),
                };
                (2 | kind << 8 | u32::from(n) << 16, 0)
            }
            FragmentCacheStrategy::Global { budget, decoded } => {
                (3 | u32::from(decoded) << 8, budget)
            }
        };
        self.budget.store(budget, Ordering::Relaxed);
        self.code.store(code, Ordering::Release);
    }

    fn load(&self) -> FragmentCacheStrategy {
        let code = self.code.load(Ordering::Acquire);
        let n = (code >> 16) as u8;
        match code & 0xff {
            0 => FragmentCacheStrategy::Storage,
            2 => FragmentCacheStrategy::PerInode(match (code >> 8) & 0xff {
                0 => LeafPolicy::Load,
                1 => LeafPolicy::Decoded(n),
                _ => LeafPolicy::Stored(n),
            }),
            _ => FragmentCacheStrategy::Global {
                budget: self.budget.load(Ordering::Relaxed),
                decoded: (code >> 8) & 1 == 1,
            },
        }
    }
}

#[lore_macro::test_pub]
pub struct FragmentCache {
    strategy: StrategyCell,
    inodes: DashMap<u64, Arc<InodeSlot>>,
    /// The CLOCK hand over per-inode state, oldest first. Entries of released slots are dropped
    /// as the hand or a FORGET reaches them.
    clock: Clock<Weak<InodeSlot>>,
    global: GlobalCache,
    inode_budget: usize,
    inode_held: AtomicUsize,
    held: AtomicUsize,
    peak: AtomicUsize,
    /// Whether sequential READs prefetch ahead.
    prefetch: AtomicBool,
    /// What prefetched leaves hold, within `inode_held`.
    ahead_held: AtomicUsize,
    /// The cache itself once shared, for the prefetch tasks it starts.
    this: Weak<FragmentCache>,
    #[cfg(feature = "test-util")]
    evictions: AtomicUsize,
    #[cfg(feature = "test-util")]
    visits: AtomicUsize,
    #[cfg(feature = "test-util")]
    counters: prefetch::Counters,
}

impl FragmentCache {
    pub fn new(strategy: FragmentCacheStrategy) -> Self {
        Self {
            strategy: StrategyCell::new(strategy),
            inodes: DashMap::new(),
            clock: Clock::default(),
            global: GlobalCache::default(),
            inode_budget: INODE_BUDGET,
            inode_held: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            prefetch: AtomicBool::new(false),
            ahead_held: AtomicUsize::new(0),
            this: Weak::new(),
            #[cfg(feature = "test-util")]
            evictions: AtomicUsize::new(0),
            #[cfg(feature = "test-util")]
            visits: AtomicUsize::new(0),
            #[cfg(feature = "test-util")]
            counters: prefetch::Counters::default(),
        }
    }

    /// Caps the bytes all per-inode state holds together.
    pub fn with_inode_budget(mut self, budget: usize) -> Self {
        self.inode_budget = budget;
        self
    }

    /// The cache shared, as the prefetch tasks it starts need; unshared, it never prefetches.
    pub fn into_shared(self) -> Arc<Self> {
        Arc::new_cyclic(|this| {
            let mut cache = self;
            cache.this = this.clone();
            cache
        })
    }

    /// Switches prefetching ahead of sequential READs, which applies under the per-inode
    /// strategies only; off by default.
    pub fn set_sequential_prefetch(&self, on: bool) {
        self.prefetch.store(on, Ordering::Relaxed);
    }

    pub fn strategy(&self) -> FragmentCacheStrategy {
        self.strategy.load()
    }

    /// Switches strategy, dropping everything held. For an idle provider: READs in flight may
    /// finish under the old strategy.
    pub fn set_strategy(&self, strategy: FragmentCacheStrategy) {
        self.strategy.store(strategy);
        let inodes: Vec<u64> = self.inodes.iter().map(|entry| *entry.key()).collect();
        for ino in inodes {
            self.forget(ino);
        }
        self.clock.clear();
        let freed = self.global.clear();
        self.held.fetch_sub(freed, Ordering::Relaxed);
    }

    /// Drops what `ino` holds; its content is unreachable once the kernel forgets it.
    pub fn forget(&self, ino: u64) {
        if let Some((_, slot)) = self.inodes.remove(&ino) {
            self.release_slot(&slot);
            self.trim_hand();
        }
    }

    fn release_slot(&self, slot: &InodeSlot) {
        slot.evicted.store(true, Ordering::SeqCst);
        let guard = epoch::pin();
        let freed: usize = slot.cells().map(|cell| cell_swap(cell, None, &guard)).sum();
        self.release_inode_bytes(freed + SLOT_OVERHEAD);
        self.release_stream(slot, &guard);
    }

    /// Takes two entries off the hand, putting back those of live slots: each release outside
    /// eviction leaves one entry behind, so this keeps the hand within a small multiple of the
    /// slots held.
    fn trim_hand(&self) {
        for _ in 0..2 {
            let Some(entry) = self.clock.pop() else {
                return;
            };
            if entry.upgrade().is_some_and(|slot| slot.live()) {
                self.clock.push(entry);
            }
        }
    }

    pub fn held_bytes(&self) -> usize {
        self.held.load(Ordering::Relaxed)
    }

    pub fn peak_bytes(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    pub fn reset_peak(&self) {
        self.peak.store(self.held_bytes(), Ordering::Relaxed);
    }

    pub fn inodes_held(&self) -> usize {
        self.inodes.len()
    }

    fn hold(&self, bytes: usize) {
        let now = self.held.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    fn hold_inode_bytes(&self, bytes: usize) {
        self.inode_held.fetch_add(bytes, Ordering::SeqCst);
        self.hold(bytes);
    }

    fn release_inode_bytes(&self, bytes: usize) {
        self.inode_held.fetch_sub(bytes, Ordering::SeqCst);
        self.held.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// The state for `ino`, replaced if it last held content other than `root`. Borrowed: the
    /// shard stays read-locked until the borrow drops, which must come before any await.
    #[lore_macro::test_pub]
    fn slot(&self, ino: u64, root: Hash) -> Ref<'_, u64, Arc<InodeSlot>> {
        if let Some(slot) = self.inodes.get(&ino)
            && slot.root == root
        {
            clock::mark(&slot.referenced);
            return slot;
        }
        self.slot_entry(ino, root)
    }

    /// [`Self::slot`] under the shard's write lock: a racing insert's state is handed out only if
    /// it holds `root`, and replaced otherwise.
    #[lore_macro::test_pub]
    fn slot_entry(&self, ino: u64, root: Hash) -> Ref<'_, u64, Arc<InodeSlot>> {
        // The shard lock is held only to insert; no cell lock is taken under it.
        #[allow(clippy::disallowed_methods)]
        let entry = self.inodes.entry(ino);
        match entry {
            dashmap::Entry::Occupied(mut occupied) => {
                if occupied.get().root != root {
                    let replaced = occupied.insert(self.fresh_slot(ino, root));
                    self.release_slot(&replaced);
                    self.trim_hand();
                }
                occupied.into_ref().downgrade()
            }
            dashmap::Entry::Vacant(vacant) => vacant.insert(self.fresh_slot(ino, root)).downgrade(),
        }
    }

    /// New state for `ino` holding `root`, counted in and on the hand.
    fn fresh_slot(&self, ino: u64, root: Hash) -> Arc<InodeSlot> {
        let slot = Arc::new(InodeSlot::new(ino, root));
        self.hold_inode_bytes(SLOT_OVERHEAD);
        self.clock.push(Arc::downgrade(&slot));
        slot
    }

    /// Evicts per-inode state other than `keep`'s until the budget holds: a slot read since the
    /// hand last passed is spared once.
    fn enforce_inode_budget(&self, keep: u64) {
        self.clock.evict(
            || self.inode_held.load(Ordering::SeqCst) > self.inode_budget,
            |entry| self.visit_slot(entry, keep),
        );
    }

    /// Visits one slot on the hand; it is removed only while it is still the one held for its
    /// inode and no READ has marked it since.
    fn visit_slot(&self, entry: &Weak<InodeSlot>, keep: u64) -> Visit {
        #[cfg(feature = "test-util")]
        self.visits.fetch_add(1, Ordering::Relaxed);
        let Some(slot) = entry.upgrade().filter(|slot| slot.live()) else {
            return Visit::Stale;
        };
        if slot.ino == keep || clock::spare(&slot.referenced) {
            return Visit::Kept;
        }
        let removed = self.inodes.remove_if(&slot.ino, |_, held| {
            Arc::ptr_eq(held, &slot) && !held.referenced.load(Ordering::Relaxed)
        });
        if removed.is_none() {
            return if slot.live() {
                Visit::Kept
            } else {
                Visit::Stale
            };
        }
        #[cfg(feature = "test-util")]
        self.evictions.fetch_add(1, Ordering::Relaxed);
        self.release_slot(&slot);
        Visit::Evicted
    }

    /// Replaces `cell`'s entry with `cached`, keeping the byte count exact: every entry is
    /// counted in before the swap that makes it visible, so a concurrent release never counts
    /// out bytes not yet counted in, and out by the swap that displaces it.
    fn install(&self, ino: u64, slot: &InodeSlot, cell: &Cell, cached: Cached) {
        let out = self.swap_in(slot, cell, cached);
        self.release_inode_bytes(out);
        if slot.live() {
            self.enforce_inode_budget(ino);
        }
    }

    /// Swaps `cached` into `cell`, counted in first; answers the bytes it displaced, and those it
    /// took straight back when `slot` was released meanwhile, for the caller to count out.
    fn swap_in(&self, slot: &InodeSlot, cell: &Cell, cached: Cached) -> usize {
        self.hold_inode_bytes(cached.payload.len());
        let guard = epoch::pin();
        let mut out = cell_swap(cell, Some(cached), &guard);
        if !slot.live() {
            out += cell_swap(cell, None, &guard);
        }
        out
    }

    /// Keeps a list or leaf the strategy reuses.
    #[lore_macro::test_pub]
    fn keep(
        &self,
        ino: u64,
        slot: Option<&InodeSlot>,
        depth: usize,
        cached: Cached,
        strategy: FragmentCacheStrategy,
    ) {
        if let Some(slot) = slot {
            if is_list(&cached.fragment) {
                if let Some(cell) = slot.lists.get(depth) {
                    self.install(ino, slot, cell, cached);
                }
            } else {
                let kept = strategy.leaves_kept();
                if kept > 0 {
                    let index = slot.next_leaf.fetch_add(1, Ordering::Relaxed) % kept;
                    self.install(ino, slot, &slot.leaves[index], cached);
                }
            }
        } else if let FragmentCacheStrategy::Global { budget, .. } = strategy {
            let (added, freed) =
                self.global
                    .insert(cached.hash, cached.fragment, cached.payload, budget);
            self.held.fetch_sub(freed, Ordering::Relaxed);
            self.hold(added);
        }
    }

    /// Reads `out.len()` bytes of the content at `address` from `offset`; the caller has clamped
    /// the range to `size`. Fetched payloads are cached locally as `repository` says, which for
    /// the provider's context is always.
    ///
    /// A READ the caches serve whole finishes before its first await, holding its inode's state
    /// borrowed. Its only shared writes are reader counts of the inode map's shard and a
    /// referenced bit that is clear; with sequential prefetch on, also its inode's stream
    /// position. The store is borrowed, and the session resolved only for a load.
    #[allow(clippy::too_many_arguments)]
    pub async fn read(
        &self,
        repository: &Arc<RepositoryContext>,
        ino: u64,
        address: Address,
        size: u64,
        offset: u64,
        out: &mut [u8],
        fetch: bool,
    ) -> Result<(), StorageError> {
        if out.is_empty() {
            return Ok(());
        }
        let options = read_options(repository, fetch);
        let store = repository.immutable_store_ref();
        let partition = repository.id;
        let start = usize::try_from(offset)
            .map_err(|_overflow| StorageError::internal("read offset overflows"))?;
        let range = start..start + out.len();
        let strategy = self.strategy();
        if strategy == FragmentCacheStrategy::Storage {
            // Boxed: the storage walk's future is large, and the provider's READ future stays
            // small for the strategies that allocate nothing.
            let session = fetch
                .then(|| crate::immutable::resolve_session(repository))
                .flatten();
            return Box::pin(lore_storage::read_into(
                store.clone(),
                partition,
                address,
                Some(range),
                out,
                options,
                session,
            ))
            .await;
        }
        let mut walk = Walk {
            cache: self,
            repository,
            store,
            partition,
            context: address.context,
            options,
            strategy,
            ahead: false,
            prefetching: false,
        };
        if size <= FRAGMENT_SIZE_THRESHOLD as u64
            && walk.read_single(address.hash, start, out).await?
        {
            return Ok(());
        }
        let whole = Span {
            from: offset,
            to: offset + out.len() as u64,
            start: 0,
            end: size,
            depth: 0,
            key: 0,
        };
        let mut jobs = SmallVec::new();
        let borrowed = match strategy {
            FragmentCacheStrategy::PerInode(_) => Some(self.slot(ino, address.hash)),
            _ => None,
        };
        let sequential = match &borrowed {
            Some(slot) if self.prefetch.load(Ordering::Relaxed) => {
                walk.ahead = true;
                !fetch && self.stream_of(slot).note(offset, whole.to)
            }
            _ => false,
        };
        let root_served = walk.serve_root(
            borrowed.as_deref().map(Arc::as_ref),
            address.hash,
            &whole,
            out,
            &mut jobs,
        )?;
        if root_served && jobs.is_empty() {
            return Ok(());
        }
        let slot = borrowed.map(|slot| Arc::clone(&slot));
        // Boxed: only a READ the caches did not serve whole gets here, and it allocates for what
        // it loads anyway; the READ future stays small.
        let read = Box::pin(walk.read_tree(
            ino,
            slot.as_deref(),
            address.hash,
            whole,
            root_served,
            out,
            jobs,
        ))
        .await;
        if sequential
            && read.is_ok()
            && let Some(slot) = &slot
        {
            self.start_stream(slot, repository, address, size, whole.to);
        }
        read
    }
}

/// Stops the prefetch tasks of the state held: each waits on its inode's state, which a task
/// keeps alive after the cache is gone.
impl Drop for FragmentCache {
    fn drop(&mut self) {
        for slot in self.inodes.iter() {
            slot.evicted.store(true, Ordering::SeqCst);
            if let Some(stream) = slot.stream() {
                stream.stop();
            }
        }
    }
}

/// Part of a READ, `from..to`, to walk from a list spanning `start..end` at `depth`.
#[derive(Clone, Copy)]
struct Span {
    from: u64,
    to: u64,
    start: u64,
    end: u64,
    depth: usize,
    /// The list's [`child_key`] in the tree; 0 for the root.
    key: usize,
}

/// References a full list holds: the lists of a tree are cut at this many, all but each level's
/// last.
const LIST_REFERENCES: usize = FRAGMENT_SIZE_THRESHOLD / size_of::<FragmentReference>();

/// The key of the child at `index` of the list keyed `key`: its position among the children at
/// its depth of a tree whose lists are full, so consecutive leaves have consecutive keys across
/// sub-lists. Only its residue modulo a power of two is used, which wrapping keeps.
fn child_key(key: usize, index: usize) -> usize {
    key.wrapping_mul(LIST_REFERENCES).wrapping_add(index)
}

/// A fragment the walk has yet to load, and the part of the READ it covers.
struct Job {
    hash: Hash,
    /// The absolute content offset the parent list gives the fragment.
    offset_content: u64,
    depth: usize,
    /// Offset of this job's first byte within the fragment.
    local: usize,
    /// Where in the READ the fragment's bytes go.
    from: u64,
    to: u64,
    /// The content the fragment spans.
    start: u64,
    end: u64,
    /// The fragment's [`child_key`].
    key: usize,
}

impl Job {
    /// The part of the content this job's fragment spans that it covers.
    fn span(&self) -> Span {
        Span {
            from: self.from,
            to: self.to,
            start: self.start,
            end: self.end,
            depth: self.depth,
            key: self.key,
        }
    }
}

/// What a loaded fragment turned out to be. Nothing here has touched the reply: only the READ's
/// own future writes it, so no write outlives the READ however it ends.
enum Fetched {
    /// A leaf.
    Leaf(Fragment, Bytes),
    /// A further list, not yet walked.
    List(Fragment, Bytes),
}

struct Walk<'a> {
    cache: &'a FragmentCache,
    repository: &'a Arc<RepositoryContext>,
    store: &'a Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    options: ReadOptions,
    strategy: FragmentCacheStrategy,
    /// Whether the prefetched leaves are searched too.
    ahead: bool,
    /// Whether this walk prefetches, loading leaves decoded whatever the strategy.
    prefetching: bool,
}

impl Walk<'_> {
    /// The remote session a load hands to storage, resolved only when a READ loads and may fetch.
    fn session(&self) -> Option<Arc<StorageSession>> {
        self.options
            .remote
            .then(|| crate::immutable::resolve_session(self.repository))
            .flatten()
    }

    fn address(&self, hash: Hash) -> Address {
        Address {
            hash,
            context: self.context,
        }
    }

    fn load_options(&self) -> ReadOptions {
        let mut options = self.options;
        options.decompress = self.prefetching || self.strategy.decodes_on_load();
        options
    }

    /// Content one fragment holds: nothing is kept, so a directory of small files holds nothing.
    /// `false` when the fragment is a list, which content-defined chunking also makes of files
    /// under [`FRAGMENT_SIZE_THRESHOLD`]; the caller then walks it as a tree.
    async fn read_single(
        &self,
        hash: Hash,
        start: usize,
        out: &mut [u8],
    ) -> Result<bool, StorageError> {
        let mut options = self.options;
        options.decompress = true;
        #[cfg(feature = "test-util")]
        self.count_loads(1);
        let fetched = fetch(
            self.store.clone(),
            self.partition,
            self.address(hash),
            options,
            self.session(),
        )
        .await?;
        Ok(!matches!(place(fetched, start, out)?, Fetched::List(..)))
    }

    /// `hash` from whichever cache the strategy keeps, borrowed or as a refcount, never a copy.
    fn cached<'g>(
        &self,
        slot: Option<&'g InodeSlot>,
        depth: usize,
        hash: Hash,
        key: usize,
        guard: &'g epoch::Guard,
    ) -> Option<Hit<'g>> {
        match (slot, self.strategy) {
            (Some(slot), _) => slot
                .find(depth, hash, key, guard, self.ahead)
                .map(Hit::Borrowed),
            (None, FragmentCacheStrategy::Global { .. }) => self
                .cache
                .global
                .get(&hash)
                .map(|(fragment, payload)| Hit::Shared(fragment, payload)),
            _ => None,
        }
    }

    /// Serves `whole` as far as a cached root list reaches, queueing in `jobs` what has to load;
    /// `false` when the root list is not cached.
    fn serve_root(
        &self,
        slot: Option<&InodeSlot>,
        root: Hash,
        whole: &Span,
        out: &mut [u8],
        jobs: &mut SmallVec<[Job; 4]>,
    ) -> Result<bool, StorageError> {
        let guard = epoch::pin();
        let Some(root_list) = self
            .cached(slot, 0, root, 0, &guard)
            .filter(|hit| is_list(&hit.fragment()))
        else {
            return Ok(false);
        };
        self.serve(
            slot,
            root_list.payload(),
            whole,
            whole.from,
            out,
            jobs,
            &guard,
        )?;
        Ok(true)
    }

    /// Serves `span` of the READ from `list` down: cached lists are walked and cached leaves
    /// copied under `guard`, and the rest is queued in `jobs`. `out` holds the READ from content
    /// offset `offset`. Nothing awaits, so the caller may hold the slot borrowed.
    #[allow(clippy::too_many_arguments)]
    fn serve(
        &self,
        slot: Option<&InodeSlot>,
        list: &Bytes,
        span: &Span,
        offset: u64,
        out: &mut [u8],
        jobs: &mut SmallVec<[Job; 4]>,
        guard: &epoch::Guard,
    ) -> Result<(), StorageError> {
        let mut pos = span.from;
        while pos < span.to {
            let mut below: Option<Hit<'_>> = None;
            let (mut node_start, mut node_end, mut depth) = (span.start, span.end, span.depth);
            let mut key = span.key;
            loop {
                let refs = below
                    .as_ref()
                    .map_or(list, Hit::payload)
                    .as_type_slice::<FragmentReference>();
                let (index, child_start, child_end) = locate(refs, node_start, node_end, pos)
                    .ok_or_else(|| {
                        StorageError::internal("fragment list does not cover the read")
                    })?;
                let FragmentReference {
                    hash,
                    offset_content,
                } = refs[index];
                let take = span.to.min(child_end);
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(StorageError::internal("fragment lists nest too deep"));
                }
                let local = (pos - child_start) as usize;
                let dst = (pos - offset) as usize..(take - offset) as usize;
                key = child_key(key, index);
                match self.cached(slot, depth, hash, key, guard) {
                    Some(hit) if is_list(&hit.fragment()) => {
                        below = Some(hit);
                        node_start = child_start;
                        node_end = child_end;
                    }
                    Some(hit) => {
                        #[cfg(feature = "test-util")]
                        self.count_used(slot, hash, key, dst.len(), guard);
                        copy_range(hit.fragment(), hit.payload(), local, &mut out[dst])?;
                        pos = take;
                        break;
                    }
                    None => {
                        jobs.push(Job {
                            hash,
                            offset_content,
                            depth,
                            local,
                            from: pos,
                            to: take,
                            start: child_start,
                            end: child_end,
                            key,
                        });
                        pos = take;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn load_root(
        &self,
        ino: u64,
        slot: Option<&InodeSlot>,
        root: Hash,
        whole: &Span,
    ) -> Result<Bytes, StorageError> {
        #[cfg(feature = "test-util")]
        self.count_loads(1);
        let (fragment, payload) = Box::pin(lore_storage::load_fragment(
            self.store.clone(),
            self.partition,
            self.address(root),
            self.load_options(),
            self.session(),
        ))
        .await?;
        if !is_list(&fragment) {
            return Err(StorageError::internal("fragmented content without a list"));
        }
        let list = payload.to_aligned::<FragmentReference>();
        check_list(&list, &fragment, None, whole.from..whole.to)?;
        self.cache.keep(
            ino,
            slot,
            0,
            Cached {
                hash: root,
                fragment,
                payload: list.clone(),
            },
            self.strategy,
        );
        Ok(list)
    }

    /// Loads what the caches left of the READ, `jobs` first, walking each list as it arrives; a
    /// READ's uncached fragments load side by side rather than one after another.
    #[allow(clippy::too_many_arguments)]
    async fn read_tree(
        &self,
        ino: u64,
        slot: Option<&InodeSlot>,
        root: Hash,
        whole: Span,
        root_served: bool,
        out: &mut [u8],
        mut jobs: SmallVec<[Job; 4]>,
    ) -> Result<(), StorageError> {
        let offset = whole.from;
        let mut pending: SmallVec<[(Span, Bytes); 2]> = SmallVec::new();
        if !root_served {
            pending.push((whole, self.load_root(ino, slot, root, &whole).await?));
        }
        loop {
            for (job, fetched) in self.fetch_all(&jobs).await? {
                let dst = &mut out[(job.from - offset) as usize..(job.to - offset) as usize];
                match place(fetched, job.local, dst)? {
                    Fetched::Leaf(fragment, payload) => self.cache.keep(
                        ino,
                        slot,
                        job.depth,
                        Cached {
                            hash: job.hash,
                            fragment,
                            payload,
                        },
                        self.strategy,
                    ),
                    Fetched::List(fragment, list) => {
                        let parent = FragmentReference {
                            hash: job.hash,
                            offset_content: job.offset_content,
                        };
                        let span = job.span();
                        check_sublist(&list, &fragment, &parent, &span)?;
                        self.cache.keep(
                            ino,
                            slot,
                            job.depth,
                            Cached {
                                hash: job.hash,
                                fragment,
                                payload: list.clone(),
                            },
                            self.strategy,
                        );
                        pending.push((span, list));
                    }
                }
            }
            jobs.clear();
            let Some((span, list)) = pending.pop() else {
                return Ok(());
            };
            let guard = epoch::pin();
            self.serve(slot, &list, &span, offset, out, &mut jobs, &guard)?;
        }
    }

    /// Loads every job's fragment, side by side when there is more than one. Tasks own what they
    /// load, so dropping this (a deadline, an INTERRUPT) leaves them nothing of the caller's.
    async fn fetch_all<'j>(
        &self,
        jobs: &'j [Job],
    ) -> Result<SmallVec<[(&'j Job, Fetched); 4]>, StorageError> {
        let mut results = SmallVec::new();
        #[cfg(feature = "test-util")]
        self.count_loads(jobs.len());
        let options = self.load_options();
        let fetch_job = |job: &Job| {
            fetch(
                self.store.clone(),
                self.partition,
                self.address(job.hash),
                options,
                self.session(),
            )
        };
        match jobs {
            [] => return Ok(results),
            [job] => {
                results.push((job, fetch_job(job).await?));
                return Ok(results);
            }
            _ => {}
        }
        let mut tasks = JoinSet::new();
        for (index, job) in jobs.iter().enumerate() {
            let future = fetch_job(job);
            lore_base::lore_spawn!(tasks, async move { (index, future.await) });
        }
        let mut failure = None;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((index, Ok(fetched))) => results.push((&jobs[index], fetched)),
                Ok((_, Err(err))) => failure = failure.or(Some(err)),
                Err(err) => {
                    failure = failure.or(Some(StorageError::internal_with_context(
                        err,
                        "task failure",
                    )));
                }
            }
        }
        match failure {
            Some(err) => Err(err),
            None => Ok(results),
        }
    }
}

/// A loaded fragment as the walk takes it.
fn stored(fragment: Fragment, payload: Bytes) -> Fetched {
    if is_list(&fragment) {
        return Fetched::List(fragment, payload.to_aligned::<FragmentReference>());
    }
    Fetched::Leaf(fragment, payload)
}

/// Copies the part of a loaded leaf from content offset `local` into `dst`, handing back what the
/// caches may keep.
fn place(fetched: Fetched, local: usize, dst: &mut [u8]) -> Result<Fetched, StorageError> {
    if let Fetched::Leaf(fragment, payload) = &fetched {
        copy_range(*fragment, payload, local, dst)?;
    }
    Ok(fetched)
}

/// Loads the fragment at `address`.
async fn fetch(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> Result<Fetched, StorageError> {
    // Boxed: a miss allocates for its payload anyway, and the READ future stays small.
    let (fragment, payload) = Box::pin(lore_storage::load_fragment(
        store, partition, address, options, session,
    ))
    .await?;
    Ok(stored(fragment, payload))
}

/// Checks a list as it loads, as `lore_storage::defragment` checks a level before walking it:
/// every entry's window strictly increasing and non-empty, no zero hash, the level reaching
/// `window` (the READ's part of it, from the level's start), and for a sub-list, covering
/// exactly the window `parent`'s list gives it. A cached list was checked when it loaded, so a
/// READ served from the caches pays nothing here.
fn check_list(
    list: &Bytes,
    fragment: &Fragment,
    parent: Option<(&FragmentReference, std::ops::Range<u64>)>,
    window: std::ops::Range<u64>,
) -> Result<(), StorageError> {
    let refs = list.as_type_slice::<FragmentReference>();
    let size = usize::try_from(fragment.size_content)
        .map_err(|_overflow| StorageError::internal("fragment list content overflows"))?;
    if let Some((parent, span)) = parent {
        lore_storage::defragment::sublist_coverage(parent, refs, size, &span)?;
    }
    let base = refs
        .first()
        .ok_or_else(|| StorageError::internal("fragment list is empty"))?
        .offset_content;
    let level_end = base
        .checked_add(fragment.size_content)
        .ok_or_else(|| StorageError::internal("fragment list end overflows"))?;
    let window = window.start + base..window.end + base;
    if lore_storage::defragment::wanted_entries(refs, level_end, &window)?.is_none() {
        return Err(StorageError::internal(
            "fragment list does not cover the read",
        ));
    }
    Ok(())
}

/// Checks a sub-list `parent` names as it loads, over the part `span.from..span.to` of the
/// content it spans, `span.start..span.end`.
fn check_sublist(
    list: &Bytes,
    fragment: &Fragment,
    parent: &FragmentReference,
    span: &Span,
) -> Result<(), StorageError> {
    let window = parent.offset_content..parent.offset_content + (span.end - span.start);
    check_list(
        list,
        fragment,
        Some((parent, window)),
        span.from - span.start..span.to - span.start,
    )
}

/// The child of a list spanning `node_start..node_end` that holds `pos`: its index and span.
/// Checks only what the search needs; [`check_list`] checked the whole list as it loaded.
fn locate(
    refs: &[FragmentReference],
    node_start: u64,
    node_end: u64,
    pos: u64,
) -> Option<(usize, u64, u64)> {
    let base = refs.first()?.offset_content;
    let start_of = |reference: &FragmentReference| {
        reference
            .offset_content
            .checked_sub(base)
            .and_then(|relative| node_start.checked_add(relative))
    };
    let index = refs
        .partition_point(|reference| start_of(reference).is_some_and(|start| start <= pos))
        .checked_sub(1)?;
    let start = start_of(&refs[index])?;
    let end = match refs.get(index + 1) {
        Some(next) => start_of(next)?,
        None => node_end,
    };
    (start <= pos && pos < end && end <= node_end).then_some((index, start, end))
}

/// Puts content bytes `start..start + dst.len()` of a leaf into `dst`, decoding the whole of a
/// compressed one.
#[lore_macro::test_pub]
fn copy_range(
    fragment: Fragment,
    payload: &[u8],
    start: usize,
    dst: &mut [u8],
) -> Result<(), StorageError> {
    if dst.is_empty() {
        return Ok(());
    }
    let decoded;
    let content = if fragment.flags & FragmentFlags::PayloadCompressed == 0 {
        payload
    } else {
        decoded = lore_storage::compress::decompress(fragment, payload)
            .forward::<StorageError>("decoding a leaf")?
            .1;
        &decoded[..]
    };
    let source = start
        .checked_add(dst.len())
        .and_then(|end| content.get(start..end))
        .ok_or_else(|| StorageError::internal("leaf shorter than its list claims"))?;
    dst.copy_from_slice(source);
    Ok(())
}

/// The options a READ loads with. Local bytes are read as stored; what the remote serves is checked
/// on receipt as the command's trust of it asks (`lore_storage::read::check_receipt`). A process
/// waits on what a READ fetches, so it goes at priority, ahead of the background warm-ups'
/// normal-priority fetches; `fetch` allows the remote at all. Without it the READ runs on a worker,
/// which must never block, so a local miss answers at once.
#[lore_macro::test_pub]
fn read_options(repository: &RepositoryContext, fetch: bool) -> ReadOptions {
    let mut options = crate::immutable::read_options_from_repository(repository).with_priority();
    options.remote = fetch;
    options.never_wait = !fetch;
    options
}
