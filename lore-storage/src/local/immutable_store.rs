// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

#[cfg(not(feature = "test-util"))]
mod info;
#[cfg(feature = "test-util")]
pub mod info;
#[cfg(all(feature = "oodle", not(feature = "test-util")))]
mod oodle_migration;
#[cfg(all(feature = "oodle", feature = "test-util"))]
pub mod oodle_migration;
#[cfg(not(feature = "test-util"))]
mod payload_gate;
#[cfg(feature = "test-util")]
pub mod payload_gate;
use std::backtrace::Backtrace;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashSet;
use std::io;
use std::io::ErrorKind;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "failure_generator")]
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::allocator::GrowVec;
use lore_error_set::Internal;
use lore_error_set::prelude::*;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;
use smallvec::SmallVec;
use tokio::sync::Mutex;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::Address;
use crate::Context;
use crate::Fragment;
use crate::FragmentFlags;
use crate::FragmentReference;
use crate::Hash;
use crate::PackfileSize;
use crate::Partition;
use crate::TypedBytes;
use crate::compress;
use crate::errors::AddressNotFound;
use crate::errors::NotSupported;
use crate::errors::PayloadNotFound;
#[cfg(feature = "failure_generator")]
use crate::errors::SlowDown;
use crate::fs_util;
use crate::hash;
use crate::immutable_store::CopyBehavior;
use crate::immutable_store::StoreError;
use crate::immutable_store::sanitise_fragment_behavior_flags;
use crate::local::fan_out::BucketGroup;
use crate::local::fan_out::GroupLevel;
use crate::local::immutable_store::info::ImmutableStoreInfo;
use crate::local::immutable_store::info::get_or_init_disk_info;
use crate::local::immutable_store::info::info_path_for_store_root;
use crate::local::immutable_store::info::read_info_file;
use crate::local::immutable_store::info::write_info_file;
use crate::local::immutable_store::payload_gate::PayloadGate;
use crate::local::store_lock::Claim;
use crate::local::store_lock::Intent;
use crate::local::store_lock::LockedStore;
use crate::local::store_lock::ReleasingOpeningClaim;
use crate::local::store_lock::StoreGuard;
use crate::local::store_lock::StoreLock;
use crate::store_types::PayloadRead;
use crate::store_types::StoreGetData;
use crate::store_types::StoreMatch;
use crate::store_types::StoreMatchResult;
use crate::store_types::StoreObliterateStats;

#[error_set]
pub enum LocalImmutableStoreError {
    NotSupported,
    PayloadNotFound,
}

const VALIDATE_COMPACTION: bool = false;

pub const GROUP_COUNT: usize = 256;
pub const BUCKET_COUNT: usize = 256;

pub const DEFAULT_FLUSH_DELAY_SECONDS: u64 = 5;

const DOT_COMPACT: &str = "compact";

/// Compaction rewrites a group's packfiles only once at least this percentage of their bytes is
/// unreferenced. A rewrite copies every referenced byte to reclaim the rest.
const COMPACTION_MIN_UNREFERENCED_PERCENTAGE: usize = 10;

/// Head bytes requested by the composite bucket open: one pool buffer
/// class, covering fan-out-threshold-sized buckets (~100 KiB) entirely so
/// the common bucket load is a single backend dispatch.
#[lore_macro::test_pub]
pub(crate) const BUCKET_HEAD_READ: usize = 256 * 1024;

// 256 u32 makes the u32 growvec chunks 2048 bytes in size
const CHUNK_SIZE_U32: usize = 256;

// 32 entries makes the ImmutableStoreEntry growvec chunks 3072 bytes in size
const CHUNK_SIZE_ENTRY: usize = 32;

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, Eq, Hash, IntoBytes, FromBytes, Immutable, PartialEq)]
pub struct ImmutableData {
    pub flags: u32,
    pub size_payload: u32,
    pub size_content: u64,
    pub pack_offset: u32,
    pub pack_file: u32,
    pub last_access: u64,
}

impl ImmutableData {
    /// The stored flags of an entry holding the payload at `pack_file`, 0 for none, stored with
    /// `flags`: durable as they say, and local exactly when the entry holds its payload, whatever
    /// they say.
    #[inline]
    fn stored_flags(flags: u32, pack_file: u32) -> u32 {
        let local = if pack_file != 0 {
            FragmentFlags::PayloadStoredLocal.bits()
        } else {
            0
        };
        (flags & FragmentFlags::PayloadStored & !FragmentFlags::PayloadStoredLocal.bits()) | local
    }

    /// Assign the relevant data to make this instance of a fragment point to the same
    /// deduplicated payload as another fragment. Synchronizes `PayloadStoredLocal` with the
    /// resulting `pack_file` so the flag and the pointer it describes always agree.
    fn assign_deduplicated_payload(&mut self, deduplicated: ImmutableData) {
        debug_assert!(
            self.size_content == deduplicated.size_content,
            "Invalid deduplication, content size do not match"
        );
        self.pack_file = deduplicated.pack_file;
        self.pack_offset = deduplicated.pack_offset;
        self.size_payload = deduplicated.size_payload;
        self.flags = (deduplicated.flags & !FragmentFlags::PayloadStored)
            | Self::stored_flags(self.flags, self.pack_file);
    }

    /// Apply data from a copy operation's source onto `self`. Delegates payload adoption to
    /// [`Self::assign_deduplicated_payload`] (which preserves the `pack_file` ↔ stored-local
    /// invariant) and handles the per-(partition, address) durability flag separately:
    /// source's `PayloadStoredDurable` never propagates, since the source tuple's durability
    /// says nothing about the destination tuple's; the caller asserts the destination's
    /// durability through `durable`, typically after a successful remote round-trip.
    /// `self`'s pre-existing Durable bit is preserved.
    ///
    /// `last_access` is left untouched; the caller stamps it on paths that modify the entry.
    #[lore_macro::test_pub]
    fn merge_from_copy_source(&mut self, source: ImmutableData, durable: bool) {
        self.size_content = source.size_content;
        self.assign_deduplicated_payload(source);
        if durable {
            self.flags |= FragmentFlags::PayloadStoredDurable.bits();
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
pub struct ImmutableDataBeforeLastAccess {
    pub flags: u32,
    pub size_payload: u32,
    pub size_content: u64,
    pub pack_offset: u32,
    pub pack_file: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct ImmutableStoreFindResult {
    pub group: usize,
    pub data: ImmutableData,
    pub matching: StoreMatch,
    /// The partition the matched entry belongs to. The one searched for whenever it holds the hash,
    /// since `lookup` prefers it; another only when it does not.
    pub partition: Partition,
    /// The context the matched entry is stored under, which with the partition names the
    /// association found rather than only where it lives.
    pub context: Context,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, FromBytes, Immutable)]
pub struct ImmutableStoreEntry {
    pub address: Address,
    pub partition: Partition,
    pub data: ImmutableData,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, FromBytes, Immutable)]
pub struct ImmutableStoreEntryBeforeLastAccess {
    pub address: Address,
    pub partition: Partition,
    pub data: ImmutableDataBeforeLastAccess,
}

#[lore_macro::test_pub]
#[derive(Default)]
pub struct ImmutableStoreBucket {
    pub entry: GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
    pub sorted_index: GrowVec<u32, CHUNK_SIZE_U32>,
    deserialized: bool,
    upgrade_packfile: bool,
    serialize_lock: Arc<Mutex<()>>,
}

/// Where a payload is stored: its packfile, and its offset in that packfile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PayloadLocation {
    pack_file: u32,
    pack_offset: u32,
}

impl PayloadLocation {
    /// Where the payload of an entry holding `data` is stored.
    fn of(data: &ImmutableData) -> Self {
        Self {
            pack_file: data.pack_file,
            pack_offset: data.pack_offset,
        }
    }
}

/// A payload of a bucket: the hash of its content, where it is stored, its size, whether eviction
/// may take it from every entry holding it, and the newest last access among those entries.
struct BucketPayload {
    hash: Hash,
    location: PayloadLocation,
    size: u32,
    evictable: bool,
    last_access: u64,
}

/// Payload bytes a bucket stores, and the bytes of the payloads eviction may take.
#[derive(Clone, Copy, Debug, Default)]
struct BucketSizes {
    stored: usize,
    evictable: usize,
}

/// What size eviction took from a bucket: the entries that lost their payload, the payloads
/// evicted, and their size in bytes.
#[derive(Clone, Copy, Debug, Default)]
struct BucketEviction {
    entries: usize,
    payloads: usize,
    bytes: usize,
}

/// What compaction did with a packfile it rewrote.
enum Rewrite {
    /// Truncated, after copying this many bytes out of it.
    Truncated(usize),
    /// Kept, as an entry may still point into it, such as at a payload that could not be copied.
    Referenced,
    /// Kept, as the group fanned out during the rewrite.
    Interrupted,
}

/// What [`LocalImmutableStore::store_entry`] did with an entry.
#[derive(Clone, Copy)]
enum StoreOutcome {
    /// The entry already held its payload, or the store carried none, so it was kept. Its stored
    /// flags changed to `changed_flags`, when set.
    Kept { changed_flags: Option<u32> },
    /// The entry was inserted or updated to hold `flags` and point at packfile `pack_file`, 0 for
    /// no payload.
    Stored { flags: u32, pack_file: u32 },
    /// The entry needs a payload written for it, which the caller writes before storing again.
    NeedsPayload,
}

impl StoreOutcome {
    /// The flags to schedule the group's index flush for, when the bucket changed.
    fn flush_flags(self) -> Option<u32> {
        match self {
            Self::Kept { changed_flags } => changed_flags,
            Self::Stored { flags, .. } => Some(flags),
            Self::NeedsPayload => None,
        }
    }
}

/// The payload a store carries, as [`LocalImmutableStore::store_entry`] takes it.
enum StorePayload {
    /// None: the entry is recorded without one.
    Absent,
    /// One not written to a packfile yet.
    Unwritten,
    /// One written at this location.
    Written(crate::PackStoreRef),
}

/// What [`LocalImmutableStore::tombstone`] did with an entry.
enum Tombstone {
    /// Nothing: it was obliterated already.
    Already,
    /// Tombstoned it. Another entry of its hash is not a tombstone, or it held no payload.
    Kept,
    /// Tombstoned it, and every other entry of its hash is a tombstone: the payload it held, as
    /// this describes, is to be zeroed.
    Zero(ImmutableData),
    /// Nothing yet: its payload is to be zeroed, which happens only while compaction is held off.
    ZeroOnceCompactionStops,
}

impl ImmutableStoreBucket {
    /// Drops everything this bucket holds from disk, keeping what identifies it.
    ///
    /// For a store discarding state another process invalidated. Deliberately not
    /// `*self = Self::default()`: that would also replace `serialize_lock` with a
    /// fresh one, and a task holding a clone of the old `Arc` would then be excluding
    /// against a lock nobody else takes. The lock is the bucket's identity, not its
    /// contents.
    fn reset_content(&mut self) {
        self.entry = GrowVec::default();
        self.sorted_index = GrowVec::default();
        self.deserialized = false;
        self.upgrade_packfile = false;
    }

    fn clone_for_compaction(&self) -> (Vec<ImmutableStoreEntry>, Vec<u32>) {
        (self.entry.to_vec(), self.sorted_index.to_vec())
    }
}

/// One group's bucket levels, as a survey read them.
#[derive(Clone, Copy)]
struct GroupLevels {
    /// Active buckets in the group: slots `[0..count]` are addressable.
    count: usize,
    /// Bucket count the on-disk `level` marker records; `0` when there is no marker.
    committed: usize,
}

/// What a survey of the store directory found.
///
/// Carries the store-wide decision and the per-group levels together because the two
/// are decided together: the serialize version depends on whether *any* group has a
/// marker, so no group's level can be settled until every marker has been read.
struct GroupSurvey {
    /// Version to write into bucket file headers, the same for every group.
    serialize_version: u32,
    /// One entry per group, in group order, always [`GROUP_COUNT`] long.
    levels: Vec<GroupLevels>,
    /// The highest group with anything on disk — a level marker, or bucket files from
    /// before fan-out — or `-1` where no group has either.
    #[cfg(feature = "oodle")]
    highest_materialized: i32,
}

pub struct ImmutableStoreGroup {
    /// Per-slot lazily-initialized bucket. Empty `OnceLock` at construction; first
    /// `bucket()` call materializes the `Arc<RwLock<ImmutableStoreBucket>>`. Use
    /// `try_bucket()` for paths that must be a no-op when the slot has never been
    /// touched (flush of a clean slot, dirty-only scans, diagnostic iteration).
    pub bucket: [OnceLock<Arc<RwLock<ImmutableStoreBucket>>>; BUCKET_COUNT],
    /// Dirty flag per bucket, kept outside the bucket's `RwLock` so `flush_all`
    /// can scan for work with lock-free atomic loads.
    ///
    /// Set by a modification, which is why it keeps the store's flock: a process must
    /// not sit on changes another process cannot see.
    pub dirty: [AtomicBool; BUCKET_COUNT],
    /// Bucket has an access stamp worth writing, and nothing else.
    ///
    /// Access stamps rank fragments for eviction, and a client reads far more than it
    /// writes — so ranking that ignored reads would be ranking on almost nothing. But a
    /// stamp is recorded under a *read* claim, which never advances the epoch, and it
    /// is not a change another process needs to be told about.
    ///
    /// So this marks the bucket for the next flush and nothing more. A flush writes it
    /// out — under a read claim when stamps are all there is to write, so a read's stamp
    /// reaches disk without announcing a change — and the store's answer about unflushed
    /// buckets ignores it, so stamps alone never hold the flock and never keep a store
    /// claimed after the work is done.
    pub soft_dirty: [AtomicBool; BUCKET_COUNT],
    /// Number of active buckets in this group. Slots `[0..bucket_count]` are addressable;
    /// `[bucket_count..BUCKET_COUNT]` are pre-allocated but unused (always empty, never dirty,
    /// never serialized). Loaded with `Relaxed` ordering — synchronization between fan-out and
    /// concurrent reads/writes comes from the per-bucket `RwLock`, not this atomic.
    pub bucket_count: std::sync::atomic::AtomicUsize,
    /// Version to write into bucket file headers on serialize. `LazyFanOut` (v5) for fan-out-aware
    /// stores; `LastAccessInEntry` (v4) for legacy stores untouched by fan-out-aware code
    /// (preserves backward compatibility with older clients). Set once at store construction;
    /// same value for every group in the same store. `Relaxed` ordering — only read by serialize.
    pub serialize_version: std::sync::atomic::AtomicU32,
    /// Per-bucket entry threshold that triggers a fan-out at the next serialize. Mirrored from
    /// `ImmutableStoreSettings::fan_out_threshold` so the per-group serialize task has access
    /// without holding a store reference. Same value across all groups in a store.
    pub fan_out_threshold: usize,
    /// Bucket count recorded by the on-disk `level` marker. `0` means "no marker exists yet"
    /// (a fresh fan-out-aware store before its first flush). Updated only after a successful
    /// two-phase commit (`level.pending` deleted), so a mismatch with `bucket_count` indicates a
    /// pending level transition that needs the two-phase commit on the next flush.
    pub committed_level: std::sync::atomic::AtomicUsize,
    /// The level a fan-out commit names in the `level.pending` it wrote, until the commit completes
    /// or a flush rolls it forward, or `0`. The commit holds the flush lock throughout, so under it
    /// this is non-zero only after a commit that failed past that point. Recovery takes that commit
    /// as made, and the bucket files on disk are partly at its layout, so no bucket is written
    /// until a flush, explicit or delayed, rolls it forward.
    pub pending_level: std::sync::atomic::AtomicUsize,
    /// Forces the bucket-file writes for this group to be serial.
    ///
    /// `flush_all` holds it for a whole group flush so that the fan-out check, the
    /// `committed_level` read that selects the commit path, and the writes are one
    /// atomic unit. Every other writer (the delayed flush, the evictor, the
    /// compactor, the packfile upgrade) takes it only around its own bucket write.
    ///
    /// Without it, an overlapping flush can observe a half-finished level transition
    /// and take the regular path while a two-phase commit is still pending. The
    /// commit's later `rename` of `index_<bb>.new` then publishes its older snapshot
    /// over the newer regular write, discarding it. The losing write still
    /// returns `Ok`, and the clobbered file inherits the `.new` file's older mtime.
    /// Locking the rename alone would not help: the snapshot it publishes is taken
    /// before the rename, so the two paths must not interleave at all.
    ///
    /// Scope is deliberately narrow outside `flush_all` because
    /// `compact_group_packfiles` calls `evict_group_sized`, and a `tokio::sync::Mutex`
    /// is not reentrant - a per-function guard would self-deadlock.
    pub flush_lock: Arc<Mutex<()>>,
    /// The writes in flight in this group, which taking [`Self::flush_lock`] waits for, see
    /// [`crate::local::fan_out::GroupWrites`].
    pub writes: Arc<crate::local::fan_out::GroupWrites>,
    pub packstore: crate::PackStore,
    pub flush: Mutex<JoinSet<()>>,
    /// Whether a delayed flush is due for this group and has not yet begun its sweep.
    ///
    /// A writer that flags a bucket schedules a sweep only when it is the one to set this,
    /// and a sweep clears it as it wakes, before it reads a single flag. A bucket flagged
    /// while a sweep runs — after the sweep has passed it — therefore schedules the next
    /// sweep rather than being left to the one already past it, where it would keep the
    /// store claimed with nothing coming to write it.
    pub scheduled: AtomicBool,
    /// Held by a store from before its packfile write until its entry is in the bucket, and by a
    /// read from its lookup until it has loaded the payload. Compaction drains it between reading
    /// the packfile sizes and measuring the bytes the index references, once it has stopped
    /// writes to the packfiles it rewrites, and before it truncates one, so that it neither misses
    /// a payload written but not yet indexed nor truncates one a read is loading.
    pub payload_gate: PayloadGate,
}

impl ImmutableStoreGroup {
    /// Whether this bucket has anything a flush should write — a modification, or an
    /// access stamp recorded since the last one.
    #[inline]
    pub fn needs_flush(&self, bucket_index: usize) -> bool {
        self.dirty[bucket_index].load(atomic::Ordering::Relaxed)
            || self.soft_dirty[bucket_index].load(atomic::Ordering::Relaxed)
    }

    /// What a flush of this group's buckets would write, which decides the claim that
    /// flush takes.
    ///
    /// Stops at the first changed bucket, since a change decides the answer; a group
    /// holding only access stamps, or nothing, is scanned in full.
    fn pending_flush(&self) -> PendingFlush {
        let mut stamps = false;
        for (dirty, stamped) in self.dirty.iter().zip(self.soft_dirty.iter()) {
            if dirty.load(atomic::Ordering::Relaxed) {
                return PendingFlush::Changes;
            }
            stamps |= stamped.load(atomic::Ordering::Relaxed);
        }
        if stamps {
            PendingFlush::Stamps
        } else {
            PendingFlush::Nothing
        }
    }

    /// Drop everything this group has cached from disk, and take the levels a fresh
    /// survey read.
    ///
    /// For a store that found another process had changed it. Every field here is
    /// either interior-mutable or fixed by the store's settings, so a group can be
    /// emptied where it stands — which is what keeps [`LocalImmutableStore::group`]
    /// reachable without a lock.
    ///
    /// A materialized bucket is reset **through its own `RwLock`**, the one every read
    /// and write already takes, so this adds no synchronization to any path. Slots
    /// never touched are not locked: an unmaterialized `OnceLock` holds nothing to
    /// discard, so the locking and resetting are proportional to what was used. The
    /// flags are cleared for every slot, a pass over `BUCKET_COUNT` atomics. A fan-out commit
    /// left pending is forgotten: the reload's survey has rolled it forward on disk.
    ///
    /// Does no I/O and cannot fail. The packstore is only *forgotten*; `resume()`
    /// re-reads it on next use, as it does for a store that has just been opened.
    ///
    /// # Requirements
    ///
    /// The caller must hold the store's flock with nothing else in flight on this store, so that
    /// no walk, read or flush finds buckets emptied under it. A reload's caller does: a guard is
    /// stale only where it takes the flock afresh, with nothing of this process running on the
    /// directory, for a store that has adopted no epoch, which has nothing in flight, or after
    /// another store object on the directory advanced the epoch. `lore-revision` opens its disk
    /// stores through a cache that keeps one store object per directory in a process; only the
    /// creation of a shared store opens others, while it runs.
    pub async fn invalidate(&self, count: usize, committed: usize, serialize_version: u32) {
        for slot in 0..BUCKET_COUNT {
            if let Some(bucket) = self.try_bucket(slot) {
                bucket.write().await.reset_content();
            }
            self.dirty[slot].store(false, atomic::Ordering::Relaxed);
            self.soft_dirty[slot].store(false, atomic::Ordering::Relaxed);
        }
        self.bucket_count.store(count, atomic::Ordering::Relaxed);
        self.committed_level
            .store(committed, atomic::Ordering::Relaxed);
        self.pending_level.store(0, atomic::Ordering::Relaxed);
        self.serialize_version
            .store(serialize_version, atomic::Ordering::Relaxed);
        self.packstore.forget().await;
    }

    /// Resolve a bucket slot, creating its `Arc<RwLock<ImmutableStoreBucket>>` on
    /// first touch.
    #[inline]
    pub fn bucket(&self, idx: usize) -> &Arc<RwLock<ImmutableStoreBucket>> {
        self.bucket[idx].get_or_init(|| Arc::new(RwLock::new(ImmutableStoreBucket::default())))
    }

    /// Return the bucket at `idx` only if it has been initialized. Never
    /// triggers materialization.
    #[inline]
    pub fn try_bucket(&self, idx: usize) -> Option<&Arc<RwLock<ImmutableStoreBucket>>> {
        self.bucket[idx].get()
    }
}

/// What a background pass gets when it claims the store without waiting.
enum BackgroundClaim {
    /// Claimed, and reloaded first if the store had changed.
    Held(StoreGuard),
    /// An in-memory store, which has no flock to claim.
    Unlocked,
    /// Another process holds the store, or this one has it mid-transition; the pass
    /// runs another time.
    Busy,
}

/// What a flush would write, which decides the claim it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingFlush {
    /// Nothing is flagged: no claim is taken and nothing is written.
    Nothing,
    /// Only access stamps, which change no entry: written under a read claim, so no
    /// other process is told the store changed.
    Stamps,
    /// A modification: written under a write claim, which advances the epoch before
    /// anything reaches disk.
    Changes,
}

impl BucketGroup for ImmutableStoreGroup {
    type Bucket = ImmutableStoreBucket;

    fn bucket_count(&self) -> &AtomicUsize {
        &self.bucket_count
    }

    fn bucket(&self, index: usize) -> &Arc<RwLock<ImmutableStoreBucket>> {
        ImmutableStoreGroup::bucket(self, index)
    }
}

#[cfg(feature = "failure_generator")]
pub struct LocalImmutableStoreFailureGenerator {
    retry_rate: f32,
    miss_fragment_writes: HashSet<Hash>,
    /// Hashes whose first write fails, from `LORE_REFUSE_FRAGMENT_WRITES`.
    refuse_fragment_writes: parking_lot::Mutex<HashSet<Hash>>,
}

#[cfg(feature = "failure_generator")]
impl LocalImmutableStoreFailureGenerator {
    /// The hashes the environment variable `name` lists, separated by commas.
    fn hashes_from_env(name: &str) -> HashSet<Hash> {
        std::env::var(name)
            .map(|val| {
                val.split(",")
                    .filter_map(|hash| Hash::from_str(hash.trim()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Holds a store's garbage collection stop raised while it lives. A terminating request
/// stays raised once dropped; any other lowers, so a drain that is cancelled rather than
/// completed — a shutdown timing out, say — cannot leave collection stopped by accident.
#[lore_macro::test_pub]
struct GcStopRequest<'a> {
    requests: &'a AtomicUsize,
    terminate: bool,
}

impl<'a> GcStopRequest<'a> {
    #[lore_macro::test_pub]
    fn raise(requests: &'a AtomicUsize, terminate: bool) -> Self {
        requests.fetch_add(1, atomic::Ordering::Relaxed);
        Self {
            requests,
            terminate,
        }
    }
}

impl Drop for GcStopRequest<'_> {
    fn drop(&mut self) {
        if !self.terminate {
            self.requests.fetch_sub(1, atomic::Ordering::Relaxed);
        }
    }
}

#[lore_macro::test_pub]
pub struct LocalImmutableStore {
    info: RwLock<ImmutableStoreInfo>,
    path: Option<Arc<PathBuf>>,
    /// The groups, one per leading hash byte.
    ///
    /// **Reached without synchronization, and it must stay that way.** `get`, `store`
    /// and `find` index this on every call, so a lock here sits in front of the bucket
    /// lock those paths already take, on the hottest path in the store.
    ///
    /// That is affordable because the vector is fixed: [`GROUP_COUNT`] is a constant
    /// and a group is chosen by `address.hash.data()[0]`, so the length and every
    /// group's identity hold for the store's life. Only what a group *caches* varies,
    /// and [`ImmutableStoreGroup::invalidate`] drops that where it stands.
    pub group: Vec<Arc<ImmutableStoreGroup>>,
    eviction: Semaphore,
    compaction: Semaphore,
    /// Stop requests outstanding; callers overlap, so the stop stays raised until the last
    /// of them has drained.
    stop_requests: AtomicUsize,
    /// Bytes reclaimed by the compaction step in progress, accumulated across its groups
    /// and reset as the step starts. Reported in the compaction-end callback.
    compaction_reclaimed: AtomicU64,
    deserialize_all: Semaphore,
    deserialized_all: AtomicBool,
    settings: ImmutableStoreSettings,
    instruments: StoreInstruments,
    /// Per-store running totals collected as data is loaded from disk; drive the
    /// load-triggered automatic GC. Shared (by `Arc` clone) with this store's
    /// packstores. See [`crate::maintenance::GcCounters`].
    gc_counters: Arc<crate::maintenance::GcCounters>,

    #[cfg(feature = "failure_generator")]
    failure_generator: LocalImmutableStoreFailureGenerator,

    /// This store's cross-process lock, or `None` for an in-memory store, which
    /// has no on-disk state for another process to touch.
    ///
    /// Not an `FSLock` owned for the store's lifetime: the flock is held for a span
    /// of use — a `lore_storage_open` handle from open to close, or one command — and
    /// a store kept alive between spans holds none of its own. See
    /// [`crate::local::store_lock`].
    lock: Option<Arc<StoreLock>>,
}

/// How far a last-access stamp has to move before the bucket holding it is marked for rewrite.
const ATIME_GRANULARITY_SECONDS: u64 = 60 * 60;

/// Why a lookup is happening, which decides whether the access stamp it records is
/// worth keeping.
///
/// The stamp ranks fragments for eviction, so losing one costs a slightly worse
/// eviction choice and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The caller is reading. **The stamp marks the bucket for flush, not as changed.**
    ///
    /// A read's stamp is not a change another process needs to be told about, and a
    /// bucket flagged as changed keeps the flock until a flush writes it, so stamps
    /// counted as changes would have a read-heavy process hold the store against every
    /// other one and advance the epoch for nothing. So the stamp sets the bucket's soft
    /// flag, which a flush writes under a read claim.
    Read,
    /// The caller is writing and the lookup is part of that write.
    ///
    /// The write has already advanced the epoch, so keeping the stamp costs nothing —
    /// and a `put` of content the store already has is still a touch of that content,
    /// which is exactly what the ranking is for.
    Write,
}

pub struct ImmutableStoreSettings {
    /// Protect local fragments during eviction/compaction (true for clients, false for server)
    pub protect_local_fragment: bool,
    /// Consider all fragments durably stored (false for clients, generally true for server)
    pub implicit_durable_stored: bool,
    /// Refuse to serve a payload found under a different partition (false for clients, true for
    /// server). Partitions are the unit access is granted on, so a process holding content for
    /// several tenants must not let one of them read another's bytes by hash alone.
    pub isolate_partitions: bool,
    /// Flush in the background
    pub flush_background: bool,
    /// Flush delay in seconds
    pub flush_delay_seconds: u64,
    /// Eviction target capacity as a percentage of the max capacity (0-100)
    pub target_capacity_percentage: usize,
    /// Compaction target size as a percentage of the max size (0-100)
    pub target_size_percentage: usize,
    /// Number of groups done in parallel during compaction (1-256)
    pub compaction_parallel_groups: usize,
    /// Verify writes by read back and rehash data
    pub verify_write: bool,
    /// Record the last access time of an entry on reads. Eviction and compaction rank by that
    /// time, so a store that reclaims needs it on.
    pub atime: bool,
    /// Number of buckets per group at store creation. Must be a value from
    /// `lore_storage::local::fan_out::LEVEL_LADDER`. Defaults to `1` (client). Server processes
    /// should set this to `256` to match today's flat layout. Existing on-disk stores ignore this
    /// and load at whatever level their per-group marker files indicate.
    pub initial_fan_out_level: usize,
    /// Per-bucket entry threshold that triggers fan-out at the next serialize. Default is `1000`.
    pub fan_out_threshold: usize,
    /// Reuse the in-memory state across holds of the store's flock when its epoch is
    /// unchanged, rather than reloading at the first claim after a release. Default is `false`.
    pub reuse_state_across_holds: bool,
}

impl Default for ImmutableStoreSettings {
    fn default() -> Self {
        Self {
            protect_local_fragment: true,
            implicit_durable_stored: false,
            isolate_partitions: false,
            flush_background: false,
            flush_delay_seconds: DEFAULT_FLUSH_DELAY_SECONDS,
            target_capacity_percentage: 70,
            target_size_percentage: 70,
            compaction_parallel_groups: 8,
            verify_write: false,
            atime: true,
            initial_fan_out_level: 1,
            fan_out_threshold: crate::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT,
            reuse_state_across_holds: false,
        }
    }
}

#[repr(u32)]
pub enum ImmutableStoreVersion {
    /// Initial version
    Initial = 1,
    /// Added last access timestamp
    LastAccessTimestamps = 2,
    /// Packfiles per group
    PackfilePerGroup = 3,
    /// Last access timestamp in entry
    LastAccessInEntry = 4,
    /// Lazy fan-out: bucket count per group is variable (see `local::fan_out`); marker file may
    /// be present in the group directory recording the current bucket count. Bucket file format
    /// itself is unchanged from `LastAccessInEntry`; this version is purely a forward-compat
    /// sentinel that prevents older binaries from misinterpreting `index_<bb>` filenames.
    LazyFanOut = 5,
}

#[lore_macro::test_pub]
#[repr(C)]
#[derive(Clone, Copy, Default, IntoBytes, FromBytes, Immutable)]
struct ImmutableStoreHeader {
    version: u32,
    /// The packfile sizes the file records, see [`RecordedSizes`]: [`SIZES_RECORDED`], their
    /// number at [`SIZES_COUNT_SHIFT`], and the id of the first, zero in a file written without.
    packfile_sizes: u32,
    count: u32,
    /// The size of the first packfile `packfile_sizes` records.
    first_packfile_size: u32,
    // Following the index store is
    // Sorted index of entries
    // sorted_index: [u32; count]
    // All entries
    // entry[IndexStoreEntry; count]
    // The recorded packfile sizes after the first
    // packfile_size[PackfileSize; recorded - 1]
}

/// Set in [`ImmutableStoreHeader::packfile_sizes`] by a writer that records packfile sizes.
#[lore_macro::test_pub]
const SIZES_RECORDED: u32 = 1 << 31;
/// Where [`ImmutableStoreHeader::packfile_sizes`] counts the recorded sizes.
#[lore_macro::test_pub]
const SIZES_COUNT_SHIFT: u32 = 24;
/// The bits of [`ImmutableStoreHeader::packfile_sizes`] holding the first packfile's id.
const SIZES_ID_MASK: u32 = (1 << SIZES_COUNT_SHIFT) - 1;
/// The most packfile sizes a bucket file holds after its entries. They take fewer bytes than an
/// entry and its sorted index slot, so that a reader that does not know of them counts the
/// entries alone and ignores them, see `ImmutableStoreBucket::deserialize_file_content`.
#[lore_macro::test_pub]
const TRAILER_SIZES: usize = 12;
const _: () = assert!(
    TRAILER_SIZES * size_of::<PackfileSize>() < size_of::<u32>() + size_of::<ImmutableStoreEntry>()
);

impl ImmutableStoreHeader {
    /// Record `sizes`, at most one more than [`TRAILER_SIZES`]: the first in the header, the rest
    /// to follow the entries.
    fn record_sizes(&mut self, sizes: &[PackfileSize]) {
        self.packfile_sizes = SIZES_RECORDED | ((sizes.len() as u32) << SIZES_COUNT_SHIFT);
        if let Some(first) = sizes.first() {
            self.packfile_sizes |= first.id;
            self.first_packfile_size = first.size;
        }
    }

    /// The packfile sizes the file records, the first from the header and the rest from
    /// `trailer`, the bytes after the entries.
    fn recorded_sizes(&self, trailer: Bytes) -> RecordedSizes {
        if self.packfile_sizes & SIZES_RECORDED == 0 {
            return RecordedSizes::Unrecorded;
        }
        let count = ((self.packfile_sizes & !SIZES_RECORDED) >> SIZES_COUNT_SHIFT) as usize;
        if count.saturating_sub(1) * size_of::<PackfileSize>() != trailer.len() {
            return RecordedSizes::Torn;
        }
        RecordedSizes::Recorded {
            first: (count > 0).then_some(PackfileSize {
                id: self.packfile_sizes & SIZES_ID_MASK,
                size: self.first_packfile_size,
            }),
            rest: trailer,
        }
    }
}

/// The packfile sizes a bucket file records: the bytes reserved, as it was written, in each
/// packfile a crash could leave shorter than an entry pointing into it, see
/// [`crate::PackStore::unsettled_sizes`]. A packfile not recorded is settled, or was synced
/// before the file was written.
#[lore_macro::test_pub]
enum RecordedSizes {
    /// Written by a version that records none. Its entries are taken to point at payloads stored.
    Unrecorded,
    /// The first size, from the header, and the bytes of the rest, which follow the entries.
    Recorded {
        first: Option<PackfileSize>,
        rest: Bytes,
    },
    /// Fewer sizes follow the entries than the header counts, as when a crash cut the file short.
    Torn,
}

/// What a bucket file holds, see [`ImmutableStoreBucket::deserialize_files`].
#[lore_macro::test_pub]
struct BucketFile {
    sorted_index: GrowVec<u32, CHUNK_SIZE_U32>,
    entry: GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
    upgrade_packfile: bool,
    mark_dirty: bool,
    sizes: RecordedSizes,
}

impl BucketFile {
    /// A bucket file without entries.
    fn empty() -> Self {
        Self {
            sorted_index: GrowVec::new(),
            entry: GrowVec::new(),
            upgrade_packfile: false,
            mark_dirty: false,
            sizes: RecordedSizes::Unrecorded,
        }
    }
}

pub fn format_bucket_path(path: &Path, group_index: usize, bucket_index: usize) -> PathBuf {
    use crate::local::fan_out::BUCKET_FILENAME_PREFIX as PREFIX;
    use crate::local::fan_out::write_hex_byte;
    let mut path = path.to_path_buf();
    path.reserve(20);
    path.push("index");
    let mut name = [0u8; PREFIX.len() + 2];
    write_hex_byte(&mut name, group_index as u8);
    path.push(std::str::from_utf8(&name[..2]).unwrap_or_default());
    name[..PREFIX.len()].copy_from_slice(PREFIX.as_bytes());
    write_hex_byte(&mut name[PREFIX.len()..], bucket_index as u8);
    path.push(std::str::from_utf8(&name).unwrap_or_default());
    path
}

/// Walks the immutable index dir and returns true as soon as it finds any bucket file whose
/// header records a version older than `LastAccessInEntry` (v4). Used at construction time to
/// decide whether to upgrade an existing store all the way to `LazyFanOut` (v5) — older
/// stores need the full upgrade so the next flush writes markers and forward-compat sentinels.
/// Reads only the first 4 bytes (the version field) of each scanned bucket file. Samples one
/// bucket per group dir to bound the worst-case I/O at 256 file opens.
fn detect_any_older_immutable_bucket(index_path: &Path) -> bool {
    let Ok(group_dirs) = std::fs::read_dir(index_path) else {
        return false;
    };
    for entry in group_dirs.flatten() {
        let group_dir = entry.path();
        if !group_dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&group_dir) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let name_str = name.to_str().unwrap_or("");
            if !crate::local::fan_out::is_bucket_file_name(name_str) {
                continue;
            }
            if let Ok(mut f) = std::fs::File::open(file.path()) {
                let mut bytes = [0u8; 4];
                if f.read_exact(&mut bytes).is_ok() {
                    let version = u32::from_le_bytes(bytes);
                    if version < ImmutableStoreVersion::LastAccessInEntry as u32 {
                        return true;
                    }
                }
            }
            // Sampling one bucket per group is enough — versions don't realistically vary across buckets within a single store.
            break;
        }
    }
    false
}

enum DeserializeFileError {
    FutureVersion(u32),
    Corrupt(String),
    Unreadable(io::Error),
}

/// The error for a failed read of a bucket file's contents: a file shorter than its header says
/// is corrupt, and any other failure, such as a transient I/O error, leaves the file unread.
fn bucket_read_error(err: io::Error, what: &str) -> DeserializeFileError {
    if err.kind() == ErrorKind::UnexpectedEof {
        DeserializeFileError::Corrupt(format!("{what}: {err}"))
    } else {
        DeserializeFileError::Unreadable(err)
    }
}

/// Final-destination segments for a bucket read: one vectored operation
/// scatters the on-disk sorted-index and entry regions straight into the
/// bucket's chunk allocations, with no staging buffer.
struct ImmutableBucketSegments {
    sorted_index: GrowVec<u32, CHUNK_SIZE_U32>,
    entry: GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
}

impl lore_io::StableBufListMut for ImmutableBucketSegments {
    fn byte_segments_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        self.sorted_index
            .byte_segments_mut()
            .chain(self.entry.byte_segments_mut())
    }
}

/// A bucket's file contents, the serialized header, the bucket's sorted-index and entry chunks,
/// then the packfile sizes the header has no room for, copied out of the bucket so that its read
/// lock is released before the file is created and written. Writeback throttling can stall those
/// for seconds, and an insert waiting for the write lock would hold every later lookup in the
/// bucket behind it. The copy is one allocation of the file's size.
struct ImmutableBucketWrite {
    bytes: Vec<u8>,
}

impl ImmutableBucketWrite {
    /// Copy `bucket` after room for the header, see [`Self::finish`].
    fn copy(bucket: &ImmutableStoreBucket) -> Self {
        let chunks = || {
            bucket
                .sorted_index
                .byte_segments()
                .chain(bucket.entry.byte_segments())
        };
        let mut bytes = Vec::with_capacity(
            size_of::<ImmutableStoreHeader>()
                + chunks().map(<[u8]>::len).sum::<usize>()
                + TRAILER_SIZES * size_of::<PackfileSize>(),
        );
        bytes.resize(size_of::<ImmutableStoreHeader>(), 0);
        for chunk in chunks() {
            bytes.extend_from_slice(chunk);
        }
        Self { bytes }
    }

    /// Write `header` in the room left for it, recording `sizes`, see
    /// [`ImmutableStoreHeader::record_sizes`], and append the sizes it has no room for.
    fn finish(&mut self, mut header: ImmutableStoreHeader, sizes: &[PackfileSize]) {
        header.record_sizes(sizes);
        self.bytes[..size_of::<ImmutableStoreHeader>()].copy_from_slice(header.as_bytes());
        if let Some(rest) = sizes.get(1..) {
            self.bytes.extend_from_slice(rest.as_bytes());
        }
    }
}

impl lore_io::StableBufList for ImmutableBucketWrite {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.bytes.as_slice())
    }
}

/// Re-arms a bucket's flag when a serialize ends without [`Self::success`], while the write still
/// holds the bucket's serialize lock, so that a bucket write that failed, or was dropped part way,
/// leaves the bucket to the next flush.
///
/// The flag is cleared before the write begins, so a failed write that left it clear would
/// never be retried, and a flush scan would find the store clean and release its flock over
/// state that is only in memory.
pub struct SerializeFailureGuard<'a> {
    success: bool,
    dirty: &'a AtomicBool,
}

impl<'a> SerializeFailureGuard<'a> {
    /// A guard over `dirty`, the flag of the bucket being written that the write claimed.
    pub fn new(dirty: &'a AtomicBool) -> Self {
        Self {
            success: false,
            dirty,
        }
    }

    /// Records the write as complete, so that dropping the guard leaves the flag alone.
    pub fn success(&mut self) {
        self.success = true;
    }
}

impl<'a> Drop for SerializeFailureGuard<'a> {
    fn drop(&mut self) {
        if !self.success {
            self.dirty.store(true, atomic::Ordering::Relaxed);
        }
    }
}

impl ImmutableStoreBucket {
    #[lore_macro::test_pub]
    async fn deserialize_files(path: PathBuf) -> Result<BucketFile, LocalImmutableStoreError> {
        let (file, metadata, head) = match lore_io::IoDriver::global()
            .open_read_head(
                &path,
                &lore_io::OpenOptions::new().read(true),
                BUCKET_HEAD_READ,
            )
            .await
        {
            Ok(parts) => parts,
            Err(err) => {
                if err.kind() == ErrorKind::NotFound {
                    return Ok(BucketFile::empty());
                }
                return Err(LocalImmutableStoreError::internal_with_context(
                    err,
                    "Failed to deserialize storage bucket",
                ));
            }
        };

        // Recover from corruption by resetting the bucket. Future-version sentinels and
        // unreadable files propagate untouched — deleting a file a newer binary wrote, or one a
        // transient error kept from being read, would destroy its entries.
        // The reason is taken out of the match before the recovery is awaited: a match left open
        // across it would hold the whole result in the future.
        let reason = match Self::deserialize_file_content(
            &path,
            &file,
            metadata.len() as usize,
            &head,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(DeserializeFileError::FutureVersion(version)) => {
                return Err(LocalImmutableStoreError::internal_with_context(
                    io::Error::other(format!(
                        "Incompatible store version {version} encountered, please update your client to the latest version"
                    )),
                    "Failed to deserialize storage bucket",
                ));
            }
            Err(DeserializeFileError::Corrupt(reason)) => reason,
            Err(DeserializeFileError::Unreadable(err)) => {
                return Err(LocalImmutableStoreError::internal_with_context(
                    err,
                    "Failed to read storage bucket",
                ));
            }
        };
        Self::recover_corrupt_bucket(&path, reason).await
    }

    /// Buckets that fit inside the composite open's head bytes — the
    /// common case — parse straight from the head with no further
    /// dispatch. Larger buckets do one vectored read scattering the
    /// sorted index and entries straight into their final chunk
    /// allocations, and read the packfile sizes after the entries
    /// separately. Legacy layouts (before `LastAccessInEntry`) parse the
    /// remainder with entry conversion.
    async fn deserialize_file_content(
        path: &Path,
        file: &lore_io::IoFile,
        file_size: usize,
        head: &Bytes,
    ) -> Result<BucketFile, DeserializeFileError> {
        let header_size = size_of::<ImmutableStoreHeader>();
        if head.len() < header_size {
            return Err(DeserializeFileError::Corrupt(format!(
                "file size {file_size} smaller than header size {header_size}"
            )));
        }
        let mut header = ImmutableStoreHeader::new_zeroed();
        header.as_mut_bytes().copy_from_slice(&head[..header_size]);

        // Version is validated before any count math — a future format with a different
        // per_entry_size could otherwise produce a spurious count mismatch and trigger
        // recovery on a perfectly valid newer-format file.
        if (header.version > ImmutableStoreVersion::LazyFanOut as u32) && (header.version < 0xFFFF)
        {
            return Err(DeserializeFileError::FutureVersion(header.version));
        }
        let per_entry_size = match header.version {
            // Rust enum discriminants are painful, use if construct trick
            x if x == ImmutableStoreVersion::LazyFanOut as u32
                || x == ImmutableStoreVersion::LastAccessInEntry as u32 =>
            {
                size_of::<u32>() /* sorted index */
                        + size_of::<ImmutableStoreEntry>() /* entry */
            }
            x if (x == ImmutableStoreVersion::PackfilePerGroup as u32)
                || (x == ImmutableStoreVersion::LastAccessTimestamps as u32) =>
            {
                size_of::<u32>() /* sorted index */
                        + size_of::<ImmutableStoreEntryBeforeLastAccess>() /* entry */
                        + size_of::<u32>() /* last access timestamp */
            }
            x if x == ImmutableStoreVersion::Initial as u32 => {
                size_of::<u32>() /* sorted index */ + size_of::<ImmutableStoreEntry>() /* entry */
            }
            _ => {
                return Err(DeserializeFileError::Corrupt(format!(
                    "invalid store version {}",
                    header.version
                )));
            }
        };

        let expected_count = (file_size - header_size) / per_entry_size;
        if expected_count == 0 {
            return Ok(BucketFile::empty());
        }

        if header.count != expected_count as u32 {
            return Err(DeserializeFileError::Corrupt(format!(
                "bad index file, unexpected count {} when expecting {} for index file {path:?}",
                header.count, expected_count,
            )));
        }

        let upgrade_packfile = header.version < ImmutableStoreVersion::PackfilePerGroup as u32;

        // LazyFanOut keeps the LastAccessInEntry layout, so any version ≥ LastAccessInEntry uses the new entry layout.
        if header.version >= ImmutableStoreVersion::LastAccessInEntry as u32 {
            let entries_end = header_size + expected_count * per_entry_size;
            if file_size <= head.len() {
                let mut reader = &head[header_size..];
                let sorted_index =
                    GrowVec::read_from(&mut reader, expected_count).map_err(|err| {
                        DeserializeFileError::Corrupt(format!("read sorted index: {err}"))
                    })?;
                let entry = GrowVec::read_from(&mut reader, expected_count)
                    .map_err(|err| DeserializeFileError::Corrupt(format!("read entries: {err}")))?;
                return Ok(BucketFile {
                    sorted_index,
                    entry,
                    upgrade_packfile,
                    mark_dirty: false,
                    sizes: header.recorded_sizes(head.slice(entries_end..file_size)),
                });
            }
            let segments = ImmutableBucketSegments {
                // SAFETY: the scatter below fills every byte of both vectors or fails, and a
                // failure drops them here rather than returning them.
                sorted_index: unsafe { GrowVec::new_unzeroed_with_size(expected_count) },
                entry: unsafe { GrowVec::new_unzeroed_with_size(expected_count) },
            };
            let segments = file
                .read_exact_vectored_at(segments, header_size as u64)
                .await
                .map_err(|err| bucket_read_error(err, "read bucket data"))?;
            let trailer = if header.packfile_sizes & SIZES_RECORDED != 0 && file_size > entries_end
            {
                file.read_exact_at(file_size - entries_end, entries_end as u64)
                    .await
                    .map_err(|err| bucket_read_error(err, "read packfile sizes"))?
            } else {
                Bytes::new()
            };
            return Ok(BucketFile {
                sorted_index: segments.sorted_index,
                entry: segments.entry,
                upgrade_packfile,
                mark_dirty: false,
                sizes: header.recorded_sizes(trailer),
            });
        }

        let remainder;
        let mut reader: &[u8] = if file_size <= head.len() {
            &head[header_size..]
        } else {
            remainder = file
                .read_exact_at(file_size - header_size, header_size as u64)
                .await
                .map_err(|err| bucket_read_error(err, "read bucket data"))?;
            &remainder
        };
        let sorted_index = GrowVec::read_from(&mut reader, expected_count)
            .map_err(|err| DeserializeFileError::Corrupt(format!("read sorted index: {err}")))?;
        let entry_old: GrowVec<ImmutableStoreEntryBeforeLastAccess, CHUNK_SIZE_ENTRY> =
            GrowVec::read_from(&mut reader, expected_count).map_err(|err| {
                DeserializeFileError::Corrupt(format!("read legacy entries: {err}"))
            })?;

        let mut entry = GrowVec::new();
        for entry_old in entry_old.iter() {
            entry.push(ImmutableStoreEntry {
                address: entry_old.address,
                partition: entry_old.partition,
                data: ImmutableData {
                    flags: entry_old.data.flags,
                    size_payload: entry_old.data.size_payload,
                    size_content: entry_old.data.size_content,
                    pack_offset: entry_old.data.pack_offset,
                    pack_file: entry_old.data.pack_file,
                    last_access: 0,
                },
            });
        }

        Ok(BucketFile {
            sorted_index,
            entry,
            upgrade_packfile,
            mark_dirty: true,
            sizes: RecordedSizes::Unrecorded,
        })
    }

    /// Drop the corrupt file and return an empty bucket. Pack file payloads survive
    /// until compaction reclaims the now-orphaned ranges.
    async fn recover_corrupt_bucket(
        path: &Path,
        reason: String,
    ) -> Result<BucketFile, LocalImmutableStoreError> {
        lore_base::lore_warn!(
            "Resetting corrupt immutable bucket {} after deserialize failure: {reason}. Bucket lookup state lost; pack file payloads remain until compaction.",
            path.display()
        );
        if let Err(err) = lore_io::IoDriver::global().remove_file(path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(LocalImmutableStoreError::internal_with_context(
                err,
                "Failed to remove corrupt storage bucket",
            ));
        }
        Ok(BucketFile::empty())
    }

    /// Write the bucket to a temporary file renamed over `path`, so that a process stopped during
    /// the write leaves the file it last wrote. The bucket is copied and its lock released before
    /// the write, see [`ImmutableBucketWrite`]. With `sync_data`, the group's packfiles are synced
    /// between the copy and the write, so that the file points only at payload bytes stored, and
    /// the file is synced before the rename and its directory after. Without, the file records the
    /// sizes of the packfiles a crash could leave shorter than an entry pointing into them, see
    /// [`crate::PackStore::unsettled_sizes`], or, when it cannot, see [`recordable_sizes`], the
    /// packfiles are synced as with `sync_data`.
    async fn serialize_files(
        bucket: OwnedRwLockReadGuard<ImmutableStoreBucket, ImmutableStoreBucket>,
        group: Arc<ImmutableStoreGroup>,
        bucket_index: usize,
        path: PathBuf,
        changed: bool,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        // A failed write re-arms the flag it claimed, so a bucket flushed only for its access
        // stamps stays a stamp rather than becoming a change that keeps the store claimed.
        let rearm = if changed {
            &group.dirty[bucket_index]
        } else {
            &group.soft_dirty[bucket_index]
        };
        let mut failure_guard = SerializeFailureGuard::new(rearm);

        let count = bucket.entry.len();
        if bucket.sorted_index.len() != count {
            return Err(LocalImmutableStoreError::internal_with_context(
                io::Error::other("Immutable store entry and index count mismatch"),
                "Failed to serialize storage bucket",
            ));
        }

        let mut header = ImmutableStoreHeader::new_zeroed();
        header.version = group.serialize_version.load(atomic::Ordering::Relaxed);
        header.count = count as u32;

        let mut contents = ImmutableBucketWrite::copy(&bucket);
        drop(bucket);
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Serializing).await;
        // The sizes are held across one await, with no future around it: held across more, or a
        // larger one, they would make this the largest state of the future.
        let mut recorded = false;
        if !sync_data {
            let mut sizes = [PackfileSize::default(); 1 + TRAILER_SIZES];
            let unsettled = group.packstore.unsettled_sizes(&mut sizes).await;
            if let Some(sizes) = recordable_sizes(&sizes, unsettled) {
                contents.finish(header, sizes);
                recorded = true;
            }
        }
        if !recorded {
            group.flush_packstore(true).await?;
            contents.finish(header, &[]);
        }

        if let Some(parent_path) = path.parent()
            && !parent_path.exists()
        {
            let _ = lore_io::IoDriver::global()
                .create_dir_all(parent_path)
                .await;
        }

        let file_options = lore_io::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true);
        let completion = crate::local::fan_out::write_share(&group.writes);
        #[cfg(feature = "test-util")]
        let completion = crate::test_util::park_store_write(completion).await;
        lore_io::IoDriver::global()
            .write_file_segments_atomic(
                crate::local::fan_out::temporary_path(&path),
                path,
                &file_options,
                contents,
                sync_data,
                completion,
            )
            .await
            .internal("Failed to serialize storage bucket")?;
        failure_guard.success();

        Ok(())
    }

    async fn deserialize(
        &mut self,
        group: &ImmutableStoreGroup,
        path: &Path,
        group_index: usize,
        bucket_index: usize,
        gc_counters: Option<&Arc<crate::maintenance::GcCounters>>,
    ) -> Result<(), LocalImmutableStoreError> {
        if self.deserialized {
            return Ok(());
        }

        // Ensure only one serialization/deserialization of this bucket is happening at any given time
        let _lock = self.serialize_lock.lock().await;

        if self.deserialized {
            return Ok(());
        }

        let path = format_bucket_path(path, group_index, bucket_index);

        let mut file = Self::deserialize_files(path).await?;
        let cleared = group
            .clear_lost_payloads(&mut file, group_index, bucket_index)
            .await?;

        self.sorted_index = file.sorted_index;
        self.entry = file.entry;
        self.upgrade_packfile = file.upgrade_packfile;
        self.deserialized = true;

        if let Some(gc) = gc_counters {
            gc.add_loaded_fragments(self.entry.len());
        }

        if file.mark_dirty || cleared {
            group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        }

        atomic::fence(atomic::Ordering::Release);

        Ok(())
    }

    /// Claim the bucket's dirty mark and write it, see [`Self::serialize_files`]. A bucket whose
    /// mark another write claimed is left to that write.
    async fn serialize(
        bucket: OwnedRwLockReadGuard<ImmutableStoreBucket, ImmutableStoreBucket>,
        group: Arc<ImmutableStoreGroup>,
        path: &Path,
        group_index: usize,
        bucket_index: usize,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        // An emptied bucket is written out like any other. Skipping it would leave the file it
        // was last written to on disk, and reopening the store would load the entries back.
        //
        // Ensure only one serialization/deserialization of this bucket is happening at any given time
        let _lock = bucket.serialize_lock.clone().lock_owned().await;

        if group.fan_out_uncommitted() {
            return Err(LocalImmutableStoreError::internal(
                "Bucket left dirty for the uncommitted fan-out of its group",
            ));
        }

        // Atomically flip both flags from true to false; if neither was set another
        // flush task has already claimed this bucket. A bucket that is only
        // soft-dirty — an access stamp and nothing else — is still worth writing, which
        // is how a read's stamp reaches disk.
        let was_dirty = group.dirty[bucket_index].swap(false, atomic::Ordering::Acquire);
        let was_stamped = group.soft_dirty[bucket_index].swap(false, atomic::Ordering::Acquire);
        if !was_dirty && !was_stamped {
            return Ok(());
        }

        lore_base::lore_trace!(
            "Serialize immutable store group {group_index} bucket {bucket_index}"
        );

        let path = format_bucket_path(path, group_index, bucket_index);

        Self::serialize_files(bucket, group, bucket_index, path, was_dirty, sync_data).await
    }

    /// Serialize the bucket to its `.new` twin during a fan-out commit. Differs from the regular
    /// `serialize` path in two ways: (1) bypasses the short-circuit that skips a bucket with
    /// neither `dirty` nor `soft_dirty` set, because every `[0..committed_level]` bucket must be
    /// rewritten at the new layout to overwrite stale level-N files even if it is clean; (2)
    /// always clears both flags after claiming ownership. A write to the bucket takes its write
    /// lock, which the caller's read lock excludes, so such a write lands after the release,
    /// re-sets dirty and is picked up by the next flush — matching the regular `serialize` path's
    /// semantics. A last-access stamp is the exception, written under the read lock, which is why
    /// the claim acquires.
    pub async fn serialize_to_new(
        bucket: OwnedRwLockReadGuard<ImmutableStoreBucket, ImmutableStoreBucket>,
        group: Arc<ImmutableStoreGroup>,
        path: &Path,
        group_index: usize,
        bucket_index: usize,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        let _lock = bucket.serialize_lock.clone().lock_owned().await;

        group.dirty[bucket_index].swap(false, atomic::Ordering::Acquire);
        group.soft_dirty[bucket_index].swap(false, atomic::Ordering::Acquire);

        let final_path = format_bucket_path(path, group_index, bucket_index);
        let new_path = {
            let mut p = final_path.into_os_string();
            p.push(crate::local::fan_out::BUCKET_NEW_SUFFIX);
            PathBuf::from(p)
        };

        Self::serialize_files(bucket, group, bucket_index, new_path, true, sync_data).await
    }
}

/// The packfile sizes a bucket file records, the first `unsettled` of `sizes`, see
/// [`crate::PackStore::unsettled_sizes`], or `None` when there were more than `sizes` holds, or the
/// first id has no room in the header, and the group's packfiles are to be synced instead.
fn recordable_sizes(sizes: &[PackfileSize], unsettled: Option<usize>) -> Option<&[PackfileSize]> {
    let sizes = &sizes[..unsettled?];
    sizes
        .first()
        .is_none_or(|first| first.id <= SIZES_ID_MASK)
        .then_some(sizes)
}

impl ImmutableStoreGroup {
    async fn flush_packstore(&self, sync_data: bool) -> Result<(), LocalImmutableStoreError> {
        self.packstore.flush_all(sync_data).await.map_err(|err| {
            LocalImmutableStoreError::internal_with_context(err, "Failed syncing packfiles")
        })
    }

    /// Clear the payload location of each entry of `file` that may point past the end of its
    /// packfile, as after a crash lost the end of a packfile the file points into, so that a later
    /// store of the payload writes it again rather than reading what a write after the end left
    /// there. The entries are scanned only when a packfile is shorter, as the store opened it, than
    /// the file recorded, or its recorded sizes are torn. A file recording none is trusted. Returns
    /// whether a location was cleared.
    async fn clear_lost_payloads(
        &self,
        file: &mut BucketFile,
        group_index: usize,
        bucket_index: usize,
    ) -> Result<bool, LocalImmutableStoreError> {
        let packstore_error = |err| {
            LocalImmutableStoreError::internal_with_context(err, "Failed reading packfile lengths")
        };
        let lost = match &file.sizes {
            RecordedSizes::Unrecorded => false,
            RecordedSizes::Recorded { first, rest } => {
                let rest = rest
                    .as_chunks::<{ size_of::<PackfileSize>() }>()
                    .0
                    .iter()
                    .filter_map(|size| PackfileSize::read_from_bytes(size).ok());
                self.packstore
                    .short_of(first.iter().copied().chain(rest))
                    .await
                    .map_err(packstore_error)?
            }
            RecordedSizes::Torn => true,
        };
        if !lost {
            return Ok(false);
        }
        let opened = self
            .packstore
            .opened_lengths()
            .await
            .map_err(packstore_error)?;
        let mut cleared = 0;
        for entry in file.entry.iter_mut() {
            let data = &mut entry.data;
            let Some(index) = (data.pack_file as usize).checked_sub(1) else {
                continue;
            };
            let end = u64::from(data.pack_offset) + u64::from(data.size_payload);
            if end > u64::from(opened.get(index).copied().unwrap_or(0)) {
                data.pack_file = 0;
                data.pack_offset = 0;
                data.flags &= !FragmentFlags::PayloadStoredLocal.bits();
                cleared += 1;
            }
        }
        if cleared > 0 {
            lore_base::lore_warn!(
                "Cleared {cleared} payload locations past the end of their packfile in group {group_index} bucket {bucket_index}, as left by a crash; the payloads are stored again when next fetched"
            );
        }
        Ok(cleared > 0)
    }

    /// Whether the group's buckets can only be written by a flush of the whole group. It fanned
    /// out to a layout no commit recorded on disk, and writing a bucket as it is in memory would
    /// mix two layouts, or a commit failed after writing its `level.pending`, and completing it
    /// renames the commit's `.new` files over any bucket file written meanwhile. Read under the
    /// flush lock, which every change to the levels holds.
    fn fan_out_uncommitted(&self) -> bool {
        let committed = self.committed_level.load(atomic::Ordering::Relaxed);
        self.pending_level.load(atomic::Ordering::Relaxed) != 0
            || (committed != 0 && committed != self.bucket_count.load(atomic::Ordering::Relaxed))
    }

    /// Whether a flush can skip the group: no bucket is flagged and no fan-out awaits its commit.
    /// Read under the flush lock, under which every bucket write claims a bucket's flag and flags it
    /// again when the write fails or is dropped, see [`SerializeFailureGuard`]; read without the
    /// lock, a group whose write is failing reads as clean.
    fn nothing_to_flush(&self) -> bool {
        !self.fan_out_uncommitted() && !(0..self.dirty.len()).any(|bucket| self.needs_flush(bucket))
    }

    /// Writes out the group's flagged buckets after `delay` seconds, each after rolling forward a
    /// fan-out commit the group left pending and committing a level marker it has yet to write.
    ///
    /// **Claimed before anything is written, with a guard of its own, and only as far as the
    /// work needs**: to write for changes, to read for access stamps alone. This wakes long
    /// after the operation that scheduled it, by which time the span that operation ran in may
    /// be gone, or may end while this runs. Writing without a claim would put bucket files and a
    /// level marker on disk with no flock and no epoch advance, over whatever another process
    /// has since written, and tell nobody. A guard of its own keeps the flock for the whole pass
    /// whether or not a span covers it, and its drop releases the flock once the pass has left
    /// nothing flagged.
    #[lore_macro::test_pub]
    async fn flush_delayed(weak_ref: Weak<LocalImmutableStore>, group_index: usize, delay: u64) {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        #[cfg(feature = "test-util")]
        let _task = crate::test_util::store_task(crate::test_util::StoreTask::DelayedFlush);
        if let Some(store) = weak_ref.upgrade()
            && let Some(path) = store.path.as_ref()
        {
            let group = store.group[group_index].clone();
            // Cleared before a single flag is read: a bucket flagged from here on schedules
            // the next sweep, and one flagged before this is seen by this one.
            group.scheduled.store(false, atomic::Ordering::Relaxed);
            // Looked at before claiming, because a sweep that finds nothing — the usual
            // case once a command's own flush has run — must not claim to write: that
            // advances the epoch and tells every other process its state is worthless.
            // The flags are read again under the claim below, and a claim that finds
            // the store stale reloads first, which empties the buckets and leaves
            // nothing to write. That is the right answer: state already invalidated must
            // not reach disk.
            let intent = match group.pending_flush() {
                PendingFlush::Changes => Intent::Write,
                PendingFlush::Stamps => Intent::Read,
                PendingFlush::Nothing => {
                    store.note_flushed();
                    return;
                }
            };
            let Ok(claim) = store.claim(intent).await else {
                return;
            };
            let _claim = claim;

            for bucket_index in 0..group.bucket.len() {
                // Atomic pre-check avoids acquiring the bucket RwLock for clean buckets.
                if !group.needs_flush(bucket_index) {
                    continue;
                }
                let Some(bucket) = group.try_bucket(bucket_index).cloned() else {
                    continue;
                };
                // Guard this bucket write against a concurrent two-phase commit's
                // rename. Taken before the bucket guard to keep the lock order
                // flush_lock -> bucket RwLock -> serialize_lock uniform with `flush_all`.
                let flush_guard =
                    crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;

                // Re-check under the lock: a flush that ran while we waited may already
                // have written this bucket. The read above happened before the lock, so
                // it is stale.
                if !group.needs_flush(bucket_index) {
                    continue;
                }

                if let Err(err) = crate::local::fan_out::roll_pending_commit_forward(
                    &group.writes,
                    &group.committed_level,
                    &group.pending_level,
                    path,
                    group_index,
                    false,
                )
                .await
                {
                    lore_base::lore_warn!(
                        "Failed to roll the fan-out commit of group {group_index} forward: {err}"
                    );
                    return;
                }
                if let Err(err) = crate::local::fan_out::commit_if_initial_level(
                    &flush_guard,
                    &group.writes,
                    &group.committed_level,
                    &group.pending_level,
                    &group.bucket_count,
                    path,
                    group_index,
                    false,
                )
                .await
                {
                    lore_base::lore_warn!(
                        "Failed to write level marker for group {group_index}: {err}"
                    );
                    return;
                }

                let bucket = bucket.read_owned().await;
                let _ = ImmutableStoreBucket::serialize(
                    bucket,
                    group.clone(),
                    path,
                    group_index,
                    bucket_index,
                    false, /* Don't wait and sync all data to storage media */
                )
                .await;
            }
        }
    }
}

impl LocalImmutableStore {
    /// Set the automatic-GC caps (from create options) on this store's load-driven GC
    /// counters. Caps of 0 leave the corresponding trigger disabled — which is how
    /// read-only / `--no-gc` opens (whose options carry no caps) stay inert.
    pub fn set_gc_caps(&self, max_size: usize, max_capacity: usize, sync_data: bool) {
        self.gc_counters.set_caps(max_size, max_capacity, sync_data);
    }

    /// Releases the flock's hold on unflushed state, if there is none left.
    ///
    /// Called wherever a flush finishes. Checks the buckets rather than trusting
    /// the caller, because a flush can race a write that dirties a bucket behind
    /// it, and releasing then would leave modifications no other process can see.
    #[lore_macro::test_pub]
    fn note_flushed(&self) {
        if let Some(lock) = self.lock.as_ref() {
            lock.release_if_clean();
        }
    }

    /// Claims this store's cross-process lock for an operation, reloading first if
    /// another process changed the store while this one held nothing.
    ///
    /// Inside a span this costs two relaxed loads and takes no lock: the span's guard
    /// keeps the flock, and the store answers `None`, or in test builds a [`Claim`]
    /// counting the operation. Only an operation outside any span claims for itself, and
    /// holds the guard for as long as its work runs. `None` is also the answer for an
    /// in-memory store, which has no flock and nothing on disk for anyone to change.
    ///
    /// # Errors
    ///
    /// [`crate::immutable_store::StoreError`] if the lock cannot be taken, or if the
    /// reload a stale acquisition demands cannot read the store.
    pub async fn hold(
        &self,
        intent: Intent,
    ) -> Result<Option<Claim>, crate::immutable_store::StoreError> {
        let Some(lock) = self.lock.as_ref() else {
            return Ok(None);
        };
        if lock.covers(intent) {
            return Ok(lock.covered(intent));
        }
        let guard = lock
            .acquire(intent)
            .await
            .internal("Failed to acquire store lock")?;
        self.settle_claim(guard)
            .await
            .map(|guard| Some(Claim::Held(guard)))
    }

    /// Claims this store with `intent` and a counted guard of its own: for a span of use, as
    /// [`crate::immutable_store::ImmutableStore::hold_for_command`] describes, or for a pass
    /// whose guard has to keep the flock however long it runs.
    ///
    /// Always a counted guard, where [`Self::hold`] answers `None` inside a span: the
    /// span is what the operations inside it rely on, so it must be held even when
    /// another span already is.
    ///
    /// # Errors
    ///
    /// As [`Self::hold`].
    async fn claim(
        &self,
        intent: Intent,
    ) -> Result<Option<StoreGuard>, crate::immutable_store::StoreError> {
        let Some(lock) = self.lock.as_ref() else {
            return Ok(None);
        };
        let guard = lock
            .acquire(intent)
            .await
            .internal("Failed to acquire store lock")?;
        self.settle_claim(guard).await.map(Some)
    }

    /// Claims the store for a background pass as [`Self::hold`] does, but without
    /// waiting — see [`StoreLock::try_acquire`].
    ///
    /// # Errors
    ///
    /// As [`Self::hold`].
    async fn try_hold(
        &self,
        intent: Intent,
    ) -> Result<BackgroundClaim, crate::immutable_store::StoreError> {
        let Some(lock) = self.lock.as_ref() else {
            return Ok(BackgroundClaim::Unlocked);
        };
        let Some(guard) = lock
            .try_acquire(intent)
            .await
            .internal("Failed to acquire store lock")?
        else {
            return Ok(BackgroundClaim::Busy);
        };
        self.settle_claim(guard).await.map(BackgroundClaim::Held)
    }

    /// Reloads the store if `guard` reports it stale.
    ///
    /// # Errors
    ///
    /// [`crate::immutable_store::StoreError`] if the reload cannot read the store.
    async fn settle_claim(
        &self,
        mut guard: StoreGuard,
    ) -> Result<StoreGuard, crate::immutable_store::StoreError> {
        if guard.is_stale() {
            // Boxed: a reload is rare, and carried inline its state would be part of every
            // operation's future.
            Box::pin(self.refresh())
                .await
                .forward::<crate::immutable_store::StoreError>(
                    "Failed to reload a store another process changed",
                )?;
            // Only now is the epoch this store's own. Returning early above leaves it
            // behind, so the next acquisition reports stale and retries the reload —
            // and until then no other operation joins, because a stale guard holds
            // the directory's gate.
            guard.note_refreshed();
        }
        Ok(guard)
    }

    /// Discards what the groups cache and re-reads the store from disk.
    ///
    /// Called only from [`Self::hold`], on the acquisition that found the epoch moved
    /// — the transition into the first operation, so nothing is in flight.
    ///
    /// **Invalidates the groups where they stand.** The vector's length and every
    /// group's identity are fixed, so only what each group caches has to go, and all
    /// of that is interior-mutable — which is what leaves
    /// [`LocalImmutableStore::group`] reachable without a lock.
    ///
    /// **The two halves are ordered so the store cannot be left half-refreshed.** The
    /// survey does every fallible thing — it reads markers and rolls forward pending
    /// level transitions — and touches no in-memory state, so a failure returns with
    /// the store exactly as it was. Applying it does no I/O and cannot fail. A torn
    /// set of groups, some invalidated and some not, is therefore unrepresentable
    /// rather than merely unlikely, and such a set would be flushed back over a store
    /// this process cannot describe.
    ///
    /// The info file is read with the survey, before anything is applied. A store with
    /// none is given the info an open would write for it, in memory only: the claim this
    /// runs under may be a read claim, and the next open writes the file.
    ///
    /// # Errors
    ///
    /// [`LocalImmutableStoreError`] if the store's groups cannot be surveyed, or its
    /// info file cannot be read.
    #[lore_macro::test_pub]
    async fn refresh(&self) -> Result<(), LocalImmutableStoreError> {
        let survey = Self::survey_groups(self.path.as_deref(), &self.settings).await?;
        let info = match self.path.as_deref() {
            Some(path) => Some(Self::read_info(path).await?.unwrap_or_else(|| {
                // As `get_or_init_disk_info` builds the file an open writes.
                let mut info = ImmutableStoreInfo::default();
                let start = Self::oodle_migration_start(&survey);
                if start != -1 {
                    info.next_group_index_to_migrate_oodle = start;
                }
                info
            })),
            None => None,
        };
        // Everything the store records about having loaded its contents goes with the
        // contents, once for the whole store and before any group re-reads.
        //
        // The counters are cumulative and every group is about to load its packfiles
        // again. The latch is what makes `deserialize_all_buckets` a no-op: left set
        // over a reload, the three callers that need every bucket — packfile
        // compaction, verify, and the legacy packfile upgrade — would each be handed
        // a store they believe is fully loaded and which holds nothing.
        self.gc_counters.reset_loaded();
        self.deserialized_all
            .store(false, atomic::Ordering::Relaxed);
        if let Some(info) = info {
            *self.info.write().await = info;
        }
        for (group, level) in self.group.iter().zip(survey.levels.iter()) {
            group
                .invalidate(level.count, level.committed, survey.serialize_version)
                .await;
        }
        Ok(())
    }

    /// Reads every group's level marker and decides the store's serialize version.
    ///
    /// Shared by [`Self::build_groups`] and [`Self::refresh`], which need the same
    /// answer and must not disagree about it. It is also the whole of the fallible
    /// part of a refresh, which is what lets that do all its I/O before it touches a
    /// group.
    ///
    /// Not read-only despite the name: it rolls forward a pending level transition it
    /// finds, which is a rename on disk. That is safe to do while another process
    /// holds a stale view, because whoever left the transition pending had already
    /// advanced the epoch when it took its write hold.
    ///
    /// # Errors
    ///
    /// [`LocalImmutableStoreError`] if a group's pending level transition cannot be
    /// recovered or its marker cannot be read.
    async fn survey_groups(
        immutable_path: Option<&PathBuf>,
        settings: &ImmutableStoreSettings,
    ) -> Result<GroupSurvey, LocalImmutableStoreError> {
        // Groups are surveyed before their levels are decided: the decision needs the store's
        // serialize version, which is only known once every marker has been read.
        // Every group is read at once. Each group is one recovery check and one marker read, and
        // awaiting them in turn puts a whole store open behind `GROUP_COUNT` round trips to the
        // I/O engine — a cost every process that opens a store pays before it does any work. The
        // groups are independent directories, so the reads overlap and the engine's thread budget
        // is what paces them. Completions arrive in whatever order the reads finish, so each task
        // carries the group it answers for.
        let mut group_levels = vec![GroupLevel::Unwritten; GROUP_COUNT];
        if let Some(path) = immutable_path {
            let index_path = path.as_path().join("index");
            // One listing says which groups exist, so only those are read; the rest stay
            // unwritten. Without one, every group is probed.
            let existing =
                crate::local::fan_out::existing_group_dirs(&index_path, GROUP_COUNT).await;
            let mut tasks = JoinSet::new();
            for group_index in 0..GROUP_COUNT {
                if existing
                    .as_ref()
                    .is_some_and(|existing| !existing[group_index])
                {
                    continue;
                }
                let group_path = crate::local::fan_out::group_dir_path(&index_path, group_index);
                lore_base::lore_spawn!(tasks, async move {
                    // A group directory that cannot be read fails the open: taken for one never
                    // written, the group would be laid out at the initial level, where lookups
                    // miss its bucket files, and a write would commit that level over them.
                    match crate::fs_util::exists(&group_path).await {
                        Ok(true) => {}
                        Ok(false) => return (group_index, Ok(GroupLevel::Unwritten)),
                        Err(err) => {
                            return (
                                group_index,
                                Err(LocalImmutableStoreError::internal_with_context(
                                    err,
                                    "Failed to read group directory",
                                )),
                            );
                        }
                    }
                    // Roll forward any pending fan-out commit before reading the marker. After this returns the marker reflects the post-recovery state.
                    if let Err(err) =
                        crate::local::fan_out::recover_level_transition(&group_path, false, None)
                            .await
                    {
                        return (
                            group_index,
                            Err(LocalImmutableStoreError::internal_with_context(
                                err,
                                "Failed to recover pending level transition for group",
                            )),
                        );
                    }

                    let level = crate::local::fan_out::read_group_level(&group_path)
                        .await
                        .map_err(|err| {
                            LocalImmutableStoreError::internal_with_context(
                                err,
                                "Failed to read level marker for group",
                            )
                        });
                    (group_index, level)
                });
            }

            // Drained to the end rather than returned from at the first failure: dropping
            // the set would abort the other groups' surveys part way, and a survey rolls
            // pending level transitions forward.
            let mut failure = None;
            while let Some(joined) = tasks.join_next().await {
                let surveyed = joined
                    .map_err(|err| {
                        LocalImmutableStoreError::internal_with_context(err, "level marker task")
                    })
                    .and_then(|(group_index, level)| Ok((group_index, level?)));
                match surveyed {
                    Ok((group_index, level)) => group_levels[group_index] = level,
                    Err(err) => {
                        if failure.is_none() {
                            failure = Some(err);
                        }
                    }
                }
            }
            if let Some(err) = failure {
                return Err(err);
            }
        }

        let any_marker_seen = group_levels
            .iter()
            .any(|level| matches!(level, GroupLevel::Marked(_)));
        // Group directories also hold packfiles, so a store stopped before its first index flush
        // has them without a bucket file, and is new.
        let index_written = group_levels
            .iter()
            .any(|level| !matches!(level, GroupLevel::Unwritten));

        // Determine serialize_version per Decision 8. Fresh stores, stores with markers, and
        // existing stores with bucket files at any older version (v1-v3) all go to LazyFanOut.
        // Existing stores at the current pre-fan-out version (v4 LastAccessInEntry) with no
        // markers stay at v4 for backward compatibility (old binaries can still read them).
        let any_older_bucket_seen = if index_written
            && !any_marker_seen
            && let Some(path) = immutable_path
        {
            let mut idx = path.as_path().to_path_buf();
            idx.push("index");
            detect_any_older_immutable_bucket(&idx)
        } else {
            false
        };
        let serialize_version: u32 = if !index_written || any_marker_seen || any_older_bucket_seen {
            ImmutableStoreVersion::LazyFanOut as u32
        } else {
            ImmutableStoreVersion::LastAccessInEntry as u32
        };

        let unwritten_level = crate::local::fan_out::unwritten_group_level(
            serialize_version == ImmutableStoreVersion::LazyFanOut as u32,
            settings.initial_fan_out_level,
        );

        #[cfg(feature = "oodle")]
        let highest_materialized = group_levels
            .iter()
            .rposition(|level| !matches!(level, GroupLevel::Unwritten))
            .map_or(-1, |group_index| group_index as i32);
        let levels = group_levels
            .into_iter()
            .map(|level| match level {
                GroupLevel::Marked(level) => GroupLevels {
                    count: level,
                    committed: level,
                },
                GroupLevel::PreFanOut => GroupLevels {
                    count: BUCKET_COUNT,
                    committed: 0,
                },
                GroupLevel::Unwritten => GroupLevels {
                    count: unwritten_level,
                    committed: 0,
                },
            })
            .collect();
        Ok(GroupSurvey {
            serialize_version,
            levels,
            #[cfg(feature = "oodle")]
            highest_materialized,
        })
    }

    /// Builds one group per bucket group, at the levels `survey` read.
    ///
    /// For [`Self::new`]. A store that finds the epoch moved goes to
    /// [`Self::refresh`], which applies a fresh [`Self::survey_groups`] to the groups
    /// it already has.
    fn build_groups(
        immutable_path: Option<&PathBuf>,
        settings: &ImmutableStoreSettings,
        gc_counters: &Arc<crate::maintenance::GcCounters>,
        survey: GroupSurvey,
    ) -> Vec<Arc<ImmutableStoreGroup>> {
        // With per group packstores the minimum number of packfiles per group
        // can be set to 1 - and will then grow dynamically as needed
        const MIN_PACKFILE_COUNT: usize = 1;

        let mut group = Vec::with_capacity(GROUP_COUNT);
        for (group_index, level) in survey.levels.into_iter().enumerate() {
            let packpath = immutable_path.map(|path| {
                let mut path = path.clone();
                path.reserve(16);
                path.push("index");
                crate::local::fan_out::push_group_dir(&mut path, group_index);
                path
            });
            group.push(Arc::new(ImmutableStoreGroup {
                bucket: [const { OnceLock::new() }; BUCKET_COUNT],
                dirty: std::array::from_fn(|_| AtomicBool::new(false)),
                soft_dirty: std::array::from_fn(|_| AtomicBool::new(false)),
                bucket_count: std::sync::atomic::AtomicUsize::new(level.count),
                serialize_version: std::sync::atomic::AtomicU32::new(survey.serialize_version),
                fan_out_threshold: settings.fan_out_threshold,
                committed_level: std::sync::atomic::AtomicUsize::new(level.committed),
                pending_level: std::sync::atomic::AtomicUsize::new(0),
                flush_lock: Arc::new(Mutex::new(())),
                writes: Arc::default(),
                packstore: crate::PackStore::new(
                    packpath,
                    MIN_PACKFILE_COUNT,
                    Some(gc_counters.clone()),
                ),
                flush: Mutex::new(JoinSet::new()),
                scheduled: AtomicBool::new(false),
                payload_gate: PayloadGate::default(),
            }));
        }
        group
    }

    /// The group an Oodle migration of a store with no info file starts from, given what
    /// `survey` found on disk.
    fn oodle_migration_start(_survey: &GroupSurvey) -> i32 {
        // Assume that any store with groups that exist on disk were made by another
        // Oodle-enabled binary before this one, and therefore may have data to migrate
        #[cfg(feature = "oodle")]
        {
            _survey.highest_materialized
        }
        #[cfg(not(feature = "oodle"))]
        {
            -1
        }
    }

    /// The store's info file, or `None` where it has none.
    ///
    /// # Errors
    ///
    /// [`LocalImmutableStoreError`] if the file cannot be read, or is not an info file
    /// this build can read.
    async fn read_info(
        path: &Path,
    ) -> Result<Option<ImmutableStoreInfo>, LocalImmutableStoreError> {
        read_info_file(&info_path_for_store_root(path))
            .await
            .map_err(|err| {
                LocalImmutableStoreError::internal_with_context(
                    err,
                    "Failed to load store info file",
                )
            })
    }

    /// Reads the info file of the store being opened, giving it one where it has none.
    ///
    /// Read under the claim the open holds. Giving the store a file writes, so it is claimed
    /// to write on that branch alone, and an open of a store that has one advances nothing.
    /// The write claim joins the open's own, so nothing outside this process can write the
    /// file between the look and the write, and the write looks for it again regardless.
    ///
    /// # Errors
    ///
    /// [`LocalImmutableStoreError`] if the file cannot be read or written, or the store
    /// cannot be claimed to write it.
    async fn open_info(
        path: &Path,
        lock: &Arc<StoreLock>,
        survey: &GroupSurvey,
    ) -> Result<ImmutableStoreInfo, LocalImmutableStoreError> {
        if let Some(info) = Self::read_info(path).await? {
            return Ok(info);
        }
        let _writing = lock
            .acquire(Intent::Write)
            .await
            .internal("Failed to claim the store to write its info file")?;
        get_or_init_disk_info(path, Self::oodle_migration_start(survey)).await
    }

    /// Opens the store under `path`, or an in-memory one for `None`, releasing the claim it was
    /// read under: a caller that claims the store next opens it with [`Self::new_claimed`].
    pub fn new(
        path: Option<PathBuf>,
        settings: ImmutableStoreSettings,
    ) -> impl Future<Output = Result<Arc<Self>, LocalImmutableStoreError>> {
        ReleasingOpeningClaim(Self::new_claimed(path, settings))
    }

    /// Opens the store as [`Self::new`] does, and returns with it the claim it was read
    /// under, `None` for an in-memory store.
    ///
    /// A caller that claims the store next keeps this one until its own claim has joined it.
    /// The flock is then held throughout, so that claim finds the store current, where one
    /// taken after this was dropped would find the flock released and the store reloads.
    ///
    /// # Errors
    ///
    /// [`LocalImmutableStoreError`] if the store cannot be claimed or read.
    pub async fn new_claimed(
        path: Option<PathBuf>,
        settings: ImmutableStoreSettings,
    ) -> Result<(Arc<Self>, Option<StoreGuard>), LocalImmutableStoreError> {
        let immutable_path = path.map(|path| {
            let mut path = path;
            path.push("immutable");
            Arc::new(path)
        });

        #[cfg(feature = "failure_generator")]
        let failure_generator = LocalImmutableStoreFailureGenerator {
            retry_rate: std::env::var("LORE_GENERATE_RETRY_RATE")
                .unwrap_or_default()
                .parse::<f32>()
                .unwrap_or_default(),
            miss_fragment_writes: LocalImmutableStoreFailureGenerator::hashes_from_env(
                "LORE_MISS_FRAGMENT_WRITES",
            ),
            refuse_fragment_writes: parking_lot::Mutex::new(
                LocalImmutableStoreFailureGenerator::hashes_from_env("LORE_REFUSE_FRAGMENT_WRITES"),
            ),
        };

        // Target 70% percentage of max size by default if the given setting is invalid
        let mut settings = settings;
        if settings.target_size_percentage == 0 || settings.target_size_percentage >= 100 {
            settings.target_size_percentage = 70;
        }

        // Target 70% percentage of max capacity by default if the given setting is invalid
        if settings.target_capacity_percentage == 0 || settings.target_capacity_percentage >= 100 {
            settings.target_capacity_percentage = 70;
        }

        let settings = if !settings.verify_write
            && let Ok(var) = std::env::var("LORE_IMMUTABLE_STORE_VERIFY_WRITE")
            && (var == "1" || var.to_lowercase() == "true")
        {
            let mut settings = settings;
            settings.verify_write = true;
            settings
        } else {
            settings
        };

        let lock = if let Some(path) = immutable_path.as_deref() {
            if !path.exists() {
                let _ = lore_io::IoDriver::global().create_dir_all(path).await;
            }
            Some(
                StoreLock::new(path.as_path(), settings.reuse_state_across_holds)
                    .internal("Failed to open the store's lock directory")?,
            )
        } else {
            None
        };
        // Held across the survey and the info read below, which read from disk and so
        // under the flock, and returned to the caller.
        let mut opening = match lock.as_ref() {
            Some(lock) => Some(
                lock.acquire(Intent::Read)
                    .await
                    .internal("Failed to acquire store lock")?,
            ),
            None => None,
        };

        let gc_counters = Arc::new(crate::maintenance::GcCounters::new());
        let survey = Self::survey_groups(immutable_path.as_deref(), &settings).await?;
        // The groups were just read under this hold, so the store is current as of
        // the epoch the acquisition established. Saying so here is what keeps the
        // first real operation from reporting stale and reloading what was just read.
        if let Some(opening) = opening.as_mut() {
            opening.note_refreshed();
        }
        let info = match (immutable_path.as_deref(), lock.as_ref()) {
            (Some(path), Some(lock)) => Self::open_info(path, lock, &survey).await?,
            _ => ImmutableStoreInfo::default(),
        };
        let group = Self::build_groups(immutable_path.as_deref(), &settings, &gc_counters, survey);
        let store = LocalImmutableStore {
            info: RwLock::new(info),
            path: immutable_path.clone(),
            lock,
            group,
            settings,
            eviction: Semaphore::new(1),
            compaction: Semaphore::new(1),
            stop_requests: AtomicUsize::new(0),
            compaction_reclaimed: AtomicU64::new(0),
            deserialize_all: Semaphore::new(1),
            deserialized_all: AtomicBool::new(false),
            instruments: StoreInstruments::default(),
            gc_counters,
            #[cfg(feature = "failure_generator")]
            failure_generator,
        };

        let store = Arc::new(store);
        if let Some(lock) = store.lock.as_ref() {
            lock.watch(Arc::<Self>::downgrade(&store));
            for group in &store.group {
                group.packstore.anchor(lock).await;
                crate::local::fan_out::anchor_writes(&group.writes, lock);
            }
        }

        // The `Arc` only exists now (not when the groups were built above), so back-fill
        // the weak self-ref the load hooks need to fire a pass.
        let dyn_store: Arc<dyn crate::immutable_store::ImmutableStore> = store.clone();
        store.gc_counters.set_store(&dyn_store);
        if let Some(path) = immutable_path.as_deref() {
            let mut old_packpath = path.clone();
            old_packpath.push("pack");
            if let Ok(metadata) = lore_io::IoDriver::global()
                .metadata(old_packpath.clone())
                .await
                && metadata.is_dir()
            {
                // Claimed for the upgrade, which rewrites every entry's packfile
                // reference, writes new per-group packfiles, and unlinks the old pack
                // directory. Unclaimed, two processes opening the same legacy store
                // would both run it, and a third reading those packfiles would see
                // them deleted underneath it and never be told the store had changed.
                //
                // Inside this branch rather than around the whole constructor: a
                // top-level `pack` directory only exists on a legacy store — a current
                // one keeps its packfiles per group — so an ordinary open never reaches
                // here and pays nothing.
                let _claim = store.hold(Intent::Write).await.map_err(|err| {
                    LocalImmutableStoreError::internal_with_context(
                        err,
                        "Failed to claim the store for a packfile upgrade",
                    )
                })?;
                // Looked for again under the claim: another process opening the same
                // legacy store may have upgraded it, and removed the directory, while this
                // one waited for the claim.
                if lore_io::IoDriver::global()
                    .metadata(old_packpath.clone())
                    .await
                    .is_ok_and(|metadata| metadata.is_dir())
                {
                    store
                        .clone()
                        .upgrade_global_packfiles(path, old_packpath.as_path())
                        .await?;
                }
            }
        }

        Ok((store, opening.map(StoreGuard::into_span)))
    }

    pub fn packstore(&self, group_index: usize) -> &crate::PackStore {
        &self.group[group_index].packstore
    }

    pub async fn run_migrations(self: &Arc<Self>) -> Result<(), LocalImmutableStoreError> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        self.run_migrations_held(path).await
    }

    /// The body of [`Self::run_migrations`], holding the claims it takes until it returns.
    ///
    /// The info file is read under a read claim, which reloads a store another process
    /// changed, so a migration that process finished is not run again here. A migration
    /// rewrites buckets, packfiles and the info file, so it runs under a write claim,
    /// taken only once there is one to run.
    async fn run_migrations_held(
        self: &Arc<Self>,
        _path: &Arc<PathBuf>,
    ) -> Result<(), LocalImmutableStoreError> {
        let _reading = self.hold(Intent::Read).await.map_err(|err| {
            LocalImmutableStoreError::internal_with_context(
                err,
                "Failed to claim the store to read its info file",
            )
        })?;
        let oodle_group_to_migrate = self.info.read().await.next_group_index_to_migrate_oodle;
        if oodle_group_to_migrate != -1 {
            #[cfg(feature = "oodle")]
            {
                let opted_out = std::env::var("LORE_IMMUTABLE_NO_OODLE_MIGRATION")
                    .is_ok_and(|var| var == "1" || var.to_lowercase() == "true");
                if !opted_out {
                    let _writing = self.hold(Intent::Write).await.map_err(|err| {
                        LocalImmutableStoreError::internal_with_context(
                            err,
                            "Failed to claim the store for a migration",
                        )
                    })?;
                    // Read again under the write claim, which reloads a store changed since
                    // the read above.
                    let oodle_group_to_migrate =
                        self.info.read().await.next_group_index_to_migrate_oodle;
                    if oodle_group_to_migrate != -1 {
                        oodle_migration::migrate_groups(
                            self.clone(),
                            _path,
                            oodle_group_to_migrate,
                        )
                        .await?;
                    }
                }
            }
            #[cfg(not(feature = "oodle"))]
            {
                lore_base::lore_warn!(
                    "Local Store Oodle migration needs to run, but this binary does not support Oodle. Eventually, Oodle support will be dropped and your local Oodle data will be irretrievable"
                );
            }
        }

        Ok(())
    }

    /// Changes the store's info with `accessor` and writes it to the info file.
    ///
    /// Claimed to write, since the info file is the store's on-disk state like any other:
    /// the claim advances the epoch before the file is written, and reloads a store
    /// another process changed before `accessor` sees the info. Joins the claim of a
    /// migration it is part of.
    ///
    /// # Errors
    ///
    /// [`LocalImmutableStoreError`] if the store cannot be claimed or the file cannot
    /// be written.
    pub async fn update_store_info<F>(&self, accessor: F) -> Result<(), LocalImmutableStoreError>
    where
        F: FnOnce(&mut ImmutableStoreInfo),
    {
        let _claim = self.hold(Intent::Write).await.map_err(|err| {
            LocalImmutableStoreError::internal_with_context(
                err,
                "Failed to claim the store to write its info file",
            )
        })?;
        self.write_store_info(accessor).await
    }

    /// The body of [`Self::update_store_info`], run under the claim it holds.
    async fn write_store_info<F>(&self, accessor: F) -> Result<(), LocalImmutableStoreError>
    where
        F: FnOnce(&mut ImmutableStoreInfo),
    {
        let mut store = self.info.write().await;
        accessor(&mut store);

        if let Some(path) = self.path.as_deref() {
            write_info_file(&store, &info_path_for_store_root(path))
                .await
                .map_err(|err| {
                    LocalImmutableStoreError::internal_with_context(
                        err,
                        "Failed to store updated info file",
                    )
                })?;
        }

        Ok(())
    }

    async fn upgrade_global_packfiles(
        self: Arc<Self>,
        path: &Path,
        old_packpath: &Path,
    ) -> Result<(), LocalImmutableStoreError> {
        self.deserialize_all_buckets().await?;
        let start = std::time::Instant::now();
        // Don't care about minimum number of packfiles, just pass 1
        let old_packstore = Arc::new(crate::PackStore::new(Some(path.to_path_buf()), 1, None));
        if old_packstore.total_size().await.unwrap_or_default() == 0 {
            drop(old_packstore);
            lore_base::lore_debug!("Ignore upgrade of packstore, old packstore is empty");
            let _ = fs_util::unlink_recursive(old_packpath).await;
            return Ok(());
        }

        lore_base::lore_warn!("Upgrading old global packfiles to new group packfiles");
        let path = Arc::new(path.to_path_buf());
        for group_index in 0..self.group.len() {
            let mut tasks = JoinSet::new();
            let active_buckets = self.group[group_index]
                .bucket_count
                .load(atomic::Ordering::Relaxed);
            for bucket_index in 0..active_buckets {
                let old_packstore = old_packstore.clone();
                let path = path.clone();
                let store = self.clone();
                lore_base::lore_spawn!(tasks, async move {
                    let group = &store.group[group_index];
                    let packstore = &group.packstore;
                    let bucket_ref = group.bucket(bucket_index).clone();
                    let mut bucket = bucket_ref.write().await;
                    let mut last_hash = Hash::default();
                    let mut last_data = ImmutableData::default();

                    if packstore.total_size().await.unwrap_or_default() > 0
                        && !bucket.upgrade_packfile
                    {
                        lore_base::lore_debug!(
                            "Ignore upgrade of packstore for bucket {group_index} {bucket_index}, already upgraded"
                        );
                        return;
                    }

                    let sorted_index = bucket.sorted_index.to_vec();
                    for sorted_index in sorted_index {
                        let entry = &mut bucket.entry[sorted_index as usize];
                        if entry.data.pack_file == 0 {
                            continue;
                        }

                        if entry.address.hash == last_hash {
                            entry.data.assign_deduplicated_payload(last_data);
                            continue;
                        }

                        last_hash = Hash::default();
                        last_data = ImmutableData::default();

                        match Self::load(&old_packstore, entry.data).await {
                            Ok(payload) => match packstore.store(payload).await {
                                Ok(packref) => {
                                    lore_base::lore_trace!(
                                        "Wrote payload to group {group_index} packfile {} offset {}",
                                        packref.id,
                                        packref.offset
                                    );
                                    entry.data.pack_file = packref.id;
                                    entry.data.pack_offset = packref.offset;

                                    last_hash = entry.address.hash;
                                    last_data = entry.data;
                                }
                                Err(err) => {
                                    lore_base::lore_warn!(
                                        "Failed to write payload to group packstore in upgrade: {err}"
                                    );
                                    entry.data.pack_file = 0;
                                    entry.data.pack_offset = 0;
                                }
                            },
                            Err(err) => {
                                lore_base::lore_warn!(
                                    "Failed to read payload from old packstore in upgrade: {err}"
                                );
                                entry.data.pack_file = 0;
                                entry.data.pack_offset = 0;
                            }
                        }
                    }
                    group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
                    drop(bucket);

                    let _flush_guard =
                        crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
                    let bucket = bucket_ref.read_owned().await;
                    let _ = ImmutableStoreBucket::serialize(
                        bucket,
                        group.clone(),
                        path.as_ref(),
                        group_index,
                        bucket_index,
                        true, /* Wait and sync all data to storage media */
                    )
                    .await;
                });
            }

            while tasks.join_next().await.is_some() {}
        }
        drop(old_packstore);

        fs_util::unlink_recursive(old_packpath)
            .await
            .internal("Failed to remove old packstore directory")?;

        let elapsed = start.elapsed().as_secs_f64();
        lore_base::lore_warn!("Packstore upgraded in {elapsed:.2}s");

        Ok(())
    }

    pub async fn deserialize_all_buckets(&self) -> Result<(), LocalImmutableStoreError> {
        let _permit = self.deserialize_all.acquire().await;
        if self.deserialized_all.load(atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let mut final_result = Ok(());
        let mut tasks = JoinSet::new();
        if let Some(path) = self.path.as_ref() {
            for (group_index, group) in self.group.iter().enumerate() {
                let resumed = group
                    .packstore
                    .resume()
                    .await
                    .forward("Failed to deserialize storage bucket");
                let resumed_ok = resumed.is_ok();
                final_result = final_result.and(resumed);
                if !resumed_ok {
                    continue;
                }
                while let Some(result) = tasks.try_join_next() {
                    final_result = final_result.and(
                        result
                            .internal("Task failed")
                            .map_err(LocalImmutableStoreError::from)
                            .flatten(),
                    );
                }
                let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
                for bucket_index in 0..active_buckets {
                    let bucket = group.bucket(bucket_index).clone();
                    if !bucket.read().await.deserialized {
                        let path = path.clone();
                        let group = group.clone();
                        let gc_counters = self.gc_counters.clone();
                        lore_base::lore_spawn!(tasks, async move {
                            let mut bucket = bucket.write().await;
                            bucket
                                .deserialize(
                                    &group,
                                    path.as_path(),
                                    group_index,
                                    bucket_index,
                                    Some(&gc_counters),
                                )
                                .await
                        });
                    }
                }
            }
        }

        while let Some(result) = tasks.join_next().await {
            final_result = final_result.and(
                result
                    .internal("Task failed")
                    .map_err(LocalImmutableStoreError::from)
                    .flatten(),
            );
        }

        if final_result.is_ok() {
            self.deserialized_all.store(true, atomic::Ordering::Relaxed);
        }

        if VALIDATE_COMPACTION && final_result.is_ok() {
            // Verify the store integrity
            for (group_index, _group) in self.group.iter().enumerate() {
                let _ = self.group_verify_store(group_index, None).await;
            }
        }

        final_result
    }

    fn lookup(
        bucket: &ImmutableStoreBucket,
        partition: Partition,
        address: Address,
        match_request: StoreMatch,
    ) -> (usize, usize, StoreMatch) {
        let count = bucket.entry.len();
        let mut start = 0;
        let mut end = count;
        let mut match_made = StoreMatch::MatchNone;
        let mut match_slot = 0;

        // Binary search the bucket
        while start < end {
            let slot = (start + end) / 2;
            let entry_index = bucket.sorted_index[slot] as usize;
            let entry = &bucket.entry[entry_index];
            let mut order = address.hash.cmp(&entry.address.hash);
            if order == Ordering::Equal {
                if match_made == StoreMatch::MatchNone {
                    match_made = StoreMatch::MatchHash;
                    match_slot = slot;

                    if match_request == StoreMatch::MatchHash {
                        break;
                    }
                }

                order = partition.cmp(&entry.partition);
                if order == Ordering::Equal {
                    if match_made == StoreMatch::MatchHash {
                        match_made = StoreMatch::MatchPartition;
                        match_slot = slot;

                        if match_request == StoreMatch::MatchPartition {
                            break;
                        }
                    }

                    order = address.context.cmp(&entry.address.context);
                    if order == Ordering::Equal {
                        match_made = StoreMatch::MatchFull;
                        match_slot = slot;
                        break;
                    }
                }
            }
            if order == Ordering::Less {
                end = slot;
            } else {
                start = slot + 1;
            }
        }

        (match_slot, start, match_made)
    }

    /// Whether the entry at `slot` is one of the associations `partition` holds for `hash`. Entries
    /// sort by hash, then partition, then context, so those occupy one contiguous run this bounds.
    fn in_partition_run(
        bucket: &ImmutableStoreBucket,
        slot: usize,
        partition: Partition,
        hash: Hash,
    ) -> bool {
        let entry = &bucket.entry[bucket.sorted_index[slot] as usize];
        entry.address.hash == hash && entry.partition == partition
    }

    /// Whether the entry at `slot` is a tombstone, which is a representation no copy adopts.
    fn is_obliterated(bucket: &ImmutableStoreBucket, slot: usize) -> bool {
        bucket.entry[bucket.sorted_index[slot] as usize].data.flags
            & FragmentFlags::PayloadObliterated.bits()
            != 0
    }

    /// Whether `slot` is the source to read from, remembering it as the fallback where it holds the
    /// representation without the payload.
    fn is_copy_source(
        bucket: &ImmutableStoreBucket,
        slot: usize,
        representation_only: &mut Option<usize>,
    ) -> bool {
        if Self::is_obliterated(bucket, slot) {
            return false;
        }
        if bucket.entry[bucket.sorted_index[slot] as usize]
            .data
            .pack_file
            != 0
        {
            return true;
        }
        representation_only.get_or_insert(slot);
        false
    }

    /// The slot a copy reads its source from, or `None` where the partition holds no live
    /// association for the hash.
    ///
    /// A context resolves to that one association; a zero context to any of them, which is all a
    /// caller acting on a partition match has. Every association in the run points at the same
    /// payload, so which one answers changes only which representation the destination adopts — and
    /// a tombstone's is not one to adopt, so obliterated entries are skipped and one holding the
    /// payload is preferred over one holding the representation alone.
    ///
    /// `lookup` lands anywhere inside the run, so both directions are walked outwards from there a
    /// step at a time and every entry is judged as it is passed. Neither has to reach an end and
    /// neither is exhausted before the other: the first association holding the payload answers, so
    /// the walk stops at whichever side it is nearest on.
    fn copy_source_slot(
        bucket: &ImmutableStoreBucket,
        partition: Partition,
        address: Address,
    ) -> Option<usize> {
        if !address.context.is_zero() {
            let (slot, _, matching) =
                Self::lookup(bucket, partition, address, StoreMatch::MatchFull);
            return (matching == StoreMatch::MatchFull && !Self::is_obliterated(bucket, slot))
                .then_some(slot);
        }

        let (anchor, _, matching) =
            Self::lookup(bucket, partition, address, StoreMatch::MatchPartition);
        if matching < StoreMatch::MatchPartition {
            return None;
        }

        let in_run = |slot: usize| {
            slot < bucket.sorted_index.len()
                && Self::in_partition_run(bucket, slot, partition, address.hash)
        };

        let mut representation_only = None;
        let mut back = Some(anchor);
        let mut forward = in_run(anchor + 1).then_some(anchor + 1);

        while back.is_some() || forward.is_some() {
            if let Some(slot) = back {
                if Self::is_copy_source(bucket, slot, &mut representation_only) {
                    return Some(slot);
                }
                back = (slot > 0 && in_run(slot - 1)).then(|| slot - 1);
            }
            if let Some(slot) = forward {
                if Self::is_copy_source(bucket, slot, &mut representation_only) {
                    return Some(slot);
                }
                forward = in_run(slot + 1).then_some(slot + 1);
            }
        }

        representation_only
    }

    /// Insert the entry for `address`, see `store_entry`, writing its payload first, outside the
    /// bucket's lock, when the entry needs one written, see `store_payload`. The payload is taken
    /// to match the address's hash, which is not checked. Takes no claim of its own: the caller
    /// holds one, as `put` does.
    #[lore_macro::test_pub]
    pub(crate) async fn store(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        let group_index = address.hash.data()[0] as usize;

        if fragment.size_payload == 0 {
            return Err(LocalImmutableStoreError::internal("Invalid payload"));
        }

        if (fragment.size_payload as usize) > crate::FRAGMENT_SIZE_THRESHOLD {
            return Err(LocalImmutableStoreError::internal(format!(
                "fragment size_payload {} exceeds FRAGMENT_SIZE_THRESHOLD {}",
                fragment.size_payload,
                crate::FRAGMENT_SIZE_THRESHOLD
            )));
        }
        if let Some(payload) = payload.as_ref()
            && payload.len() != fragment.size_payload as usize
        {
            return Err(LocalImmutableStoreError::internal(format!(
                "fragment payload length mismatch on store: buffer {} vs size_payload {}",
                payload.len(),
                fragment.size_payload
            )));
        }

        let carried = if payload.is_some() {
            StorePayload::Unwritten
        } else {
            StorePayload::Absent
        };
        let mut outcome = self
            .store_entry(&partition, &address, &fragment, carried, force)
            .await?;
        if let (StoreOutcome::NeedsPayload, Some(payload)) = (outcome, payload) {
            outcome = self
                .store_payload(&partition, &address, &fragment, payload, force)
                .await?;
        }
        if let Some(flags) = outcome.flush_flags() {
            self.flush_later(group_index, flags).await;
        }

        let StoreOutcome::Stored {
            flags: fragment_flags,
            pack_file,
        } = outcome
        else {
            return Ok(());
        };
        if self.settings.verify_write
            && pack_file != 0
            && fragment_flags & FragmentFlags::PayloadObliterated == 0
        {
            // Boxed: verifying writes is rare, and carried inline its state would be part of
            // every store's future.
            Box::pin(self.verify_written(
                partition,
                address,
                fragment,
                fragment_flags,
                group_index,
            ))
            .await?;
        }

        Ok(())
    }

    /// Read back the payload a put of `address` stored with `fragment_flags` and hold it to the
    /// address's hash, as a store that verifies its writes does.
    ///
    /// An eviction or an obliterate may take the payload, or the whole entry, between the put
    /// and the read, as it may take any payload it ranks: the put stored what it was given, and
    /// nothing of it is left to verify. Only a payload still held is read back.
    async fn verify_written(
        &self,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        fragment_flags: u32,
        group_index: usize,
    ) -> Result<(), LocalImmutableStoreError> {
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Verifying).await;
        let gate = self.group[group_index].payload_gate.hold();
        let find = self
            .find(partition, address, Lookup::Write)
            .await
            .inspect_err(|err| {
                lore_base::lore_warn!(
                    "Store write verify failed: {err}\n{}",
                    Backtrace::force_capture()
                );
            })
            .forward::<LocalImmutableStoreError>("Failed to verify written data")?;
        if find.data.pack_file == 0 {
            return Ok(());
        }

        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Located).await;
        let data = Self::load(&self.group[find.group].packstore, find.data)
            .await
            .inspect_err(|err| {
                lore_base::lore_warn!(
                    "Store write verify failed: {err}\n{}",
                    Backtrace::force_capture()
                );
            })
            .forward::<LocalImmutableStoreError>("Failed to verify written data")?;
        drop(gate);

        let hash = if fragment_flags & FragmentFlags::PayloadCompressed != 0 {
            let (_fragment, data) = compress::decompress(fragment, &data)
                .inspect_err(|err| {
                    lore_base::lore_warn!(
                        "Store write verify failed: {err}\n{}",
                        Backtrace::force_capture()
                    );
                })
                .forward::<LocalImmutableStoreError>("Failed to verify written data")?;
            hash::hash_slice(&data)
        } else {
            hash::hash_slice(&data)
        };
        if hash != address.hash {
            lore_base::lore_warn!(
                "Store write verify failed: Hash verification failed {} != {}\n{}",
                hash,
                address.hash,
                Backtrace::force_capture()
            );
            return Err(LocalImmutableStoreError::internal(
                "Failed to verify written data",
            ));
        }
        Ok(())
    }

    /// Write `payload` to the group's packstore outside any bucket lock, then insert the entry for
    /// `address` pointing at it, see [`Self::store_entry`]. The group's payload gate is held from
    /// before the write until the entry is in the bucket, see
    /// [`ImmutableStoreGroup::payload_gate`].
    ///
    /// Stores of a hash not yet stored each write the payload, since none holds the bucket during
    /// its write. Unless forced, the entry takes the payload of the first to insert, and the bytes
    /// the others wrote stay unreferenced until compaction reclaims them, as do those of a store
    /// dropped once its write has landed. One dropped during the write leaves the write to land,
    /// see [`crate::PackStore::store`].
    async fn store_payload(
        &self,
        partition: &Partition,
        address: &Address,
        fragment: &Fragment,
        payload: Bytes,
        force: bool,
    ) -> Result<StoreOutcome, LocalImmutableStoreError> {
        let group = &self.group[address.hash.data()[0] as usize];
        let _gate = group.payload_gate.hold();
        let written = group
            .packstore
            .store(payload)
            .await
            .forward::<LocalImmutableStoreError>(
                "Failed storing immutable data, packstore write failed",
            )?;
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Written).await;
        self.store_entry(
            partition,
            address,
            fragment,
            StorePayload::Written(written),
            force,
        )
        .await
    }

    /// Insert or update the entry for `address` under its bucket's write lock. An entry needing a
    /// payload, which the best match for its hash does not hold or `force` replaces, takes the one
    /// `payload` was written to, and is reported as [`StoreOutcome::NeedsPayload`] while it is
    /// unwritten. A written payload the entry does not take stays unreferenced. A store without a
    /// payload over the tombstone of its association is refused with `PayloadNotFound`.
    async fn store_entry(
        &self,
        partition: &Partition,
        address: &Address,
        fragment: &Fragment,
        payload: StorePayload,
        force: bool,
    ) -> Result<StoreOutcome, LocalImmutableStoreError> {
        let has_payload = !matches!(payload, StorePayload::Absent);
        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        let (bucket_index, mut bucket) = group.bucket_for_write(&address.hash).await;

        if !bucket.deserialized && self.path.is_some() {
            Box::pin(bucket.deserialize(
                group,
                self.path.clone().unwrap().as_ref(),
                group_index,
                bucket_index,
                Some(&self.gc_counters),
            ))
            .await?;
        }

        let (match_slot, insert_slot, match_made) =
            Self::lookup(&bucket, *partition, *address, StoreMatch::MatchFull);

        let matched_hash = match_made != StoreMatch::MatchNone;
        let matched_partition =
            (match_made == StoreMatch::MatchPartition) || (match_made == StoreMatch::MatchFull);

        let mut size_payload = fragment.size_payload;
        let mut fragment_flags = fragment.flags;
        let mut pack_file = 0;
        let mut pack_offset = 0;

        if matched_hash {
            let entry_index = bucket.sorted_index[match_slot] as usize;
            let data = bucket.entry[entry_index].data;
            if match_made == StoreMatch::MatchFull
                && !has_payload
                && data.flags & FragmentFlags::PayloadObliterated != 0
            {
                return Err(LocalImmutableStoreError::from(PayloadNotFound::from(
                    address.hash,
                )));
            }
            if data.size_content != fragment.size_content {
                if (data.flags & FragmentFlags::PayloadObliterated) == 0 {
                    // Same hash, different content - we have a collision
                    return Err(LocalImmutableStoreError::internal(format!(
                        "Hash collision in immutable store for {} size {}, previous entry has size {}",
                        address.hash, fragment.size_content as usize, data.size_content as usize,
                    )));
                } else {
                    lore_base::lore_warn!(
                        "Overwriting obliterated fragment for address: {address}"
                    );
                }
            }

            let mut current_flags = data.flags;
            if match_made == StoreMatch::MatchFull {
                lore_base::lore_trace!(
                    "Immutable store full deduplication for {} size {}:{} matching size {}:{}",
                    address,
                    fragment.size_payload,
                    fragment.size_content,
                    data.size_payload,
                    data.size_content
                );
                let keeps_payload = (!force && data.pack_file != 0) || !has_payload;
                if keeps_payload {
                    let stored = ImmutableData::stored_flags(fragment_flags, data.pack_file);
                    if stored != (current_flags & FragmentFlags::PayloadStored) {
                        let previous_flags = current_flags;
                        {
                            let entry = &mut bucket.entry[entry_index];
                            entry.data.flags &= !FragmentFlags::PayloadStored;
                            entry.data.flags |= stored;
                            current_flags = entry.data.flags;
                        }

                        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

                        lore_base::lore_trace!(
                            "Immutable store updated flags for {} from {} to {}",
                            address,
                            previous_flags,
                            current_flags
                        );
                    }
                    return Ok(StoreOutcome::Kept {
                        changed_flags: (current_flags != data.flags).then_some(current_flags),
                    });
                }
            }

            // Deduplicate payload if it already exist and not forced overwrite
            if !force || !has_payload {
                pack_file = data.pack_file;
                pack_offset = data.pack_offset;

                if pack_file != 0 {
                    // If we are deduplicating to existing data we need to keep the existing data fragment flags
                    fragment_flags = (current_flags & !FragmentFlags::PayloadStored)
                        | (fragment_flags & FragmentFlags::PayloadStored);
                    size_payload = data.size_payload;
                }
            }
        }

        // If there was no matching entry in the same partition, we must have payload data
        if !matched_partition || (pack_file == 0 && has_payload) {
            if has_payload {
                if pack_file == 0 {
                    let StorePayload::Written(written) = payload else {
                        return Ok(StoreOutcome::NeedsPayload);
                    };
                    pack_file = written.id;
                    pack_offset = written.offset;
                }
            } else {
                lore_base::lore_trace!("Storing partial fragment {address}");
            }
        }

        let last_access = Self::last_access();

        let data = ImmutableData {
            flags: (fragment_flags & !FragmentFlags::PayloadStored)
                | ImmutableData::stored_flags(fragment_flags, pack_file),
            size_payload,
            size_content: fragment.size_content,
            pack_file,
            pack_offset,
            last_access,
        };

        if match_made == StoreMatch::MatchFull {
            let entry_index = bucket.sorted_index[match_slot] as usize;
            bucket.entry[entry_index].data = data;
        } else {
            // inject new entry
            let count = bucket.entry.len();
            bucket.sorted_index.insert(insert_slot, count as u32);

            lore_base::lore_trace!(
                "Inject new immutable store entry for {address} (last access {last_access}"
            );
            bucket.entry.push(ImmutableStoreEntry {
                address: *address,
                partition: *partition,
                data,
            });
        }

        // Ensure all other instances of this hash has the same payload associated - if storing
        // a payload for an already existing hash that had no payload we should upgrade those.
        // A tombstone is left as it is, so that storing the content under another association
        // does not bring the obliterated one back.
        if data.pack_file != 0 {
            let last_slot = bucket.sorted_index.len() - 1;

            let mut update_slot = |slot| {
                let entry_index = bucket.sorted_index[slot] as usize;
                if bucket.entry[entry_index].address.hash != address.hash {
                    return false;
                }
                let entry = &mut bucket.entry[entry_index];
                if entry.data.flags & FragmentFlags::PayloadObliterated == 0
                    && (entry.data.pack_file != data.pack_file
                        || entry.data.pack_offset != data.pack_offset)
                {
                    entry.data.assign_deduplicated_payload(data);
                }
                true
            };

            let mut loop_slot = insert_slot;
            while loop_slot > 0 {
                loop_slot -= 1;
                if !update_slot(loop_slot) {
                    break;
                }
            }

            loop_slot = insert_slot;
            while loop_slot < last_slot {
                loop_slot += 1;
                if !update_slot(loop_slot) {
                    break;
                }
            }
        }

        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        Ok(StoreOutcome::Stored {
            flags: fragment_flags,
            pack_file,
        })
    }

    /// Resolve an address to the best match the bucket holds, at full strength. Callers gate the
    /// answer afterwards against the scope they serve or report at; searching at a scope instead
    /// would cap the level and lose the distinction the caller is asking for.
    pub async fn find(
        &self,
        partition: Partition,
        address: Address,
        lookup: Lookup,
    ) -> Result<ImmutableStoreFindResult, LocalImmutableStoreError> {
        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];

        // CAS-retry: re-read bucket_count after the lock acquire to detect any fan-out that landed between the index computation and the lock; on the deserialize-and-upgrade path, re-check after each lock transition because a fan-out can fire while we hold no bucket lock.
        let (bucket_index, bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&address.hash, n);
            let bucket_ref = group.bucket(idx).clone();
            let bucket = bucket_ref.clone().read_owned().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) != n {
                drop(bucket);
                continue;
            }

            if !bucket.deserialized && self.path.is_some() {
                drop(bucket);
                let path = self.path.clone().unwrap();
                let bucket_ref_for_write = bucket_ref.clone();
                let group_for_check = group;
                let gc_counters = self.gc_counters.clone();
                Box::pin(async move {
                    let mut bucket = bucket_ref_for_write.write_owned().await;
                    if group_for_check.bucket_count.load(atomic::Ordering::Relaxed) != n
                        || bucket.deserialized
                    {
                        return Ok::<_, LocalImmutableStoreError>(());
                    }
                    bucket
                        .deserialize(
                            group_for_check,
                            path.as_ref(),
                            group_index,
                            idx,
                            Some(&gc_counters),
                        )
                        .await?;
                    Ok(())
                })
                .await?;
                let bucket = bucket_ref.read_owned().await;
                if group.bucket_count.load(atomic::Ordering::Relaxed) != n {
                    drop(bucket);
                    continue;
                }
                break (idx, bucket);
            }

            break (idx, bucket);
        };

        // Binary search the bucket
        let (match_slot, _, match_made) =
            Self::lookup(&bucket, partition, address, StoreMatch::MatchFull);

        if match_made == StoreMatch::MatchNone {
            Ok(ImmutableStoreFindResult {
                group: group_index,
                ..Default::default()
            })
        } else {
            let index = bucket.sorted_index[match_slot] as usize;
            let matched_partition = bucket.entry[index].partition;
            let matched_context = bucket.entry[index].address.context;
            let data = &bucket.entry[index].data;

            let data = if data.flags & FragmentFlags::PayloadObliterated
                == FragmentFlags::PayloadObliterated
            {
                ImmutableData {
                    flags: FragmentFlags::PayloadObliterated.bits(),
                    ..Default::default()
                }
            } else {
                if self.settings.atime {
                    Self::stamp_last_access(
                        data,
                        match lookup {
                            Lookup::Write => &group.dirty[bucket_index],
                            Lookup::Read => &group.soft_dirty[bucket_index],
                        },
                    );
                }

                *data
            };

            Ok(ImmutableStoreFindResult {
                group: group_index,
                data,
                matching: match_made,
                partition: matched_partition,
                context: matched_context,
            })
        }
    }

    /// Tombstone one address and release the payload it holds, if nothing else
    /// refers to that payload.
    ///
    /// A payload is zeroed only once nothing can read it through the index. Its tombstone is
    /// written to the bucket file durably first, so that a process killed meanwhile does not
    /// reopen with the live entry pointing at zeros, and the group's payload gate is drained, so
    /// that a read that located the payload before the tombstone has loaded it whole. No
    /// compaction runs from the lookup to the zeroing: compaction moves payloads and truncates
    /// packfiles, and the range zeroed must still hold this payload rather than one written
    /// since. A tombstone that cannot be written is undone and the obliterate fails, so that a
    /// retry zeroes the payload.
    ///
    /// Whatever the address itself references is the caller's to have dealt with
    /// first; see [`ImmutableStore::obliterate`].
    async fn obliterate_one(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        let mut compaction_stopped = None;
        let original = loop {
            match self
                .tombstone(partition, address, compaction_stopped.is_some())
                .await?
            {
                Tombstone::Already => return Ok(()),
                Tombstone::Kept => {
                    stats.num_fragments.fetch_add(1, atomic::Ordering::Relaxed);
                    self.flush_later(group_index, FragmentFlags::PayloadObliterated.bits())
                        .await;
                    return Ok(());
                }
                Tombstone::Zero(original) => break original,
                Tombstone::ZeroOnceCompactionStops => {
                    compaction_stopped = Some(
                        self.compaction
                            .acquire()
                            .await
                            .internal("holding compaction off")?,
                    );
                }
            }
        };
        lore_base::lore_debug!(
            "Fragment payload has no other references, obliterating from packstore"
        );
        if let Some(path) = self.path.clone()
            && let Err(err) =
                Box::pin(self.write_bucket_of(group_index, &address.hash, &path)).await
        {
            Box::pin(self.untombstone(partition, address, original)).await;
            return Err(err);
        }
        group.payload_gate.drain().await;
        group
            .packstore
            .obliterate(
                original.pack_file,
                original.pack_offset,
                original.size_payload,
            )
            .await
            .forward::<StoreError>("Failed to obliterate payload from pack store.")?;
        drop(compaction_stopped);
        stats.num_payloads.fetch_add(1, atomic::Ordering::Relaxed);
        stats.num_fragments.fetch_add(1, atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Tombstone the entry of `address`, unless it is one already, see [`Tombstone`]. An entry
    /// whose payload is to be zeroed is tombstoned only with `compaction_stopped`.
    async fn tombstone(
        &self,
        partition: Partition,
        address: Address,
        compaction_stopped: bool,
    ) -> Result<Tombstone, StoreError> {
        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        let (bucket_index, mut bucket) = group.bucket_for_write(&address.hash).await;

        if !bucket.deserialized && self.path.is_some() {
            Box::pin(bucket.deserialize(
                group,
                self.path.clone().unwrap().as_ref(),
                group_index,
                bucket_index,
                Some(&self.gc_counters),
            ))
            .await
            .forward::<StoreError>("Failed to deserialize store data.")?;
        }

        let (match_slot, _, match_made) =
            Self::lookup(&bucket, partition, address, StoreMatch::MatchFull);

        if match_made != StoreMatch::MatchFull {
            return Err(StoreError::from(AddressNotFound::from(address)));
        }

        let index = bucket.sorted_index[match_slot] as usize;
        let entry = &bucket.entry[index];

        let is_last_fragment = {
            let previous_match = (0..match_slot)
                .rev()
                .map(|idx| bucket.sorted_index[idx] as usize)
                .map(|idx| &bucket.entry[idx])
                .take_while(|entry| entry.address.hash == address.hash)
                .any(|entry| entry.data.flags != FragmentFlags::PayloadObliterated.bits());

            let next_match = ((match_slot + 1)..bucket.sorted_index.len())
                .map(|idx| bucket.sorted_index[idx] as usize)
                .map(|idx| &bucket.entry[idx])
                .take_while(|entry| entry.address.hash == address.hash)
                .any(|entry| entry.data.flags != FragmentFlags::PayloadObliterated.bits());

            !previous_match && !next_match
        };

        if entry.data.flags & FragmentFlags::PayloadObliterated.bits()
            == FragmentFlags::PayloadObliterated
        {
            lore_base::lore_warn!("Address {address} already obliterated");
            return Ok(Tombstone::Already);
        }

        let original = entry.data;
        let zero = is_last_fragment && original.pack_file != 0;
        if zero && !compaction_stopped {
            return Ok(Tombstone::ZeroOnceCompactionStops);
        }

        bucket.entry[index].data = ImmutableData {
            flags: FragmentFlags::PayloadObliterated.bits(),
            size_payload: 0,
            size_content: 0,
            pack_file: 0,
            pack_offset: 0,
            last_access: 0,
        };
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

        Ok(if zero {
            Tombstone::Zero(original)
        } else {
            Tombstone::Kept
        })
    }

    /// Write the bucket holding `hash` in group `group_index` durably, whatever its mark says, as
    /// the tombstone in it has to be on disk before the payload it held is zeroed. In a group whose
    /// fan-out is not committed, a bucket is only written with the commit, so the group is flushed.
    /// A group below [`crate::local::fan_out::FAN_OUT_LEVEL_MAX`] with no level marker yet is given
    /// one, and the write fails if it cannot be, see
    /// [`crate::local::fan_out::commit_if_initial_level`]. The write runs in a task of its own, so
    /// that one whose caller is dropped still runs to its end, holding the group's flush lock, so
    /// that none of it lands after the lock is released, and a share of the group's writes, so that
    /// the store stays claimed while it runs: the caller's claim goes with the caller. Once it ends,
    /// it releases the flock if nothing keeps it, see [`crate::local::fan_out::releasing_once_done`].
    async fn write_bucket_of(
        &self,
        group_index: usize,
        hash: &Hash,
        path: &Arc<PathBuf>,
    ) -> Result<(), StoreError> {
        let group = self.group[group_index].clone();
        let hash = *hash;
        let path = path.clone();
        let work = async move {
            let flush_guard =
                crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
            let _in_flight = crate::local::fan_out::write_share(&group.writes);
            let bucket_index = crate::local::fan_out::bucket_index_for(
                &hash,
                group.bucket_count.load(atomic::Ordering::Relaxed),
            );
            group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
            if group.fan_out_uncommitted() {
                return flush_locked_group(group, group_index, path, true)
                    .await
                    .forward::<StoreError>("committing the group of an obliterated entry");
            }
            crate::local::fan_out::commit_if_initial_level(
                &flush_guard,
                &group.writes,
                &group.committed_level,
                &group.pending_level,
                &group.bucket_count,
                path.as_ref(),
                group_index,
                true,
            )
            .await
            .internal("committing the level of the group of an obliterated entry")?;
            let bucket = group.bucket(bucket_index).clone().read_owned().await;
            ImmutableStoreBucket::serialize(
                bucket,
                group.clone(),
                path.as_ref(),
                group_index,
                bucket_index,
                true,
            )
            .await
            .forward::<StoreError>("writing the bucket of an obliterated entry")
        };
        lore_base::lore_spawn!(crate::local::fan_out::releasing_once_done(
            self.lock.clone(),
            work
        ))
        .await
        .internal("writing the bucket of an obliterated entry")?
    }

    /// Give the tombstone of `address` back the entry `original` it replaced, as an obliterate
    /// that could not write the tombstone leaves the address as it found it.
    async fn untombstone(&self, partition: Partition, address: Address, original: ImmutableData) {
        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        let (bucket_index, mut bucket) = group.bucket_for_write(&address.hash).await;
        let (slot, _, matched) = Self::lookup(&bucket, partition, address, StoreMatch::MatchFull);
        if matched != StoreMatch::MatchFull {
            return;
        }
        let index = bucket.sorted_index[slot] as usize;
        let data = &mut bucket.entry[index].data;
        if data.flags == FragmentFlags::PayloadObliterated.bits() && data.pack_file == 0 {
            *data = original;
            group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        }
    }

    /// Starts the group's delayed flush unless one is due, after an index change that leaves an
    /// entry with `flags`. A change to a durable entry waits for an explicit flush unless
    /// [`ImmutableStoreSettings::flush_background`] is set, since it is stored upstream as well.
    /// Called after the bucket's flag is set, so that a sweep that has already cleared the group's
    /// flag sees the bucket, see [`ImmutableStoreGroup::scheduled`].
    async fn flush_later(self: &Arc<Self>, group_index: usize, flags: u32) {
        if flags & FragmentFlags::PayloadStoredDurable == FragmentFlags::PayloadStoredDurable
            && !self.settings.flush_background
        {
            return;
        }
        let group = &self.group[group_index];
        if group.scheduled.swap(true, atomic::Ordering::Relaxed) {
            return;
        }
        let mut flush = group.flush.lock().await;
        let _ = flush.try_join_next();
        lore_base::lore_spawn!(
            flush,
            ImmutableStoreGroup::flush_delayed(
                Arc::downgrade(self),
                group_index,
                self.settings.flush_delay_seconds,
            )
        );
    }

    pub async fn load(
        packstore: &crate::PackStore,
        data: ImmutableData,
    ) -> Result<Bytes, LocalImmutableStoreError> {
        if data.pack_file == 0 {
            // No emit since this is a benign error (data not stored locally)
            return Err(LocalImmutableStoreError::internal(
                "Load failed, no data stored locally",
            ));
        }

        let data = packstore
            .load(data.pack_file, data.pack_offset, data.size_payload)
            .await
            .forward::<LocalImmutableStoreError>("Read from packstore failed")?;
        Ok(data)
    }

    /// Whether garbage collection has been asked to stop. Eviction and compaction check
    /// this inside their group work, so a stop lands within one packfile sweep.
    #[lore_macro::test_pub]
    #[inline]
    fn gc_stop_requested(&self) -> bool {
        self.stop_requests.load(atomic::Ordering::Relaxed) > 0
    }

    /// Report the end of a compaction pass and what it reclaimed. Called from every exit
    /// past the `compaction_begin`, including the stopped and failed ones, so a sink that
    /// saw a pass start always sees it finish.
    fn report_compaction_end(&self, sink: Option<&crate::gc_event::GcEventSinkRef>) {
        if let Some(sink) = sink {
            sink.compaction_end(self.compaction_reclaimed.load(atomic::Ordering::Relaxed));
        }
    }

    /// Evict the least recently accessed payloads of each bucket over its share of `target_size`,
    /// an equal part per bucket, until it is below its share or has nothing left to evict. Runs
    /// only when the group stores at least `target_size` bytes and holds a payload it may evict.
    /// A protected local payload counts toward the size but is never evicted. Each bucket that
    /// evicts is written when the store has a `path`. A stop request, or a fan-out of the group,
    /// ends the pass. Returns how many payloads were evicted and their size in bytes.
    #[lore_macro::test_pub]
    async fn evict_group_sized(
        self: Arc<Self>,
        group_index: usize,
        target_size: usize,
        path: Option<Arc<PathBuf>>,
        protect_local_fragment: bool,
        sync_data: bool,
    ) -> (usize, usize) {
        let group = &self.group[group_index];
        let bucket_count = group.bucket_count.load(atomic::Ordering::Relaxed);
        let (mut run, mut candidates, mut evicted) = (vec![], vec![], vec![]);
        let Some(bucket_sizes) = self
            .bucket_payload_sizes(group, bucket_count, protect_local_fragment, &mut run)
            .await
        else {
            return (0, 0);
        };
        let total_stored_size: usize = bucket_sizes.iter().map(|sizes| sizes.stored).sum();
        let total_evictable_size: usize = bucket_sizes.iter().map(|sizes| sizes.evictable).sum();
        if total_stored_size < target_size || total_evictable_size == 0 {
            lore_base::lore_debug!(
                "Size eviction for group {group_index} skipped, currently {total_stored_size} bytes, {total_evictable_size} evictable, target {target_size}"
            );
            return (0, 0);
        }

        let mut total_evicted_count = 0usize;
        let mut total_evicted_size = 0usize;

        let bucket_target_size = target_size / std::cmp::max(1, bucket_count);

        lore_base::lore_debug!(
            "Size eviction for group {group_index} targeting {target_size} bytes, currently {total_stored_size} bytes, target {bucket_target_size} per bucket"
        );

        let mut serialize_tasks = JoinSet::new();
        for (bucket_index, sizes) in bucket_sizes.iter().enumerate() {
            while serialize_tasks.try_join_next().is_some() {}

            #[cfg(feature = "test-util")]
            crate::test_util::pause_point(crate::test_util::PausePoint::Evicting).await;
            if self.gc_stop_requested() {
                lore_base::lore_debug!(
                    "Size eviction for group {group_index} stopping at bucket {bucket_index}"
                );
                break;
            }
            if group.bucket_count.load(atomic::Ordering::Relaxed) != bucket_count {
                lore_base::lore_debug!(
                    "Size eviction for group {group_index} stopping at bucket {bucket_index}, the group fanned out"
                );
                break;
            }

            if sizes.stored <= bucket_target_size || sizes.evictable == 0 {
                continue;
            }
            let Some(bucket) = group.try_bucket(bucket_index).cloned() else {
                continue;
            };

            let stored_size = Self::eviction_candidates(
                &*bucket.read().await,
                protect_local_fragment,
                &mut run,
                &mut candidates,
            );
            let cutoff = Self::eviction_cutoff(&mut candidates, stored_size, bucket_target_size);

            let bucket_eviction = Self::evict_accessed_by(
                &mut *bucket.write().await,
                cutoff,
                protect_local_fragment,
                &mut run,
                &mut evicted,
            );

            lore_base::lore_trace!(
                "Size eviction for group {group_index} bucket {bucket_index} evicted {} payloads from {} entries",
                bucket_eviction.payloads,
                bucket_eviction.entries
            );
            if bucket_eviction.entries == 0 {
                continue;
            }
            group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

            total_evicted_size += bucket_eviction.bytes;
            total_evicted_count += bucket_eviction.payloads;

            if let Some(path) = path.as_ref() {
                let path = path.clone();
                let store = self.clone();
                lore_base::lore_spawn!(serialize_tasks, async move {
                    let group = store.group[group_index].clone();
                    let bucket = group.bucket(bucket_index).clone();
                    let flush_guard =
                        crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
                    if let Err(err) = crate::local::fan_out::commit_if_initial_level(
                        &flush_guard,
                        &group.writes,
                        &group.committed_level,
                        &group.pending_level,
                        &group.bucket_count,
                        path.as_ref(),
                        group_index,
                        sync_data,
                    )
                    .await
                    {
                        lore_base::lore_warn!(
                            "Failed to write level marker for group {group_index}: {err}"
                        );
                        return;
                    }
                    let bucket = bucket.read_owned().await;
                    let _ = ImmutableStoreBucket::serialize(
                        bucket,
                        group,
                        path.as_ref(),
                        group_index,
                        bucket_index,
                        sync_data,
                    )
                    .await;
                });
            }
        }

        // Await all serialization
        while serialize_tasks.join_next().await.is_some() {}

        if total_evicted_count > 0 {
            lore_base::lore_debug!(
                "Size evicted for group {group_index} with {total_evicted_count} payloads, {total_evicted_size} of {total_stored_size} bytes orphaned in pack store"
            );
        } else {
            lore_base::lore_debug!("Size eviction for group {group_index} did not evict anything");
        }

        (total_evicted_count, total_evicted_size)
    }

    /// Stored and evictable payload bytes of each of the group's first `bucket_count` buckets,
    /// or `None` once garbage collection is asked to stop. `run` is as for
    /// [`Self::visit_payloads`].
    async fn bucket_payload_sizes(
        &self,
        group: &ImmutableStoreGroup,
        bucket_count: usize,
        protect_local_fragment: bool,
        run: &mut Vec<BucketPayload>,
    ) -> Option<Vec<BucketSizes>> {
        let mut sizes = Vec::with_capacity(bucket_count);
        for bucket_index in 0..bucket_count {
            if self.gc_stop_requested() {
                return None;
            }
            let mut bucket_sizes = BucketSizes::default();
            if let Some(bucket) = group.try_bucket(bucket_index) {
                Self::visit_payloads(
                    &*bucket.read().await,
                    protect_local_fragment,
                    run,
                    |payload| {
                        bucket_sizes.stored += payload.size as usize;
                        if payload.evictable {
                            bucket_sizes.evictable += payload.size as usize;
                        }
                    },
                );
            }
            sizes.push(bucket_sizes);
        }
        Some(sizes)
    }

    /// Call `visit` once for each payload of the bucket. Entries sort by hash, and the entries
    /// holding one payload share its hash, so `run` gathers the payloads of one hash at a time.
    fn visit_payloads(
        bucket: &ImmutableStoreBucket,
        protect_local_fragment: bool,
        run: &mut Vec<BucketPayload>,
        mut visit: impl FnMut(&BucketPayload),
    ) {
        run.clear();
        let mut hash = None;
        for index in bucket.sorted_index.iter() {
            let entry = &bucket.entry[*index as usize];
            if entry.data.pack_file == 0 {
                continue;
            }
            if hash != Some(entry.address.hash) {
                run.iter().for_each(&mut visit);
                run.clear();
                hash = Some(entry.address.hash);
            }
            let location = PayloadLocation::of(&entry.data);
            let evictable = Self::evictable(&entry.data, protect_local_fragment);
            let last_access = Self::load_last_access(&entry.data);
            match run.iter_mut().find(|payload| payload.location == location) {
                Some(payload) => {
                    payload.evictable &= evictable;
                    payload.last_access = payload.last_access.max(last_access);
                }
                None => run.push(BucketPayload {
                    hash: entry.address.hash,
                    location,
                    size: entry.data.size_payload,
                    evictable,
                    last_access,
                }),
            }
        }
        run.iter().for_each(&mut visit);
    }

    /// Fill `candidates` with the size and newest last access of each payload of the bucket that
    /// eviction may take, and return the bytes of every payload the bucket stores.
    fn eviction_candidates(
        bucket: &ImmutableStoreBucket,
        protect_local_fragment: bool,
        run: &mut Vec<BucketPayload>,
        candidates: &mut Vec<(u32, u64)>,
    ) -> usize {
        candidates.clear();
        let mut stored_size = 0;
        Self::visit_payloads(bucket, protect_local_fragment, run, |payload| {
            stored_size += payload.size as usize;
            if payload.evictable {
                candidates.push((payload.size, payload.last_access));
            }
        });
        stored_size
    }

    /// Sort `candidates`, each a payload's size and newest last access, oldest first, and return
    /// the last access up to which evicting them brings a bucket of `stored_size` bytes below
    /// `share`, or `u64::MAX` when evicting all of them does not.
    fn eviction_cutoff(candidates: &mut [(u32, u64)], stored_size: usize, share: usize) -> u64 {
        candidates.sort_unstable_by_key(|candidate| candidate.1);
        let mut evicted_size = 0;
        for (size, last_access) in candidates.iter() {
            evicted_size += *size as usize;
            if stored_size.saturating_sub(evicted_size) < share {
                return *last_access;
            }
        }
        u64::MAX
    }

    /// Evict each payload of the bucket that eviction may remove and that was last accessed at or
    /// before `cutoff`, dropping it from every entry holding it, which then no longer claims a
    /// local payload. Only an entry eviction may take loses it, so a protected entry keeps its
    /// payload even where the index does not keep the entries of a hash together. `evicted` is a
    /// buffer for the hash and location of each payload evicted.
    fn evict_accessed_by(
        bucket: &mut ImmutableStoreBucket,
        cutoff: u64,
        protect_local_fragment: bool,
        run: &mut Vec<BucketPayload>,
        evicted: &mut Vec<(Hash, PayloadLocation)>,
    ) -> BucketEviction {
        evicted.clear();
        let mut eviction = BucketEviction::default();
        Self::visit_payloads(bucket, protect_local_fragment, run, |payload| {
            if payload.evictable && payload.last_access <= cutoff {
                evicted.push((payload.hash, payload.location));
                eviction.payloads += 1;
                eviction.bytes += payload.size as usize;
            }
        });
        if evicted.is_empty() {
            return eviction;
        }
        evicted.sort_unstable();

        for entry in bucket.entry.iter_mut() {
            if entry.data.pack_file != 0
                && Self::evictable(&entry.data, protect_local_fragment)
                && evicted
                    .binary_search(&(entry.address.hash, PayloadLocation::of(&entry.data)))
                    .is_ok()
            {
                entry.data.pack_file = 0;
                entry.data.pack_offset = 0;
                entry.data.flags &= !FragmentFlags::PayloadStoredLocal.bits();
                eviction.entries += 1;
            }
        }
        eviction
    }

    /// Evicts the least recently accessed entries of every bucket over its share of
    /// `max_capacity`, and returns how many, with the write claim the pass took at its first
    /// eviction, if it took one.
    ///
    /// The claim is handed back rather than dropped, for the caller to hold until it has
    /// written what the pass evicted. Without it, that flush runs on whatever span another use
    /// of the store holds at the time, and that span can end, and its flock go, while the
    /// flush still runs.
    pub async fn evict_oldest(
        &self,
        max_capacity: usize,
        sink: Option<&crate::gc_event::GcEventSinkRef>,
    ) -> (usize, Option<StoreGuard>) {
        let mut evict_count = 0;
        let mut began = false;
        // Taken at the first eviction and handed back with the count, so the epoch advances
        // once rather than per bucket, and not at all when there is nothing to evict.
        let mut writing = None;

        if self.gc_stop_requested() {
            return (0, None);
        }
        let Ok(_permit) = self.eviction.acquire().await else {
            lore_base::lore_warn!("Evict oldest failed to get permit");
            return (0, None);
        };
        if self.gc_stop_requested() {
            return (0, None);
        }

        let target_percentage = if self.settings.target_capacity_percentage > 0
            && self.settings.target_capacity_percentage < 100
        {
            self.settings.target_capacity_percentage
        } else {
            80
        };
        let protect_local_fragment = self.settings.protect_local_fragment;
        let mut buckets = Vec::with_capacity(BUCKET_COUNT);
        let mut total_count = 0;
        let mut group_count = 0;
        let mut bucket_count = 0;
        for group in self.group.iter() {
            if self.gc_stop_requested() {
                break;
            }

            buckets.clear();
            let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
            // Per-group target divides by this group's bucket_count, not the constant 256, so groups at level 1 still get a meaningful target rather than max_capacity / 65536.
            let target_capacity =
                (max_capacity * target_percentage) / (100 * GROUP_COUNT * active_buckets);
            {
                for bucket_index in 0..active_buckets {
                    let Some(bucket_ref) = group.try_bucket(bucket_index) else {
                        continue;
                    };
                    let bucket = bucket_ref.read().await;
                    let entry_count = bucket.entry.len();
                    total_count += entry_count;
                    if entry_count > target_capacity
                        && bucket
                            .entry
                            .iter()
                            .any(|entry| Self::evictable(&entry.data, protect_local_fragment))
                    {
                        buckets.push(bucket_index);
                    }
                }
            }
            if buckets.is_empty() {
                continue;
            }

            if !began {
                // The first group with work is where this pass stops being a read.
                // Taken once — later groups join it — and never at all by a pass that
                // finds nothing, which is what keeps an idle timer from announcing a
                // change to every other process on the store.
                match self.try_hold(Intent::Write).await {
                    Ok(BackgroundClaim::Held(claim)) => writing = Some(claim),
                    Ok(BackgroundClaim::Unlocked) => {}
                    // A timer pass has no reason to queue behind a store in use, holding
                    // the eviction permit `stop_gc` waits for; the next pass comes anyway.
                    Ok(BackgroundClaim::Busy) => {
                        lore_base::lore_debug!("Eviction deferred: the store is in use");
                        return (evict_count, None);
                    }
                    Err(err) => {
                        lore_base::lore_warn!("Eviction could not claim the store: {err}");
                        return (evict_count, None);
                    }
                }
                if let Some(sink) = sink {
                    sink.eviction_begin(max_capacity as u64);
                }
                began = true;
            }

            group_count += 1;
            bucket_count += buckets.len();

            for bucket_index in &buckets {
                let bucket = group.bucket(*bucket_index).clone();
                let bucket_evicted = Self::evict_oldest_bucket(
                    bucket,
                    &group.dirty[*bucket_index],
                    target_capacity,
                    protect_local_fragment,
                )
                .await;
                evict_count += bucket_evicted;
                if bucket_evicted > 0
                    && let Some(sink) = sink
                {
                    sink.eviction_progress(bucket_evicted as u64);
                }
            }
        }

        if began && let Some(sink) = sink {
            sink.eviction_end(evict_count as u64);
        }

        if evict_count > 0 {
            lore_base::lore_debug!(
                "Evicted {evict_count} of {total_count} fragments from {bucket_count} buckets across {group_count} groups"
            );
        } else {
            lore_base::lore_trace!(
                "No fragments evicted for max capacity {max_capacity}, total count {total_count}"
            );
        }

        (evict_count, writing)
    }

    /// Whether eviction may take an entry holding `data`, or its payload: any entry, or with
    /// `protect_local_fragment` only one whose payload is stored durably.
    fn evictable(data: &ImmutableData, protect_local_fragment: bool) -> bool {
        !protect_local_fragment || data.flags & FragmentFlags::PayloadStoredDurable != 0
    }

    /// Remove the bucket's least recently accessed entries until it holds no more than
    /// `target_capacity`, or no entry that can be evicted is left. Returns how many it removed.
    /// A protected entry counts toward the capacity but is never removed.
    pub async fn evict_oldest_bucket(
        bucket: Arc<RwLock<ImmutableStoreBucket>>,
        dirty: &AtomicBool,
        target_capacity: usize,
        protect_local_fragment: bool,
    ) -> usize {
        let cutoff_point = {
            let bucket = bucket.read().await;

            let to_evict = bucket.entry.len().saturating_sub(target_capacity);
            if to_evict == 0 {
                return 0;
            }

            let mut heap: BinaryHeap<u64> = BinaryHeap::with_capacity(to_evict);
            for entry in bucket.entry.iter() {
                if !Self::evictable(&entry.data, protect_local_fragment) {
                    continue;
                }
                let key = Self::load_last_access(&entry.data);
                if heap.len() < to_evict {
                    heap.push(key);
                } else if key < *heap.peek().unwrap() {
                    heap.pop();
                    heap.push(key);
                }
            }

            let Some(cutoff_point) = heap.peek() else {
                return 0;
            };
            *cutoff_point
        };

        // Build new arrays with the remaining items
        let mut sorted_index = GrowVec::new();
        let mut entry = GrowVec::new();

        // Accessing in sorted order means we don't have to resort
        let mut bucket = bucket.write().await;
        for index in bucket.sorted_index.iter() {
            let index = *index as usize;

            let data = &bucket.entry[index].data;
            if Self::evictable(data, protect_local_fragment) && data.last_access <= cutoff_point {
                continue;
            }

            let new_index = entry.len() as u32;
            sorted_index.push(new_index);
            entry.push(bucket.entry[index]);
        }

        let evict_count = bucket.entry.len() - entry.len();
        if evict_count == 0 {
            return 0;
        }

        bucket.sorted_index = sorted_index;
        bucket.entry = entry;

        dirty.store(true, atomic::Ordering::Relaxed);

        evict_count
    }

    /// Run a compaction step over the next `compaction_parallel_groups` groups from `at`, or
    /// start a pass when `at` is `None` and the store holds at least `max_size` bytes. Returns
    /// the group the next step starts at, or `None` when there is no step left to take. A step
    /// in which a group stopped with work left returns the group it started at, so that a later
    /// step repeats it. Every bucket is loaded first, and [`Self::compact_group_packfiles`]
    /// leaves out a group with a bucket that failed to load.
    #[lore_macro::test_pub]
    async fn compact_packfiles(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        let target_percentage = self.settings.target_size_percentage;
        let target_size = (max_size * target_percentage) / 100;

        self.instruments
            .compaction
            .target_size
            .record(target_size as u64, &[]);

        let mut group_index = at.unwrap_or(GROUP_COUNT);
        let starting_pass = group_index >= GROUP_COUNT;

        // Measuring and loading read the store's files, so the pass is claimed first; to
        // read, because it may still find the store under its cap, stop, or lose the
        // permit below without changing anything. A claim that reloaded the store has
        // the size measured from what is on disk now.
        let _reading = match self.try_hold(Intent::Read).await? {
            BackgroundClaim::Held(claim) => Some(claim),
            BackgroundClaim::Unlocked => None,
            // Left for another pass rather than queued behind a store in use. `None` has
            // the compactor wait out its interval, and a resumed pass reads its resume point
            // back from disk.
            BackgroundClaim::Busy => return Ok(None),
        };
        if starting_pass {
            let total_size = self.measure_packstores().await;
            self.instruments
                .compaction
                .total_size
                .record(total_size as u64, &[]);
            if total_size < max_size {
                lore_base::lore_debug!(
                    "Packstore compactor skipping, current size {total_size} is below threshold {max_size}"
                );
                return Ok(None);
            }
            lore_base::lore_debug!(
                "Packstore compactor running, current size {total_size} is above threshold {max_size} - targeting {target_size} bytes ({target_percentage}% of max size)"
            );
        }
        let _ = self.deserialize_all_buckets().await;

        if self.gc_stop_requested() {
            return Ok(None);
        }
        let Ok(_permit) = self.compaction.acquire().await else {
            lore_base::lore_warn!("Compact packfiles failed to get permit");
            return Ok(None);
        };

        if self.gc_stop_requested() {
            return Ok(None);
        }

        // Past the size check, the permit and the stop checks, this pass is going to
        // rewrite packfiles, so it claims to write here rather than at entry: a pass that
        // found the store under its cap, or was stopped, returns without announcing
        // anything. The claim is the pass's own, as its read claim is, and never a span's
        // cover: the pass runs on its own schedule, and the span of whatever use of the store
        // is under way can end while it runs.
        //
        // **A write claim that reloaded the store ends the pass.** The buckets loaded above
        // are gone with the reload, and rewriting packfiles against buckets that no longer
        // describe them would truncate payloads still referenced on disk. The resume point
        // stays where it was, so the next pass starts from state loaded under its own claim.
        let writing = self.claim(Intent::Write).await?;
        if writing.as_ref().is_some_and(StoreGuard::is_stale) {
            return Ok(at);
        }
        let _writing = writing;

        if starting_pass {
            lore_base::lore_debug!("Packstore compactor starting fresh");

            group_index = 0;
        } else {
            lore_base::lore_debug!(
                "Packstore compactor continuing, current in progress is group {group_index}"
            );
        }

        // Committed to a step from here, so the begin is owed exactly one end on every exit
        // below, carrying what this step reclaimed.
        self.compaction_reclaimed
            .store(0, atomic::Ordering::Relaxed);
        if let Some(sink) = &sink {
            sink.compaction_begin(max_size as u64);
        }

        let target_size = target_size / GROUP_COUNT;
        lore_base::lore_debug!("Packstore compactor targeting {target_size} bytes per group");
        self.instruments
            .compaction
            .group_target_size
            .record(target_size as u64, &[]);

        let mut tasks = JoinSet::new();
        let parallel_group_count = std::cmp::max(1, self.settings.compaction_parallel_groups);
        for parallel in 0..parallel_group_count {
            let group_index = group_index + parallel;
            if group_index >= GROUP_COUNT {
                continue;
            }

            let store = self.clone();
            let path = self.path.clone();
            let protect_local_fragment = self.settings.protect_local_fragment;
            let group_sink = sink.clone();
            lore_base::lore_spawn!(
                tasks,
                store.compact_group_packfiles(
                    group_index,
                    path,
                    target_size,
                    protect_local_fragment,
                    sync_data,
                    self.instruments.compaction.clone(),
                    group_sink,
                )
            );
        }

        let mut final_result = Ok(());
        let mut completed = true;
        while let Some(result) = tasks.join_next().await {
            let group_result = result
                .map_err(|_err| Internal::msg("Task failure"))
                .map_err(StoreError::from)
                .flatten();
            if let Ok(group_completed) = &group_result {
                completed &= *group_completed;
            }
            final_result = final_result.and(group_result.map(|_| ()));
        }

        if !completed {
            lore_base::lore_debug!(
                "Packstore compactor stopped during group {group_index}, leaving resume point"
            );
            self.report_compaction_end(sink.as_ref());
            final_result?;
            return Ok(Some(group_index));
        }

        group_index += parallel_group_count;

        if let Some(path) = self.path.as_ref() {
            let path = path.join(DOT_COMPACT);
            if group_index < GROUP_COUNT {
                let _ = lore_io::IoDriver::global()
                    .write_file_bytes(
                        path,
                        Bytes::copy_from_slice(&group_index.to_ne_bytes()),
                        false,
                    )
                    .await;
            } else {
                let _ = lore_io::IoDriver::global().remove_file(path).await;
            }
        }

        // Error out if any operation failed
        final_result.inspect_err(|_| self.report_compaction_end(sink.as_ref()))?;

        self.report_compaction_end(sink.as_ref());

        if group_index < GROUP_COUNT {
            let total_size = self.measure_packstores().await;
            lore_base::lore_debug!("Packstore compaction complete, new total size {total_size}");
            self.instruments
                .compaction
                .final_total_size
                .record(total_size as u64, &[]);
            Ok(Some(group_index))
        } else {
            Ok(None)
        }
    }

    /// Evict the group toward `target_size`, then rewrite the packfiles
    /// [`Self::reclaimable_packfiles`] names, one at a time, with [`Self::rewrite_packfile`].
    /// They are closed to writes before any is rewritten, so each referenced payload is copied
    /// once, into a packfile the pass does not rewrite, and the payload gate is drained after, so
    /// that every entry written into one of them is indexed when the rewrite reads the buckets. A
    /// stop request ends the pass only between packfiles, so that a packfile's sweep, once
    /// started, reaches its truncate rather than starting over on the next pass. A packfile kept
    /// for an entry still pointing into it lets the pass go on to the next; a rewrite that fails
    /// or that a fan-out interrupts ends the pass, as the next would fail alike or sweep a stale
    /// layout. Those the pass ends before rewriting, and those it keeps, take writes again. An
    /// in-memory store is evicted but not rewritten.
    ///
    /// Returns whether the group was left complete, or the error that ended a rewrite. `false`
    /// means a stop request or a fan-out ended the pass with work left, eviction included, and
    /// the caller must hold the compaction resume point so a later pass repeats this group. An
    /// error leaves the group to the next full pass, so that a group that keeps failing does not
    /// hold back the groups after it. A group with a bucket that failed to
    /// load is left alone and reported complete, so that the pass moves on: that bucket holds
    /// none of its entries, so the payloads only it references would look unreferenced. So is a
    /// group whose buckets are laid out for a fan-out not yet committed on disk, see
    /// [`ImmutableStoreGroup::fan_out_uncommitted`].
    #[lore_macro::test_pub]
    #[allow(clippy::too_many_arguments)]
    async fn compact_group_packfiles(
        self: Arc<Self>,
        group_index: usize,
        path: Option<Arc<PathBuf>>,
        target_size: usize,
        protect_local_fragment: bool,
        sync_data: bool,
        instruments: CompactionInstruments,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<bool, StoreError> {
        if path.is_some() && !Self::group_loaded(&self.group[group_index]).await {
            lore_base::lore_warn!(
                "Packstore compactor skipping group {group_index}, a bucket of it failed to load"
            );
            return Ok(true);
        }
        if path.is_some() && self.group[group_index].fan_out_uncommitted() {
            lore_base::lore_warn!(
                "Packstore compactor skipping group {group_index}, its fan-out is not committed"
            );
            return Ok(true);
        }

        let (evicted_count, evicted_size) = self
            .clone()
            .evict_group_sized(
                group_index,
                target_size,
                path.clone(),
                protect_local_fragment,
                sync_data,
            )
            .await;
        lore_base::lore_debug!(
            "Packstore compactor evicted {evicted_count} fragments, {evicted_size} bytes for group {group_index}"
        );

        if VALIDATE_COMPACTION && let Err(err) = self.group_verify_store(group_index, None).await {
            lore_base::lore_warn!(
                "Packstore compactor failed verification before compacting group packfiles: {err}"
            );
        }

        let labels = [KeyValue::new("group", group_index.to_string())];
        instruments
            .group_evicted_count
            .add(evicted_count as u64, &labels);
        instruments
            .group_evicted_size
            .record(evicted_size as u64, &labels);

        if self.gc_stop_requested() {
            return Ok(false);
        }
        let Some(path) = path.as_ref() else {
            return Ok(true);
        };
        let group = &self.group[group_index];
        let Some(mut reclaimable) =
            Self::reclaimable_packfiles(group, group_index, target_size).await
        else {
            return Ok(true);
        };
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Selected).await;
        let original_sizes = match group.packstore.stop_writes(&reclaimable).await {
            Ok(sizes) => sizes,
            Err(err) => {
                lore_base::lore_warn!(
                    "Packstore compactor skipping group {group_index}, its packfiles could not be closed: {err}"
                );
                return Ok(true);
            }
        };
        group.payload_gate.drain().await;
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Stopped).await;

        let mut group_reclaimed: u64 = 0;
        let mut completed = true;
        let mut failure = None;
        let mut unprocessed = reclaimable.len();
        let mut kept = vec![];
        for (position, (&packfile, &original_size)) in
            reclaimable.iter().zip(&original_sizes).enumerate()
        {
            if self.gc_stop_requested() {
                lore_base::lore_debug!(
                    "Packstore compactor stopping group {group_index} before packfile {packfile}"
                );
                completed = false;
                unprocessed = position;
                break;
            }

            if let Ok(current_size) = group.packstore.total_size().await
                && current_size < target_size
            {
                lore_base::lore_debug!(
                    "Packstore compactor complete group {group_index}, current size {current_size} below target size {target_size}"
                );
                unprocessed = position;
                break;
            }

            lore_base::lore_debug!(
                "Packstore compactor running on group {group_index} packfile {packfile} size {original_size}"
            );
            match self.rewrite_packfile(group_index, path, packfile).await {
                Ok(Rewrite::Truncated(compacted_size)) => {
                    instruments
                        .group_final_total_size
                        .record(compacted_size as u64, &labels);
                    group_reclaimed += (original_size as u64).saturating_sub(compacted_size as u64);
                }
                Ok(Rewrite::Referenced) => kept.push(packfile),
                Ok(Rewrite::Interrupted) => {
                    lore_base::lore_debug!(
                        "Packstore compactor stopping group {group_index} at packfile {packfile}, the group fanned out"
                    );
                    kept.push(packfile);
                    unprocessed = position + 1;
                    completed = false;
                    break;
                }
                Err(err) => {
                    kept.push(packfile);
                    unprocessed = position + 1;
                    failure = Some(err);
                    break;
                }
            }
        }
        let mut reopened = reclaimable.split_off(unprocessed);
        reopened.extend(kept);
        group.packstore.restart_writes(&reopened).await;

        if group_reclaimed > 0 {
            if let Some(sink) = &sink {
                sink.compaction_progress(group_reclaimed);
            }
            self.compaction_reclaimed
                .fetch_add(group_reclaimed, atomic::Ordering::Relaxed);
        }

        match failure {
            Some(err) => Err(err),
            None => Ok(completed),
        }
    }

    pub async fn group_verify_store(
        &self,
        group_index: usize,
        bucket_index: Option<usize>,
    ) -> Result<(), StoreError> {
        let mut loaded_bytes = 0;
        let mut hashed_bytes = 0;
        let group = &self.group[group_index];
        let buckets_start = bucket_index.unwrap_or(0);
        let buckets_end = bucket_index.unwrap_or(group.bucket.len() - 1) + 1;
        for bucket_index in buckets_start..buckets_end {
            let (entry, sorted_index) = {
                let lock = group.bucket(bucket_index).read().await;
                lock.clone_for_compaction()
            };

            let mut last_hash = Hash::default();
            let mut last_pack = 0;
            let mut last_offset = 0;

            for index in sorted_index.iter() {
                let entry = entry[*index as usize];

                if entry.address.hash == last_hash && entry.data.pack_file == last_pack {
                    if entry.data.pack_offset != last_offset {
                        lore_base::lore_warn!(
                            "Group {group_index} bucket {bucket_index} entry {entry:?} does not match last hash {last_hash} packfile {last_pack} offset {last_offset}"
                        );
                    } else {
                        continue;
                    }
                }

                if entry.data.pack_file == 0 {
                    continue;
                }

                last_hash = entry.address.hash;
                last_pack = entry.data.pack_file;
                last_offset = entry.data.pack_offset;

                match group
                    .packstore
                    .load(
                        entry.data.pack_file,
                        entry.data.pack_offset,
                        entry.data.size_payload,
                    )
                    .await
                {
                    Ok(payload) => {
                        loaded_bytes += payload.len();
                        if entry.data.flags & FragmentFlags::PayloadCompressed.bits() != 0 {
                            match compress::decompress(
                                Fragment {
                                    flags: entry.data.flags,
                                    size_payload: entry.data.size_payload,
                                    size_content: entry.data.size_content,
                                },
                                &payload,
                            ) {
                                Ok((_fragment, decompressed_payload)) => {
                                    hashed_bytes += decompressed_payload.len();
                                    let verify_hash = hash::hash_slice(&decompressed_payload);
                                    if verify_hash != entry.address.hash {
                                        lore_base::lore_error!(
                                            "Group {group_index} bucket {bucket_index} entry {entry:?} failed to verify decompressed payload, got {verify_hash}"
                                        );
                                        return Err(StoreError::internal(
                                            "Integrity verification failed: decompressed payload hash mismatch",
                                        ));
                                    }
                                }
                                Err(err) => {
                                    lore_base::lore_error!(
                                        "Group {group_index} bucket {bucket_index} entry {entry:?} failed to decompress: {err}"
                                    );
                                    return Err(StoreError::internal_with_context(
                                        err,
                                        "Integrity verification failed: payload decompression error",
                                    ));
                                }
                            }
                        } else {
                            hashed_bytes += payload.len();
                            let verify_hash = hash::hash_slice(&payload);
                            if verify_hash != entry.address.hash {
                                lore_base::lore_error!(
                                    "Group {group_index} bucket {bucket_index} entry {entry:?} failed to verify uncompressed payload, got {verify_hash}"
                                );
                                return Err(StoreError::internal(
                                    "Integrity verification failed: uncompressed payload hash mismatch",
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        lore_base::lore_error!(
                            "Group {group_index} bucket {bucket_index} entry {entry:?} failed to load: {err}"
                        );
                        return Err(StoreError::internal_with_context(
                            err,
                            "Integrity verification failed: unable to load payload",
                        ));
                    }
                }
            }
        }

        if let Some(bucket) = bucket_index {
            lore_base::lore_debug!(
                "Group {group_index} bucket {bucket} immutable store integrity verified, {loaded_bytes} bytes loaded, {hashed_bytes} bytes hashed"
            );
        } else {
            lore_base::lore_debug!(
                "Group {group_index} immutable store integrity verified, {loaded_bytes} bytes loaded, {hashed_bytes} bytes hashed"
            );
        }

        Ok(())
    }

    /// Whether every bucket of the group is loaded.
    async fn group_loaded(group: &ImmutableStoreGroup) -> bool {
        for bucket_index in 0..group.bucket_count.load(atomic::Ordering::Relaxed) {
            let Some(bucket) = group.try_bucket(bucket_index) else {
                return false;
            };
            if !bucket.read().await.deserialized {
                return false;
            }
        }
        true
    }

    /// The packfiles of the group that hold unreferenced bytes, when the group stores at least
    /// `target_size` bytes and at least [`COMPACTION_MIN_UNREFERENCED_PERCENTAGE`] of them are
    /// unreferenced, and `None` otherwise, or when the sizes cannot be read. The packfile sizes
    /// are read before the referenced bytes, and the group's payload gate drained in between, as
    /// [`Self::referenced_sizes`] requires.
    async fn reclaimable_packfiles(
        group: &ImmutableStoreGroup,
        group_index: usize,
        target_size: usize,
    ) -> Option<Vec<u32>> {
        let sizes = match group.packstore.sizes().await {
            Ok(sizes) => sizes,
            Err(err) => {
                lore_base::lore_warn!(
                    "Packstore compactor skipping group {group_index}, its packfile sizes could not be read: {err}"
                );
                return None;
            }
        };
        let stored_size: usize = sizes.iter().sum();
        if stored_size < target_size {
            lore_base::lore_debug!(
                "Packstore compactor complete group {group_index}, current size {stored_size} below target size {target_size}"
            );
            return None;
        }

        group.payload_gate.drain().await;
        let referenced = Self::referenced_sizes(group, sizes.len()).await;
        let mut unreferenced_size = 0;
        let mut reclaimable = vec![];
        for (index, (size, referenced)) in sizes.iter().zip(&referenced).enumerate() {
            if referenced < size {
                unreferenced_size += size - referenced;
                reclaimable.push(index as u32 + 1);
            }
        }
        if unreferenced_size == 0
            || unreferenced_size * 100 < stored_size * COMPACTION_MIN_UNREFERENCED_PERCENTAGE
        {
            lore_base::lore_debug!(
                "Packstore compactor skipping group {group_index}, {unreferenced_size} of {stored_size} bytes in {} packfiles unreferenced",
                sizes.len()
            );
            return None;
        }
        Some(reclaimable)
    }

    /// Bytes that the group's entries reference in each of its first `packfile_count`
    /// packfiles, the packfile with id `n` at index `n - 1`, each payload counted once, as
    /// [`Self::compact_bucket_packfile`] copies it.
    ///
    /// A store holds the group's payload gate from before it writes a payload until its entry is
    /// in the bucket. With the packfile sizes read, then the gate drained, before this, a payload
    /// stored meanwhile is counted here whenever a size includes it, so a fully referenced
    /// packfile never looks reclaimable. A fan-out during the scan can leave entries uncounted,
    /// which costs a rewrite that reclaims nothing.
    async fn referenced_sizes(group: &ImmutableStoreGroup, packfile_count: usize) -> Vec<usize> {
        let mut referenced = vec![0; packfile_count];
        let mut run = vec![];
        let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
        for bucket_index in 0..active_buckets {
            let Some(bucket) = group.try_bucket(bucket_index) else {
                continue;
            };
            Self::visit_payloads(&*bucket.read().await, false, &mut run, |payload| {
                if let Some(size) = referenced.get_mut(payload.location.pack_file as usize - 1) {
                    *size += payload.size as usize;
                }
            });
        }
        referenced
    }

    /// Rewrite the payloads every bucket of the group holds in `packfile` into packfiles taking
    /// writes, see [`Self::compact_bucket_packfile`], and truncate `packfile` once nothing can
    /// point into it: every copy and bucket file write succeeded, no bucket kept an entry in it,
    /// and the bucket count did not change, which a fan-out moving entries out of the sweep
    /// would. Every bucket it moved an entry in, or holding a change not yet written, is written
    /// durably first, whatever `sync_data` the store uses, from a copy taken before the group's
    /// packfiles are synced and written after, so that no index file left on disk points into the
    /// truncated packfile or at payload bytes not yet stored, and the sync holds up no lookup or
    /// insert in the bucket. A bucket left clean is as its file has it, and holds nothing in
    /// `packfile` but what a failed copy kept, which keeps the packfile too, so its file is left
    /// alone: in a group of 256 buckets, most hold nothing in any one packfile. The payload gate is drained
    /// before the truncate, so that a read that found a location in `packfile` before its entry
    /// moved has loaded it. A fan-out, or a failed bucket write, sync or truncate, which is
    /// returned, ends the rewrite, and the buckets it moved stay dirty for the next flush.
    async fn rewrite_packfile(
        self: &Arc<Self>,
        group_index: usize,
        path: &Arc<PathBuf>,
        packfile: u32,
    ) -> Result<Rewrite, StoreError> {
        let group = &self.group[group_index];
        let bucket_count = group.bucket_count.load(atomic::Ordering::Relaxed);
        let mut compacted_size = 0;
        let mut referenced = false;
        for bucket_index in 0..bucket_count {
            let (copied_size, bucket_referenced) =
                Self::compact_bucket_packfile(group, group_index, bucket_index, packfile).await;
            compacted_size += copied_size;
            referenced |= bucket_referenced;

            let bucket = group.bucket(bucket_index).clone();
            let flush_guard =
                crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) != bucket_count {
                return Ok(Rewrite::Interrupted);
            }
            // Every bucket write holds the flush lock and marks the bucket again if it fails.
            if !group.dirty[bucket_index].load(atomic::Ordering::Relaxed) {
                continue;
            }
            crate::local::fan_out::commit_if_initial_level(
                &flush_guard,
                &group.writes,
                &group.committed_level,
                &group.pending_level,
                &group.bucket_count,
                path.as_ref(),
                group_index,
                true,
            )
            .await
            .internal_with(|| format!("writing the level marker of group {group_index}"))?;
            let bucket = bucket.read_owned().await;
            ImmutableStoreBucket::serialize(
                bucket,
                group.clone(),
                path.as_ref(),
                group_index,
                bucket_index,
                true,
            )
            .await
            .forward_with::<StoreError, _>(|| {
                format!("Failed writing group {group_index} bucket {bucket_index}")
            })?;

            if VALIDATE_COMPACTION
                && let Err(err) = self
                    .group_verify_store(group_index, Some(bucket_index))
                    .await
            {
                lore_base::lore_warn!(
                    "Packstore compactor verification failed after a bucket pass: {err}"
                );
            }
        }

        let flush_guard = crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
        let fanned_out = group.bucket_count.load(atomic::Ordering::Relaxed) != bucket_count;
        drop(flush_guard);

        if fanned_out {
            return Ok(Rewrite::Interrupted);
        }
        if referenced {
            lore_base::lore_warn!(
                "Packstore compactor keeping group {group_index} packfile {packfile}, an entry may still point into it"
            );
            return Ok(Rewrite::Referenced);
        }
        lore_base::lore_debug!(
            "Packstore compactor group {group_index} truncating packfile {packfile}"
        );
        group.payload_gate.drain().await;
        group
            .packstore
            .truncate(packfile, Arc::clone(&group.writes).read_owned().await)
            .await
            .forward_with::<StoreError, _>(|| {
                format!("Failed truncating group {group_index} packfile {packfile}")
            })?;
        Ok(Rewrite::Truncated(compacted_size))
    }

    /// Copy each payload bucket `bucket_index` holds in `packfile` once, synced, into packfiles
    /// taking writes, and point the entries at its location, with its size, to the copy,
    /// changing only their packfile and offset. The entries are re-pointed in the live bucket, so
    /// an entry added since the copies were taken that deduplicates to a copied location is
    /// re-pointed too. A payload that cannot be copied is left where it is. Returns the bytes
    /// copied, and whether an entry of the bucket still points into `packfile`, as when a copy
    /// failed.
    pub async fn compact_bucket_packfile(
        group: &ImmutableStoreGroup,
        group_index: usize,
        bucket_index: usize,
        packfile: u32,
    ) -> (usize, bool) {
        let mut locations: Vec<(u32, u32)> = group
            .bucket(bucket_index)
            .read()
            .await
            .entry
            .iter()
            .filter(|entry| entry.data.pack_file == packfile)
            .map(|entry| (entry.data.pack_offset, entry.data.size_payload))
            .collect();
        if locations.is_empty() {
            return (0, false);
        }
        locations.sort_unstable();
        locations.dedup();

        let mut copies: Vec<(u32, u32, crate::PackStoreRef)> = Vec::with_capacity(locations.len());
        let mut copied_size = 0;
        for &(offset, size) in &locations {
            match Self::copy_payload(group, packfile, offset, size).await {
                Ok(copy) => {
                    debug_assert!(
                        copy.id != packfile,
                        "Compaction wrote data to same packfile being repacked"
                    );
                    copies.push((offset, size, copy));
                    copied_size += size as usize;
                }
                Err(err) => {
                    lore_base::lore_warn!(
                        "Packstore compactor could not copy group {group_index} packfile {packfile} offset {offset}: {err}"
                    );
                }
            }
        }
        drop(locations);
        if copies.is_empty() {
            return (copied_size, true);
        }

        let mut flushed = Vec::with_capacity(1);
        for (_, _, copy) in &copies {
            if !flushed.contains(&copy.id) {
                if group.packstore.flush(copy.id, true).await.is_err() {
                    return (copied_size, true);
                }
                flushed.push(copy.id);
            }
        }

        let mut referenced = false;
        let mut bucket = group.bucket(bucket_index).write().await;
        for entry in bucket.entry.iter_mut() {
            if entry.data.pack_file != packfile {
                continue;
            }
            match copies.binary_search_by_key(&entry.data.pack_offset, |(offset, _, _)| *offset) {
                Ok(position) if copies[position].1 == entry.data.size_payload => {
                    entry.data.pack_file = copies[position].2.id;
                    entry.data.pack_offset = copies[position].2.offset;
                }
                _ => referenced = true,
            }
        }
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

        lore_base::lore_trace!(
            "Packstore compactor group {group_index} bucket {bucket_index} packfile {packfile} rewrote {} payloads, {copied_size} bytes",
            copies.len()
        );
        (copied_size, referenced)
    }

    /// Copy the `size` bytes at `offset` in `packfile` into a packfile taking writes.
    async fn copy_payload(
        group: &ImmutableStoreGroup,
        packfile: u32,
        offset: u32,
        size: u32,
    ) -> Result<crate::PackStoreRef, crate::packstore::PackfileError> {
        let payload = group.packstore.load(packfile, offset, size).await?;
        group.packstore.store(payload).await
    }

    /// The size of every packfile, as the packstores hold it.
    ///
    /// Claims the store to read: measuring opens the packfiles of every group, which
    /// must not happen outside the store's lock, and outside a span the claim's release
    /// closes them again. Inside a span the claim is two atomic loads. Zero if the store
    /// cannot be claimed.
    pub async fn packstore_total_size(&self) -> usize {
        let Ok(_reading) = self.hold(Intent::Read).await else {
            return 0;
        };
        self.measure_packstores().await
    }

    /// The size of every packfile, as the packstores hold it, measured under a claim the caller
    /// holds.
    ///
    /// A compaction pass measures under the claims it took for itself, never through
    /// [`Self::packstore_total_size`], whose claim a span of whatever use of the store is under
    /// way would cover: that span can end while the pass measures, and the pass runs on its own
    /// schedule.
    async fn measure_packstores(&self) -> usize {
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Measuring).await;
        let mut total_size = 0;
        for group in self.group.iter() {
            total_size += group.packstore.total_size().await.unwrap_or_default();
        }
        total_size
    }

    #[lore_macro::test_pub]
    fn last_access() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Read an entry's last-access stamp.
    ///
    /// This is the only field written while its bucket is read-locked, so a read of it alone goes
    /// through the atomic. A bulk copy of the entry takes it with the rest and may see the
    /// previous stamp; both are times the entry was read.
    fn load_last_access(data: &ImmutableData) -> u64 {
        // SAFETY: the entry outlives the bucket lock its reader holds, and every write to this
        // field is an atomic store through the same cast.
        unsafe { AtomicU64::from_ptr(&data.last_access as *const u64 as *mut u64) }
            .load(atomic::Ordering::Relaxed)
    }

    /// Move an entry's last-access stamp to now, marking `dirty` only when the move reaches
    /// [`ATIME_GRANULARITY_SECONDS`].
    ///
    /// An entry serializes inside its whole bucket, so dirtying on every read would have each
    /// read rewrite the bucket it touched. The stamp advances regardless, so a bucket written for
    /// any other reason carries the current time.
    ///
    /// Dirtying here schedules no flush of its own.
    ///
    /// The stamp is swapped rather than loaded and stored, so that two resolves racing cannot both
    /// read the old stamp and both dirty the bucket. It carries no ordering of its own; `dirty` is
    /// released, and claimed with an acquire in [`ImmutableStoreBucket::serialize`] and
    /// [`ImmutableStoreBucket::serialize_to_new`], which is what orders the stamp before the bytes
    /// a flusher writes. Every other mutation of an entry takes the bucket write lock, whose
    /// release a flusher's read lock already synchronizes with; this one holds a read lock, so the
    /// flag has to carry the edge.
    fn stamp_last_access(data: &ImmutableData, dirty: &AtomicBool) {
        // SAFETY: as `load_last_access`.
        let stamp = unsafe { AtomicU64::from_ptr(&data.last_access as *const u64 as *mut u64) };
        let now = Self::last_access();

        let previous = stamp.swap(now, atomic::Ordering::Relaxed);
        // Which flag this is depends on why the lookup happened — see [`Lookup`]. A
        // read marks the bucket for the next flush; a write marks it as changed.
        if now.saturating_sub(previous) >= ATIME_GRANULARITY_SECONDS {
            dirty.store(true, atomic::Ordering::Release);
        }
    }

    /// Immediate flush of all dirty buckets. Parallel across groups, sequential within a group. A
    /// flush dropped part way leaves each group flush it started running to its end, holding the
    /// group's flush lock, so that no write it started lands after the lock is released, and a
    /// share of the group's writes, so that the store stays claimed while it runs: the flush's
    /// claim goes with the flush, and a group flush has no write in flight between its writes.
    /// Once it ends, it releases the flock if nothing keeps it, see
    /// [`crate::local::fan_out::releasing_once_done`].
    async fn flush_all(
        self: Arc<Self>,
        path: Option<Arc<PathBuf>>,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        let Some(path) = path else {
            return Ok(());
        };

        let mut tasks = lore_base::runtime::DetachingJoinSet::default();

        for group_index in 0..self.group.len() {
            let group = self.group[group_index].clone();

            // A group with no bucket flagged has taken no write since the last flush, so it has
            // nothing to serialize and owes no marker. A marker-less group reloads at
            // bucket_count = 256, correct for an empty group at any level, and its first write
            // fires the two-phase commit. Empty groups cost ~nothing per flush.
            #[cfg(feature = "test-util")]
            crate::test_util::pause_point(crate::test_util::PausePoint::Scanning).await;
            if group
                .flush_lock
                .try_lock()
                .is_ok_and(|_held| group.nothing_to_flush())
            {
                continue;
            }

            let path = path.clone();
            let work = async move {
                // One flusher per group at a time, held for the whole group flush so an
                // overlapping flush cannot observe a half-finished level transition and
                // take the other commit path. See `ImmutableStoreGroup::flush_lock`.
                let _flush_guard =
                    crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
                let _in_flight = crate::local::fan_out::write_share(&group.writes);

                flush_locked_group(group, group_index, path, sync_data).await
            };
            lore_base::lore_spawn!(
                tasks,
                crate::local::fan_out::releasing_once_done(self.lock.clone(), work)
            );
        }

        let mut result = Ok(());
        while let Some(task_result) = tasks.join_next().await {
            result = result.and(
                task_result
                    .internal("Task failed")
                    .map_err(LocalImmutableStoreError::from)
                    .flatten(),
            );
        }

        result?;
        Ok(())
    }

    #[allow(dead_code)]
    async fn ensure_integrity(&self) {
        for group in self.group.iter() {
            let current_time = Self::last_access();
            for bucket in group.bucket.iter().filter_map(|cell| cell.get()) {
                let bucket = bucket.read().await;
                let mut previous_address = Address::default();
                for index in bucket.sorted_index.iter() {
                    let address = bucket.entry[*index as usize].address;
                    if address.hash.cmp(&previous_address.hash).is_lt() {
                        panic!("Immutable store integrity failed, entries not sorted");
                    }
                    let last_access = Self::load_last_access(&bucket.entry[*index as usize].data);
                    if last_access > current_time {
                        panic!("Immutable store entry has last access in the future");
                    }
                    previous_address = address;
                }
            }
        }
    }

    // For test purposes, mark all fragments in all buckets as durably stored
    pub async fn mark_all_as_durably_stored(&self) {
        for group in self.group.iter() {
            for bucket in group.bucket.iter().filter_map(|cell| cell.get()) {
                let mut bucket = bucket.write().await;
                for entry in bucket.entry.iter_mut() {
                    entry.data.flags = (entry.data.flags & !FragmentFlags::PayloadStored)
                        | ImmutableData::stored_flags(
                            entry.data.flags | FragmentFlags::PayloadStoredDurable.bits(),
                            entry.data.pack_file,
                        );
                }
            }
        }
    }

    // For test purposes, mark a single fragment as NOT durably stored
    pub async fn mark_as_not_durably_stored(&self, partition: Partition, address: Address) {
        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        let (bucket_index, mut bucket) = group.bucket_for_write(&address.hash).await;
        if !bucket.deserialized && self.path.is_some() {
            let _ = bucket
                .deserialize(
                    group,
                    self.path.clone().unwrap().as_ref(),
                    group_index,
                    bucket_index,
                    Some(&self.gc_counters),
                )
                .await;
        }
        let (match_slot, _, match_made) =
            Self::lookup(&bucket, partition, address, StoreMatch::MatchFull);
        if match_made == StoreMatch::MatchFull {
            let index = bucket.sorted_index[match_slot] as usize;
            bucket.entry[index].data.flags &= !FragmentFlags::PayloadStoredDurable;
        }
    }
}

impl LockedStore for LocalImmutableStore {
    /// A pass over the dirty flags: an access stamp alone is not a change, so the soft
    /// flags are not consulted.
    fn has_unflushed(&self) -> bool {
        self.group.iter().any(|group| {
            group
                .dirty
                .iter()
                .any(|bucket| bucket.load(atomic::Ordering::Relaxed))
        })
    }

    /// Every group's packstore. The buckets stay loaded: it is the file handles that
    /// must not outlive the flock, not the state read through them.
    fn release_files(&self) -> bool {
        self.group
            .iter()
            .all(|group| group.packstore.release_files())
    }
}

#[async_trait]
impl crate::immutable_store::ImmutableStore for LocalImmutableStore {
    fn is_local(&self) -> bool {
        true
    }

    fn isolates_partitions(&self) -> bool {
        self.settings.isolate_partitions
    }

    async fn is_available(self: Arc<Self>, timeout: Duration) -> bool {
        let mut checks = JoinSet::new();
        for group_index in 0..GROUP_COUNT {
            let store = self.clone();
            lore_base::lore_spawn!(checks, async move {
                let group = &store.group[group_index];
                let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
                for bucket_index in 0..active_buckets {
                    let bucket = group.bucket(bucket_index).clone();
                    tokio::select! {
                        _bucket = bucket.read() => {
                        }
                        _ = tokio::time::sleep(timeout) => {
                            return false;
                        }
                    }
                }
                true
            });
        }

        while let Some(result) = checks.join_next().await {
            if !result.unwrap_or_default() {
                return false;
            }
        }

        true
    }

    async fn hold_for_command(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        // Read intent: this hold is for ordering, not for permission. The first write
        // inside the span advances the epoch then.
        Ok(self.claim(Intent::Read).await?.map(StoreGuard::into_span))
    }

    fn try_join_claim(&self) -> Option<StoreGuard> {
        self.lock.as_ref()?.try_join(Intent::Read)
    }

    fn store_lock(&self) -> Option<Arc<StoreLock>> {
        self.lock.clone()
    }

    async fn hold_for_task(self: Arc<Self>) -> Result<Option<StoreGuard>, StoreError> {
        self.claim(Intent::Read).await
    }

    /// One bucket pass per address establishes the best match, so this store never has a reason to
    /// under-report: it does not cost more to learn that the hash is in the partition than to learn
    /// that it exists at all.
    ///
    /// A tombstone resolves to no match. `obliterate` leaves the entry in the index — the
    /// last-reference scan needs to see it — so this is the one place that has to know the
    /// difference between an entry and a live one. Where the best match is a tombstone and a weaker
    /// live match exists elsewhere, this reports nothing rather than the weaker level; that is
    /// under-reporting, which the contract permits, and it keeps the obliteration rule absolute
    /// rather than conditional on what else happens to be stored.
    ///
    /// Durability is only ever read off a fragment this store actually holds — either recorded on
    /// it when it was cached, or implied for every entry by a store configured durable. An address
    /// that did not match carries no claim at all.
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        let _hold = self.hold(Intent::Read).await?;
        debug_assert_eq!(addresses.len(), results.len());

        for (address, result) in addresses.iter().zip(results.iter_mut()) {
            let found = self
                .find(partition, *address, Lookup::Read)
                .await
                .forward_with::<StoreError, _>(|| {
                    format!("Failed to resolve immutable store {}.", address.hash)
                })?;

            let obliterated = found.data.flags & FragmentFlags::PayloadObliterated.bits() != 0;

            *result = if obliterated || found.matching < self.query_scope() {
                StoreMatchResult::default()
            } else {
                StoreMatchResult {
                    match_made: found.matching,
                    partition: found.partition,
                    context: found.context,
                    stored_local: found.data.pack_file != 0,
                    stored_durable: found.data.flags & FragmentFlags::PayloadStoredDurable.bits()
                        != 0
                        || self.settings.implicit_durable_stored,
                }
            };
        }

        Ok(())
    }

    /// This store holds the fragment it was given, so the representation comes straight off the
    /// entry and there is nothing further to fetch. `PayloadStoredLocal` reports whether the entry
    /// holds its payload, as `query` reports `stored_local`, whatever flags it was stored with.
    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let _hold = self.hold(Intent::Read).await?;
        // Resolved at the strongest level so the caller learns whether the association is its own,
        // then gated on scope: asking `find` for the scope directly would cap the answer there and
        // a full match would come back indistinguishable from a partition one.
        let find = self
            .find(partition, address, Lookup::Read)
            .await
            .forward_with::<StoreError, _>(|| {
                format!("Failed to read immutable store metadata {}.", address.hash)
            })?;

        // `find` matches on address alone, so a tombstoned entry still resolves.
        let obliterated = find.data.flags & FragmentFlags::PayloadObliterated.bits() != 0;

        if obliterated || find.matching < self.read_scope() {
            return Ok(StoreGetData::default());
        }

        let mut stored_flags = ImmutableData::stored_flags(find.data.flags, find.data.pack_file);
        if self.settings.implicit_durable_stored {
            stored_flags |= FragmentFlags::PayloadStoredDurable.bits();
        }

        Ok(StoreGetData::metadata(
            Fragment {
                flags: (find.data.flags & !FragmentFlags::PayloadStored) | stored_flags,
                size_payload: find.data.size_payload,
                size_content: find.data.size_content,
            },
            find.matching,
            find.partition,
        ))
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        let _hold = self.hold(Intent::Read).await?;
        #[cfg(feature = "failure_generator")]
        if self.failure_generator.retry_rate > 0.0
            && rand::random::<f32>() < self.failure_generator.retry_rate
        {
            return Err(StoreError::from(SlowDown));
        }

        let gate = self.group[address.hash.data()[0] as usize]
            .payload_gate
            .hold();
        // Resolved at full strength and then gated on scope, the same way `get_metadata` does it,
        // so the level reported back is the one actually found rather than the one searched at.
        let find = self
            .find(partition, address, Lookup::Read)
            .await
            .forward_with::<StoreError, _>(|| {
                format!("Failed to query immutable store for get {}.", address.hash)
            })?;

        // Not covered by the pack file check below: that one catches this only because obliterate
        // happens to clear the pack file.
        let obliterated = find.data.flags & FragmentFlags::PayloadObliterated.bits() != 0;

        if obliterated || find.matching < self.read_scope() {
            return Err(StoreError::from(AddressNotFound::from(address)));
        }

        let mut local_flags = FragmentFlags::PayloadStoredLocal.bits();
        if self.settings.implicit_durable_stored {
            local_flags |= FragmentFlags::PayloadStoredDurable.bits();
        }

        let fragment = Fragment {
            flags: find.data.flags | local_flags,
            size_payload: find.data.size_payload,
            size_content: find.data.size_content,
        };

        if find.data.pack_file == 0 {
            return Err(StoreError::from(PayloadNotFound::from(address.hash)));
        }

        crate::validate_fragment_payload(&fragment, find.data.size_payload as usize)?;
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Located).await;
        let payload = Self::load(&self.group[find.group].packstore, find.data)
            .await
            .forward::<StoreError>("Failed to load payload from local storage.")?;
        drop(gate);
        crate::validate_fragment_payload(&fragment, payload.len())?;

        Ok(StoreGetData {
            fragment,
            match_made: find.matching,
            partition: find.partition,
            payload: Some(payload),
            content: None,
        })
    }

    /// One index lookup settles where the payload belongs and reads it there. A payload that is the
    /// content scatters straight into `dst` and allocates nothing; any other is read into its own
    /// buffer, which the reader needs to expand or walk it. Applies the same gating as `get`.
    async fn get_into(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        dst: &mut crate::CallerBuffer,
    ) -> Result<(Fragment, PayloadRead), StoreError> {
        let _hold = self.hold(Intent::Read).await?;
        let _gate = self.group[address.hash.data()[0] as usize]
            .payload_gate
            .hold();
        let find = self
            .find(partition, address, Lookup::Read)
            .await
            .forward_with::<StoreError, _>(|| {
                format!(
                    "Failed to query immutable store for get_into {}.",
                    address.hash
                )
            })?;

        let obliterated = find.data.flags & FragmentFlags::PayloadObliterated.bits() != 0;
        if obliterated || find.matching < self.read_scope() {
            return Err(StoreError::from(AddressNotFound::from(address)));
        }

        let mut local_flags = FragmentFlags::PayloadStoredLocal.bits();
        if self.settings.implicit_durable_stored {
            local_flags |= FragmentFlags::PayloadStoredDurable.bits();
        }

        let fragment = Fragment {
            flags: find.data.flags | local_flags,
            size_payload: find.data.size_payload,
            size_content: find.data.size_content,
        };

        crate::validate_fragment_size(&fragment)?;

        if find.data.pack_file == 0 {
            return Err(StoreError::from(PayloadNotFound::from(address.hash)));
        }

        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Located).await;
        if !crate::payload_is_content(&fragment) {
            let payload = Self::load(&self.group[find.group].packstore, find.data)
                .await
                .forward::<StoreError>("Failed to load payload from local storage.")?;
            crate::validate_fragment_payload(&fragment, payload.len())?;
            return Ok((fragment, PayloadRead::Returned(payload)));
        }

        let size_payload = find.data.size_payload as usize;
        crate::validate_buffer_capacity(size_payload, dst.len())?;

        self.group[find.group]
            .packstore
            .load_into(
                find.data.pack_file,
                find.data.pack_offset,
                find.data.size_payload,
                dst,
            )
            .await
            .forward::<StoreError>(
                "Failed to load payload into caller buffer from local storage.",
            )?;

        Ok((fragment, PayloadRead::IntoBuffer))
    }

    /// A put without its payload over the tombstone of its association is refused with
    /// `PayloadNotFound`, and one with its payload replaces the tombstone.
    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        mut fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        let _hold = self.hold(Intent::Write).await?;
        sanitise_fragment_behavior_flags(&mut fragment);

        let payload_len = payload.as_ref().map(Bytes::len);
        if !crate::immutable_store::put_is_valid(&fragment, payload_len) {
            #[cfg(feature = "test-util")]
            let enforced = !crate::test_util::unvalidated_puts_accepted();
            #[cfg(not(feature = "test-util"))]
            let enforced = true;
            crate::immutable_store::refuse_put(&fragment, payload_len, enforced)?;
        }

        #[cfg(feature = "failure_generator")]
        if self.failure_generator.retry_rate > 0.0
            && rand::random::<f32>() < self.failure_generator.retry_rate
        {
            return Err(StoreError::from(SlowDown));
        }

        #[cfg(feature = "failure_generator")]
        if self
            .failure_generator
            .miss_fragment_writes
            .contains(&address.hash)
        {
            lore_base::lore_warn!(
                "Skipping write for fragment with hash: {} based on failure generator configuration",
                address.hash
            );
            return Ok(());
        }
        #[cfg(feature = "failure_generator")]
        if self
            .failure_generator
            .refuse_fragment_writes
            .lock()
            .remove(&address.hash)
        {
            return Err(StoreError::internal(format!(
                "Refusing the first write for fragment with hash {} based on failure generator configuration",
                address.hash
            )));
        }
        if force && payload.is_some() {
            lore_base::lore_debug!("Force overwrite fragment in local store");
            return self
                .store(partition, address, fragment, payload, force)
                .await
                .forward_with(|| {
                    format!(
                        "Failed to store in immutable store for put {}",
                        address.hash
                    )
                });
        }

        let find = self
            .find(partition, address, Lookup::Write)
            .await
            .forward_with::<StoreError, _>(|| {
                format!(
                    "Failed to find in immutable store for put {}.",
                    address.hash
                )
            })?;

        if find.matching == StoreMatch::MatchFull
            && payload.is_none()
            && find.data.flags & FragmentFlags::PayloadObliterated != 0
        {
            return Err(StoreError::from(PayloadNotFound::from(address.hash)));
        }
        if find.matching != StoreMatch::MatchNone
            && fragment.size_content != find.data.size_content
            && (!force || payload.is_none())
        {
            if (find.data.flags & FragmentFlags::PayloadObliterated) == 0 {
                return Err(StoreError::internal("Hash collision"));
            } else {
                lore_base::lore_warn!("Overwriting obliterated fragment at {address}");
            }
        }

        match find.matching {
            StoreMatch::MatchFull => {
                let new_payload = find.data.pack_file == 0 && payload.is_some();
                let local_to_durable = (find.data.flags & FragmentFlags::PayloadStoredDurable) == 0
                    && (fragment.flags & FragmentFlags::PayloadStoredDurable) != 0;
                if new_payload || local_to_durable || force {
                    // Inherit `PayloadStoredDurable` from the existing entry. Without this, a
                    // pure-local put racing a previously-durable write (e.g., remote upload that
                    // ran in another task) would overwrite a metadata-only durable entry with a
                    // payload-bearing fragment that has the durable flag cleared, losing the
                    // remote-confirmation bookkeeping. OR-merging is safe across all three branch
                    // conditions: `local_to_durable` already has the bit set in `fragment.flags`
                    // so the merge is a no-op there; `force` callers shouldn't be silently
                    // dropping durable status; `new_payload` is the case this fix targets.
                    let mut fragment = fragment;
                    fragment.flags |= find.data.flags & FragmentFlags::PayloadStoredDurable;
                    self.store(partition, address, fragment, payload, force)
                        .await
                        .forward_with::<StoreError, _>(|| {
                            format!(
                                "Failed to store in immutable store for put {}.",
                                address.hash
                            )
                        })?;
                }
            }

            #[allow(clippy::match_same_arms)]
            StoreMatch::MatchPartition => {
                self.store(partition, address, fragment, payload, force)
                    .await
                    .forward_with::<StoreError, _>(|| {
                        format!(
                            "Failed to store in immutable store for put {}.",
                            address.hash
                        )
                    })?;
            }

            StoreMatch::MatchHash | StoreMatch::MatchNone => {
                self.store(partition, address, fragment, payload, force)
                    .await
                    .forward_with::<StoreError, _>(|| {
                        format!(
                            "Failed to store in immutable store for put {}.",
                            address.hash
                        )
                    })?;
            }
        }

        Ok(())
    }

    /// A fragment list held without its payload fails with `PayloadNotFound` and obliterates
    /// nothing: the fragments it names are out of reach without it, and obliterating the list
    /// alone would leave them readable with no list for a retry to follow.
    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let _hold = self.hold(Intent::Write).await?;
        timed!(
            self.instruments.operation_latency,
            &self
                .instruments
                .get_labels_for_operation_context("obliterate"),
            {
                lore_base::lore_debug!("Obliterating address {address}");

                // `find` reads the entry and releases the bucket, which is what
                // this needs: the sub-fragments below each choose their own group
                // and bucket from their own hash, and one in `bucket_count` of
                // them chooses the bucket this address lives in.
                // `tokio::sync::RwLock` is not reentrant, so descending into them
                // while holding that lock waits on a lock this task already owns.
                // The fan-out level sets the odds: one child in 65,536 at 256
                // buckets to a group, one in 256 at one bucket, where every child
                // in the parent's group collides.
                let group = &self.group[address.hash.data()[0] as usize];
                let gate = group.payload_gate.hold();
                let found = self
                    .find(partition, address, Lookup::Write)
                    .await
                    .forward::<StoreError>("Failed to deserialize store data.")?;

                lore_base::lore_debug!("Lookup match for {address}: {:?}", found.matching);

                if found.matching != StoreMatch::MatchFull {
                    return Err(StoreError::from(AddressNotFound::from(address)));
                }
                let data = found.data;

                let references = if (data.flags & FragmentFlags::PayloadFragmented) != 0 {
                    lore_base::lore_debug!("Payload fragmented, obliterating subfragments");
                    #[cfg(feature = "test-util")]
                    crate::test_util::pause_point(crate::test_util::PausePoint::Located).await;
                    if data.pack_file == 0 {
                        return Err(StoreError::from(crate::errors::PayloadNotFound::from(
                            address.hash,
                        )));
                    }
                    Some(
                        Self::load(&group.packstore, data)
                            .await
                            .forward_with::<StoreError, _>(|| {
                                format!("loading the fragment list of {address}")
                            })?,
                    )
                } else {
                    None
                };
                drop(gate);

                if let Some(payload) = references {
                    let payload = payload.to_aligned::<FragmentReference>();
                    for reference in payload.as_type_slice::<FragmentReference>().iter() {
                        self.clone()
                            .obliterate(
                                partition,
                                Address {
                                    context: address.context,
                                    hash: reference.hash,
                                },
                                stats.clone(),
                            )
                            .await
                            .forward_with::<StoreError, _>(|| {
                                format!("Failed to obliterate immutable {address}.")
                            })?;
                    }
                }

                // Everything this address referenced is gone, so the address
                // itself can go. The lock is taken again rather than held
                // throughout: what it guards is this entry, and none of the walk
                // above needed it.
                self.clone().obliterate_one(partition, address, stats).await
            }
        )
        .into()
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        // **Claimed only where something is actually evicted, and never waited for.** This
        // pass runs on a timer for the life of the process, including while nothing has the
        // store open, and a process with nothing open blocks nobody — so it looks at what
        // the store holds in memory without a claim, and `evict_oldest` claims to write at
        // its first over-capacity bucket, leaving a store in use elsewhere for a later pass.
        // That claim reloads a store another process changed before anything is evicted
        // from it. No claim is taken to look, because a write claim advances the epoch as it
        // is taken: claiming to write while evicting nothing would tell every other process
        // on the store that it changed, several times a minute.
        let writing;
        let evicted = timed!(
            self.instruments.operation_latency,
            &self.instruments.get_labels_for_operation_context("evict"),
            {
                let (count, claim) = self.evict_oldest(max_capacity, sink.as_ref()).await;
                writing = claim;
                Ok(count)
            }
        );
        // **What a pass evicts, the pass writes, under its own claim.** Eviction empties
        // buckets in memory and nothing else is scheduled to write them, so left dirty they
        // would keep the flock for as long as this process kept the store — for a timer
        // running in the background of a process that has finished with the store,
        // indefinitely. The write runs on the claim the eviction took rather than through
        // `flush`, whose claim a span of whatever use of the store is under way would cover,
        // and that span can end while the write runs.
        if let Ok(count) = &evicted.result
            && *count > 0
        {
            let _ = self.clone().flush_all(self.path.clone(), sync_data).await;
        }
        drop(writing);
        evicted.into()
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        // Unclaimed, for the reason `evict` gives: `compact_packfiles` claims the store once
        // the sizes it holds say the store is over its cap, and leaves a store in use
        // elsewhere for a later pass.
        let compacted = timed!(
            self.instruments.operation_latency,
            &self.instruments.get_labels_for_operation_context("compact"),
            {
                self.clone()
                    .compact_packfiles(max_size, at, sync_data, sink)
                    .await
            }
        );
        compacted.into()
    }

    async fn load_index(self: Arc<Self>) -> Result<(), StoreError> {
        let _hold = self.hold(Intent::Read).await?;
        self.deserialize_all_buckets()
            .await
            .forward::<StoreError>("loading the store's index")
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        if let Some(path) = self.path.as_deref() {
            lore_io::IoDriver::global()
                .read_file_bytes(path.join(DOT_COMPACT))
                .await
                .ok()
                .and_then(|bytes| {
                    lore_base::lore_debug!("Reading compactor resume point");
                    bytes.as_ref().try_into().ok().map(usize::from_ne_bytes)
                })
        } else {
            None
        }
    }

    async fn stop_gc(self: Arc<Self>, terminate: bool) {
        let _request = GcStopRequest::raise(&self.stop_requests, terminate);
        // Taking both permits is what waits for the passes in flight to give up.
        let _evict = self.eviction.acquire().await;
        let _compact = self.compaction.acquire().await;
    }

    /// Walks each group under its flush lock, which a fan-out holds as it moves the group's
    /// entries between buckets, so the walk reads one layout and heals each failure in the bucket
    /// it found it in. Holds the eviction and compaction permits for the whole walk: those passes
    /// rewrite the entries and packfiles being read, so one running alongside would report
    /// failures that are only the store moving underneath it. Fails, walking nothing, when a
    /// bucket cannot be loaded.
    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        let _hold = self
            .hold(if heal { Intent::Write } else { Intent::Read })
            .await?;
        let evict_permit = self.eviction.acquire().await;
        let compact_permit = self.compaction.acquire().await;

        self.deserialize_all_buckets()
            .await
            .forward::<StoreError>("Failed to load the store to verify")?;

        let mut failed_count = 0;
        let mut fragment_count = 0;
        let mut verified_count = 0;
        for (group_index, group) in self.group.iter().enumerate() {
            let _flushes =
                crate::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
            let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
            #[cfg(feature = "test-util")]
            crate::test_util::pause_point(crate::test_util::PausePoint::Checking).await;
            let mut failed = vec![];
            for bucket_index in 0..active_buckets {
                let bucket = group.bucket(bucket_index).read().await;

                for entry in bucket.entry.iter() {
                    fragment_count += 1;

                    if entry.data.pack_file == 0 {
                        continue;
                    }

                    let Ok(buffer) = group.packstore
                        .load(
                            entry.data.pack_file,
                            entry.data.pack_offset,
                            entry.data.size_payload,
                        )
                        .await
                        .inspect_err(|_err| {
                            lore_base::lore_warn!( "Failed to load data for verification: packfile {} offset {} payload size {}",
                                    entry.data.pack_file,
                                    entry.data.pack_offset,
                                    entry.data.size_payload);
                        }) else {
                        failed.push((bucket_index, entry.address.hash, entry.data));
                        continue;
                    };

                    if entry.data.flags & FragmentFlags::PayloadCompressed != 0 {
                        let Ok((_fragment, buffer)) = compress::decompress(
                            Fragment {
                                flags: entry.data.flags,
                                size_payload: entry.data.size_payload,
                                size_content: entry.data.size_content,
                            },
                            &buffer,
                        )
                            .inspect_err(|err| {
                                lore_base::lore_warn!("Failed decompress payload data: group {group_index} bucket {bucket_index} payload size {} content size {} packfile {} offset {} size {}: {}",
                                entry.data.size_payload,
                                entry.data.size_content,
                                entry.data.pack_file,
                                entry.data.pack_offset,
                                entry.data.size_payload,
                                err);
                                lore_base::lore_warn!("First 64 bytes {:?}", &buffer[..std::cmp::min(64, buffer.len())]);
                            }) else {
                            failed.push((bucket_index, entry.address.hash, entry.data));
                            continue;
                        };

                        if buffer.len() != entry.data.size_content as usize {
                            lore_base::lore_warn!(
                                "Decompressed data failed data size validation: group {group_index} bucket {bucket_index} payload size {} content size {} packfile {} offset {} size {} decompressed size {}",
                                entry.data.size_payload,
                                entry.data.size_content,
                                entry.data.pack_file,
                                entry.data.pack_offset,
                                entry.data.size_payload,
                                buffer.len()
                            );
                            failed.push((bucket_index, entry.address.hash, entry.data));
                            continue;
                        }

                        let hash = hash::hash_slice(&buffer);
                        if hash != entry.address.hash {
                            lore_base::lore_warn!(
                                "Decompressed data failed hash validation: group {group_index} bucket {bucket_index} payload size {} content size {} packfile {} offset {} size {}",
                                entry.data.size_payload,
                                entry.data.size_content,
                                entry.data.pack_file,
                                entry.data.pack_offset,
                                entry.data.size_payload
                            );
                            failed.push((bucket_index, entry.address.hash, entry.data));
                            continue;
                        }
                    } else {
                        let hash = hash::hash_slice(&buffer);
                        if hash != entry.address.hash {
                            lore_base::lore_warn!(
                                "Raw data failed hash validation: group {group_index} bucket {bucket_index} payload size {} content size {} packfile {} offset {} size {}",
                                entry.data.size_payload,
                                entry.data.size_content,
                                entry.data.pack_file,
                                entry.data.pack_offset,
                                entry.data.size_payload
                            );
                            lore_base::lore_warn!(
                                "First 64 bytes {:?}",
                                &buffer[..std::cmp::min(64, buffer.len())]
                            );
                            failed.push((bucket_index, entry.address.hash, entry.data));
                            continue;
                        }
                    }

                    verified_count += 1;
                }
            }

            for (failed_bucket_index, failed_hash, failed_data) in failed.iter() {
                for bucket_index in 0..active_buckets {
                    let bucket = group.bucket(bucket_index).read().await;

                    for (entry_index, entry) in bucket.entry.iter().enumerate() {
                        if bucket_index == *failed_bucket_index
                            && entry.address.hash == *failed_hash
                            && entry.data.pack_file == failed_data.pack_file
                            && entry.data.pack_offset == failed_data.pack_offset
                        {
                            continue;
                        }

                        if entry.data.pack_file != failed_data.pack_file {
                            continue;
                        }

                        if entry.data.pack_offset
                            >= failed_data.pack_offset + failed_data.size_payload
                            || failed_data.pack_offset
                                >= entry.data.pack_offset + entry.data.size_payload
                        {
                            continue;
                        }

                        lore_base::lore_warn!(
                            "Data overlap, failing data {failed_data:?} overlaps with data {:?} in group {group_index} bucket {bucket_index} entry {entry_index}",
                            entry.data
                        );
                    }
                }

                if heal {
                    let mut sorted_index = GrowVec::new();
                    let mut entry = GrowVec::new();

                    let mut bucket = group.bucket(*failed_bucket_index).write().await;
                    for index in bucket.sorted_index.iter() {
                        let index = *index as usize;
                        {
                            let this_entry = &bucket.entry[index];
                            if this_entry.address.hash == *failed_hash
                                && this_entry.data.pack_file == failed_data.pack_file
                                && this_entry.data.pack_offset == failed_data.pack_offset
                            {
                                continue;
                            }
                        }

                        let new_index = entry.len() as u32;
                        sorted_index.push(new_index);
                        entry.push(bucket.entry[index]);
                    }

                    bucket.sorted_index = sorted_index;
                    bucket.entry = entry;

                    group.dirty[*failed_bucket_index].store(true, atomic::Ordering::Relaxed);
                }
            }
            failed_count += failed.len();
        }

        lore_base::lore_debug!(
            "Verified {verified_count} fragments with payloads of {fragment_count} total fragments"
        );
        if failed_count > 0 {
            lore_base::lore_debug!("{failed_count} invalid fragments");
            if heal {
                drop(evict_permit);
                drop(compact_permit);
                let _ = crate::immutable_store::ImmutableStore::flush(self.clone(), false).await;

                lore_base::lore_debug!("Store healing complete");
            }
        }
        lore_base::lore_debug!("Store verification complete");

        Ok(())
    }

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        // **Claimed only as far as the work needs.** This runs after every command and on
        // every handle close, and a write claim advances the epoch — so claiming to write
        // over a store with nothing to write would tell every other process its in-memory
        // state is worthless, after every read-only command. Access stamps change no
        // entry and are written under a read claim.
        //
        // Each group is read under its flush lock, once every bucket write it had in flight
        // has landed: a write claims a bucket's mark under the lock and sets it again if it
        // fails, see [`SerializeFailureGuard`], so a group read clean while a write ran may
        // have a bucket to write once the write has ended.
        let mut pending = PendingFlush::Nothing;
        for group in &self.group {
            let _held =
                crate::local::fan_out::await_bucket_write(&group.flush_lock, &group.writes).await;
            match group.pending_flush() {
                PendingFlush::Changes => {
                    pending = PendingFlush::Changes;
                    break;
                }
                PendingFlush::Stamps => pending = PendingFlush::Stamps,
                PendingFlush::Nothing => {}
            }
        }
        let intent = match pending {
            PendingFlush::Nothing => {
                // Nothing is flagged, so a flock an earlier span left held over state
                // since written has nothing to keep it.
                self.note_flushed();
                return Ok(());
            }
            PendingFlush::Stamps => Intent::Read,
            PendingFlush::Changes => Intent::Write,
        };
        let hold = self.hold(intent).await?;
        let flushed = self
            .clone()
            .flush_all(Some(path), sync_data)
            .await
            .forward("Failed to flush store to disk");
        // The claim this flush took, if it took one, goes here, and releases the flock if
        // nothing is flagged. What a failed flush could not write stays flagged, and keeps
        // the store claimed.
        drop(hold);
        flushed
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn fragment_count(self: Arc<Self>) -> Option<usize> {
        let Ok(_hold) = self.hold(Intent::Read).await else {
            return None;
        };
        let mut fragment_count = 0;
        for group in self.group.iter() {
            for bucket in group.bucket.iter().filter_map(|cell| cell.get()) {
                fragment_count += bucket.read().await.entry.len();
            }
        }
        Some(fragment_count)
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        let _hold = self.hold(Intent::Write).await?;
        // Hash is preserved across the copy; the destination address only differs in context.
        // Same hash → same bucket, so source and destination always live in one bucket — including
        // the same-partition different-context case used for in-partition payload dedup, and the
        // zero-context source that names any association the source partition holds.
        let destination_address = Address {
            hash: source_address.hash,
            context: destination_context,
        };

        let group_index = source_address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        let (bucket_index, mut bucket) = group.bucket_for_write(&source_address.hash).await;

        if !bucket.deserialized && self.path.is_some() {
            bucket
                .deserialize(
                    group,
                    self.path.clone().unwrap().as_ref(),
                    group_index,
                    bucket_index,
                    Some(&self.gc_counters),
                )
                .await
                .forward_with::<StoreError, _>(|| {
                    format!(
                        "Failed to deserialize storage bucket for copy {}.",
                        source_address.hash
                    )
                })?;
        }

        let Some(source_slot) = Self::copy_source_slot(&bucket, source_partition, source_address)
        else {
            return Err(StoreError::from(AddressNotFound::from(source_address)));
        };

        let source_data = bucket.entry[bucket.sorted_index[source_slot] as usize].data;

        let (dest_slot, insert_slot, dest_match) = Self::lookup(
            &bucket,
            destination_partition,
            destination_address,
            StoreMatch::MatchFull,
        );

        if dest_match == StoreMatch::MatchFull {
            let index = bucket.sorted_index[dest_slot] as usize;
            let entry = &mut bucket.entry[index];
            let before = entry.data;
            entry
                .data
                .merge_from_copy_source(source_data, behavior.durable);
            if entry.data != before {
                entry.data.last_access = Self::last_access();
                let flags = entry.data.flags;
                group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
                drop(bucket);
                self.flush_later(group_index, flags).await;
            }
            return Ok(());
        }

        let mut data = ImmutableData::default();
        data.merge_from_copy_source(source_data, behavior.durable);
        data.last_access = Self::last_access();

        let count = bucket.entry.len();
        bucket.sorted_index.insert(insert_slot, count as u32);
        bucket.entry.push(ImmutableStoreEntry {
            address: destination_address,
            partition: destination_partition,
            data,
        });
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        drop(bucket);
        self.flush_later(group_index, data.flags).await;

        Ok(())
    }
}

impl LocalImmutableStore {
    pub async fn verify_fragment(
        self: Arc<Self>,
        address: Address,
        partition: Partition,
        match_requested: StoreMatch,
        heal: bool,
    ) -> Result<ImmutableStoreVerifyResult, StoreError> {
        let Some(path) = self.path.clone() else {
            lore_base::lore_warn!("Cannot verify fragment, no path to store");
            return Err(StoreError::internal(
                "Cannot verify fragment: no path to store",
            ));
        };
        // Claimed like any other operation on the store's files, and to write when it may
        // heal: a heal rewrites a bucket, and doing that unclaimed would let a reload from
        // another operation empty the bucket underneath it.
        let _hold = self
            .hold(if heal { Intent::Write } else { Intent::Read })
            .await?;
        self.clone()
            .verify_fragment_held(path, address, partition, match_requested, heal)
            .await
    }

    /// The body of [`Self::verify_fragment`], run under the claim that function holds.
    async fn verify_fragment_held(
        self: Arc<Self>,
        path: Arc<PathBuf>,
        address: Address,
        partition: Partition,
        match_requested: StoreMatch,
        heal: bool,
    ) -> Result<ImmutableStoreVerifyResult, StoreError> {
        let mut result = ImmutableStoreVerifyResult::default();

        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        result.group_index = group_index;

        let gate = group.payload_gate.hold();
        let (bucket_index, bucket_ref, bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&address.hash, n);
            let bucket_ref = group.bucket(idx).clone();
            let bucket = bucket_ref.clone().read_owned().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) != n {
                drop(bucket);
                continue;
            }
            if !bucket.deserialized && self.path.is_some() {
                drop(bucket);
                {
                    let mut bucket = bucket_ref.clone().write_owned().await;
                    if group.bucket_count.load(atomic::Ordering::Relaxed) != n {
                        drop(bucket);
                        continue;
                    }
                    if !bucket.deserialized {
                        bucket
                            .deserialize(
                                group,
                                path.as_ref(),
                                group_index,
                                idx,
                                Some(&self.gc_counters),
                            )
                            .await
                            .forward::<StoreError>("Failed to deserialize store data.")?;
                    }
                }
                let bucket = bucket_ref.clone().read_owned().await;
                if group.bucket_count.load(atomic::Ordering::Relaxed) != n {
                    drop(bucket);
                    continue;
                }
                break (idx, bucket_ref, bucket);
            }
            break (idx, bucket_ref, bucket);
        };
        result.bucket_index = bucket_index;

        let bucket_path = format_bucket_path(path.as_ref(), group_index, bucket_index);

        result.index_path = bucket_path.clone();
        result.entry_count = bucket.entry.len();

        let (lookup_repo, lookup_address) = match match_requested {
            StoreMatch::MatchNone | StoreMatch::MatchHash => (
                Partition::default(),
                Address {
                    hash: address.hash,
                    context: Context::default(),
                },
            ),
            StoreMatch::MatchPartition => (
                partition,
                Address {
                    hash: address.hash,
                    context: Context::default(),
                },
            ),
            StoreMatch::MatchFull => (partition, address),
        };

        let (match_slot, _start, match_made) =
            Self::lookup(&bucket, lookup_repo, lookup_address, match_requested);

        if match_made == StoreMatch::MatchNone || match_made < match_requested {
            return Err(StoreError::from(AddressNotFound::from(address)));
        }

        let index = bucket.sorted_index[match_slot] as usize;
        let entry = &bucket.entry[index];

        let mut entries = HashSet::new();

        entries.insert(entry.data);
        result.matches.push(ImmutableStoreVerifyMatch {
            slot: match_slot,
            index,
            partition: entry.partition,
            address: entry.address,
            data: entry.data,
        });

        let matches = |e: &ImmutableStoreEntry| -> bool {
            match match_requested {
                StoreMatch::MatchHash => e.address.hash == address.hash,
                StoreMatch::MatchPartition => {
                    e.address.hash == address.hash && e.partition == partition
                }
                StoreMatch::MatchFull => {
                    e.address.hash == address.hash
                        && e.partition == partition
                        && e.address.context == address.context
                }
                StoreMatch::MatchNone => false,
            }
        };

        if match_requested != StoreMatch::MatchFull {
            let mut backward = match_slot.checked_sub(1);
            let mut forward = (match_slot + 1 < result.entry_count).then_some(match_slot + 1);

            while backward.is_some() || forward.is_some() {
                if let Some(slot) = backward {
                    let index = bucket.sorted_index[slot] as usize;
                    let entry = &bucket.entry[index];
                    if matches(entry) {
                        entries.insert(entry.data);
                        result.matches.push(ImmutableStoreVerifyMatch {
                            slot,
                            index,
                            partition: entry.partition,
                            address: entry.address,
                            data: entry.data,
                        });
                        backward = slot.checked_sub(1);
                    } else {
                        backward = None;
                    }
                }

                if let Some(slot) = forward {
                    let index = bucket.sorted_index[slot] as usize;
                    let entry = &bucket.entry[index];
                    if matches(entry) {
                        entries.insert(entry.data);
                        result.matches.push(ImmutableStoreVerifyMatch {
                            slot,
                            index,
                            partition: entry.partition,
                            address: entry.address,
                            data: entry.data,
                        });
                        forward = (slot + 1 < result.entry_count).then_some(slot + 1);
                    } else {
                        forward = None;
                    }
                }
            }
        }

        drop(bucket);

        result.packfile_entry_count = entries.len();

        let mut failed_data: Vec<ImmutableData> = Vec::new();
        let mut missing_payload = false;

        for data in entries {
            // A tombstone holds no payload by design, so it is neither missing one nor healed.
            if data.flags & FragmentFlags::PayloadObliterated.bits() != 0 {
                continue;
            }

            if data.pack_file == 0 {
                missing_payload = true;
                result.verification_result =
                    Err(VerifyFragmentError::internal("no payload stored"));
                continue;
            }

            #[cfg(feature = "test-util")]
            crate::test_util::pause_point(crate::test_util::PausePoint::Located).await;
            let packstore_bytes = self.group[group_index]
                .packstore
                .load(data.pack_file, data.pack_offset, data.size_payload)
                .await
                .forward::<StoreError>("Failed to load payload from local storage.")?;

            let packstore_hash = if data.flags & FragmentFlags::PayloadCompressed != 0 {
                match compress::decompress(
                    Fragment {
                        flags: data.flags,
                        size_payload: data.size_payload,
                        size_content: data.size_content,
                    },
                    &packstore_bytes,
                ) {
                    Ok((_fragment, bytes)) => Some(Hash::hash_buffer(&bytes)),
                    Err(e) => {
                        lore_base::lore_warn!(
                            "Failed to decompress payload for data {data:?}: {e:?}"
                        );
                        result.verification_result = Err(VerifyFragmentError::internal(
                            "payload decompression failed",
                        ));
                        failed_data.push(data);
                        None
                    }
                }
            } else {
                Some(Hash::hash_buffer(&packstore_bytes))
            };

            if let Some(hash) = packstore_hash
                && hash != address.hash
            {
                lore_base::lore_warn!(
                    "Loaded {} bytes from packstore from packfile {} at offset {} with length {}, but the actual hash ({hash}) did not match the expected value",
                    packstore_bytes.len(),
                    data.pack_file,
                    data.pack_offset,
                    data.size_payload
                );
                result.verification_result = Err(VerifyFragmentError::internal(format!(
                    "hash mismatch: expected {}, got {hash}",
                    address.hash
                )));
                failed_data.push(data);
            }
        }
        drop(gate);

        if heal && (!failed_data.is_empty() || missing_payload) {
            // An entry left behind without a payload still answers a full match, so it has to go
            // rather than be cleared. Obliterated entries are tombstones and stay.
            let mut bucket = bucket_ref.write().await;

            let mut sorted_index = GrowVec::new();
            let mut entry = GrowVec::new();
            let mut dropped = 0;
            for index in bucket.sorted_index.iter() {
                let index = *index as usize;
                let candidate = &bucket.entry[index];
                let obliterated =
                    candidate.data.flags & FragmentFlags::PayloadObliterated.bits() != 0;
                let unserviceable = candidate.data.pack_file == 0
                    || failed_data.iter().any(|failed| {
                        candidate.data.pack_file == failed.pack_file
                            && candidate.data.pack_offset == failed.pack_offset
                    });
                if candidate.address.hash == address.hash && unserviceable && !obliterated {
                    dropped += 1;
                    continue;
                }

                let new_index = entry.len() as u32;
                sorted_index.push(new_index);
                entry.push(bucket.entry[index]);
            }

            bucket.sorted_index = sorted_index;
            bucket.entry = entry;

            lore_base::lore_warn!(
                "Verify dropped {dropped} unserviceable association(s) for {address}, across every partition that held one. The payload has to be stored again"
            );

            self.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
            drop(bucket);

            let _ = crate::immutable_store::ImmutableStore::flush(self.clone(), false).await;
            result.healed = true;
        }

        Ok(result)
    }
}

// Types that were in urc-core/src/store.rs but reference immutable store types
#[derive(Debug, Default)]
pub struct ImmutableStoreVerifyMatch {
    pub slot: usize,
    pub index: usize,
    pub partition: Partition,
    pub address: Address,
    pub data: ImmutableData,
}

pub type VerifyFragmentError = LocalImmutableStoreError;

#[derive(Debug)]
pub struct ImmutableStoreVerifyResult {
    pub hash: Hash,
    pub partition: Partition,
    pub context: Context,
    pub group_index: usize,
    pub bucket_index: usize,
    pub index_path: PathBuf,
    pub entry_count: usize,
    pub packfile_entry_count: usize,
    pub matches: Vec<ImmutableStoreVerifyMatch>,
    pub verification_result: Result<(), VerifyFragmentError>,
    pub healed: bool,
}

impl Default for ImmutableStoreVerifyResult {
    fn default() -> Self {
        ImmutableStoreVerifyResult {
            hash: Default::default(),
            partition: Default::default(),
            context: Default::default(),
            group_index: Default::default(),
            bucket_index: Default::default(),
            index_path: Default::default(),
            entry_count: Default::default(),
            packfile_entry_count: Default::default(),
            matches: Default::default(),
            verification_result: Ok(()),
            healed: false,
        }
    }
}

static STORE_ATTRIBUTES: LazyLock<[KeyValue; 1]> =
    LazyLock::new(|| [KeyValue::new("store", "local")]);

#[derive(Default)]
struct ImmutableStoreInstrumentProvider {}

impl InstrumentProvider for ImmutableStoreInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.store.immutable.local"
    }

    fn labels(&self) -> &[KeyValue] {
        STORE_ATTRIBUTES.as_slice()
    }
}

struct StoreInstruments {
    instrument_provider: ImmutableStoreInstrumentProvider,
    operation_latency: Histogram<f64>,
    compaction: CompactionInstruments,
}

impl InstrumentProvider for StoreInstruments {
    fn namespace(&self) -> &'static str {
        self.instrument_provider.namespace()
    }

    fn labels(&self) -> &[KeyValue] {
        self.instrument_provider.labels()
    }
}

impl Default for StoreInstruments {
    fn default() -> Self {
        let instrument_provider = ImmutableStoreInstrumentProvider::default();
        let operation_latency =
            instrument_provider.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME);

        Self {
            instrument_provider,
            operation_latency,
            compaction: CompactionInstruments::default(),
        }
    }
}

#[lore_macro::test_pub]
#[derive(Clone)]
struct CompactionInstruments {
    target_size: Gauge<u64>,
    total_size: Gauge<u64>,
    group_target_size: Gauge<u64>,
    group_evicted_count: Counter<u64>,
    group_evicted_size: Gauge<u64>,
    group_final_total_size: Gauge<u64>,
    final_total_size: Gauge<u64>,
}

impl Default for CompactionInstruments {
    fn default() -> Self {
        let instrument_provider = ImmutableStoreInstrumentProvider {};

        Self {
            target_size: instrument_provider.gauge("compaction_target_size"),
            total_size: instrument_provider.gauge("compaction_total_size"),
            group_target_size: instrument_provider.gauge("compaction_group_target_size"),
            group_evicted_count: instrument_provider.counter("compaction_group_evicted_count"),
            group_evicted_size: instrument_provider.gauge("compaction_group_evicted_size"),
            group_final_total_size: instrument_provider.gauge("compaction_group_final_total_size"),
            final_total_size: instrument_provider.gauge("compaction_final_size"),
        }
    }
}

// Re-export maintenance functions from dedicated module
pub use crate::maintenance::compactor;
pub use crate::maintenance::evictor;
pub use crate::maintenance::gc;

#[derive(Clone, Copy)]
pub struct ImmutableStoreCreateOptions {
    pub max_capacity: Option<usize>,
    pub eviction_delay: Option<Duration>,
    pub max_size: Option<usize>,
    pub compaction_delay: Option<Duration>,
}

impl ImmutableStoreCreateOptions {
    pub fn none() -> Self {
        Self {
            max_capacity: None,
            eviction_delay: None,
            max_size: None,
            compaction_delay: None,
        }
    }
}

pub(crate) async fn flush_locked_group(
    group: Arc<ImmutableStoreGroup>,
    group_index: usize,
    path: Arc<PathBuf>,
    sync_data: bool,
) -> Result<(), LocalImmutableStoreError> {
    let mut first_err: Option<LocalImmutableStoreError> = None;

    crate::local::fan_out::roll_pending_commit_forward(
        &group.writes,
        &group.committed_level,
        &group.pending_level,
        &path,
        group_index,
        sync_data,
    )
    .await
    .map_err(|e| {
        LocalImmutableStoreError::internal_with_context(
            e,
            "Failed to roll a failed fan-out commit forward",
        )
    })?;

    // Re-check under the lock: another flusher may have drained this group
    // while we waited. The scan that got us here released the lock before
    // this task took it, so it is stale: skip the redundant fan-out check, path selection and - in the
    // two-phase branch - the needless level-marker write. A pending level
    // transition (`committed_level != active_buckets`) still has to be
    // completed even with no dirty bucket, so it is never skipped.
    if !(0..group.dirty.len()).any(|bucket| group.needs_flush(bucket))
        && group.committed_level.load(atomic::Ordering::Relaxed)
            == group.bucket_count.load(atomic::Ordering::Relaxed)
    {
        // The packstore flush below is unconditional for `sync_data`, so it
        // still has to run on this path.
        if sync_data {
            return group.flush_packstore(sync_data).await;
        }
        return Ok(());
    }

    // Fan-out trigger: if any dirty bucket exceeds threshold and we're below max level, redistribute entries before serializing.
    if let Err(err) = maybe_fan_out_immutable_group(&group, path.as_ref(), group_index).await {
        first_err = Some(err);
    }

    let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
    let committed_level = group.committed_level.load(atomic::Ordering::Relaxed);
    let group_path = {
        let mut p = path.as_path().to_path_buf();
        p.push("index");
        crate::local::fan_out::push_group_dir(&mut p, group_index);
        p
    };
    let fan_out_aware = group.serialize_version.load(atomic::Ordering::Relaxed)
        == ImmutableStoreVersion::LazyFanOut as u32;
    let needs_two_phase_commit = fan_out_aware && committed_level != active_buckets;

    // Always flush the packstore once per group when sync_data is set, regardless of which serialize path runs below. A failed sync writes no bucket, so that no index points at payload bytes not synced.
    if sync_data
        && let Err(err) = group.flush_packstore(sync_data).await
        && first_err.is_none()
    {
        first_err = Some(err);
    }

    if needs_two_phase_commit && first_err.is_none() {
        // T10 two-phase commit. Every flagged bucket, and every bucket below committed_level, gets a .new file; a clean bucket at or past committed_level has no file, or one that matches it. A flagged bucket is written empty as well: a bucket write outside a flush, or a layout older than the level markers, may have left it a file holding entries since removed. After all .new files are durable, write level.pending as the commit point. Then rename .new -> final, write the level marker, delete level.pending. Recovery on the next store open rolls forward from any pending state.
        if let Err(e) = lore_io::IoDriver::global()
            .create_dir_all(&group_path)
            .await
            .map_err(|e| {
                LocalImmutableStoreError::internal_with_context(
                    e,
                    "Failed to create group directory for fan-out commit",
                )
            })
        {
            first_err = Some(e);
        }

        let mut commit = crate::local::fan_out::UnfinishedCommit::new(&group.dirty);
        if first_err.is_none() {
            for bucket_index in 0..active_buckets {
                // Fast path: skip the bucket entirely (no lock acquire) when it's neither dirty nor an old-level slot we need to overwrite. The dirty flag is the cheap proxy for "this bucket has data to flush"; combined with the index < committed_level check (which forces an empty .new to overwrite stale level-N files), this avoids 256× read-lock acquires per group on the common server-fresh-store first flush where most buckets are empty and committed_level == 0.
                let must_overwrite_old = bucket_index < committed_level;
                let dirty = group.needs_flush(bucket_index);
                if !must_overwrite_old && !dirty {
                    continue;
                }
                let bucket = group.bucket(bucket_index).clone().read_owned().await;
                let res = ImmutableStoreBucket::serialize_to_new(
                    bucket,
                    group.clone(),
                    path.as_ref(),
                    group_index,
                    bucket_index,
                    sync_data,
                )
                .await;
                match res {
                    Ok(()) => commit.wrote_new.push(bucket_index),
                    Err(err) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                        }
                    }
                }
            }
        }

        if commit.wrote_new.is_empty() {
            // No .new files written for this group — skip the level.pending sentinel entirely. The sentinel exists to drive roll-forward recovery of a partially-completed transition; with no .new files there is no in-progress state to recover, so a direct marker write is sufficient. Restores ~3x throughput on fresh-store-first-flush-with-sync_data when most groups are empty (the common shape on `lore repository create`).
            if first_err.is_none()
                && let Err(err) = crate::local::fan_out::write_level_marker(
                    &group_path,
                    active_buckets,
                    sync_data,
                    Some(&group.writes),
                )
                .await
                .map_err(|e| {
                    LocalImmutableStoreError::internal_with_context(
                        e,
                        "Failed to write level marker for empty group",
                    )
                })
            {
                first_err = Some(err);
            }
            if first_err.is_none() {
                group
                    .committed_level
                    .store(active_buckets, atomic::Ordering::Relaxed);
            }
        } else {
            // Full two-phase commit: pending → renames → marker → delete pending.
            if first_err.is_none()
                && let Err(err) = Box::pin(crate::local::fan_out::remove_stale_files(
                    &group_path,
                    &commit.wrote_new,
                ))
                .await
                .map_err(|e| {
                    LocalImmutableStoreError::internal_with_context(
                        e,
                        "Failed to remove stale files from the group directory",
                    )
                })
            {
                first_err = Some(err);
            }
            if first_err.is_none() {
                match crate::local::fan_out::write_level_pending(
                    &group_path,
                    active_buckets,
                    sync_data,
                    Some(&group.writes),
                )
                .await
                {
                    Ok(()) => group
                        .pending_level
                        .store(active_buckets, atomic::Ordering::Relaxed),
                    Err(e) => {
                        first_err = Some(LocalImmutableStoreError::internal_with_context(
                            e,
                            "Failed to write level.pending",
                        ));
                    }
                }
            }

            if first_err.is_none() {
                for &bucket_index in &commit.wrote_new {
                    let new_path =
                        crate::local::fan_out::bucket_new_path(&group_path, bucket_index);
                    let final_path = crate::local::fan_out::bucket_path(&group_path, bucket_index);
                    if let Err(err) = lore_io::IoDriver::global()
                        .rename(&new_path, &final_path)
                        .await
                        && first_err.is_none()
                    {
                        first_err = Some(LocalImmutableStoreError::internal_with_context(
                            err,
                            "Failed to rename .new bucket file during fan-out commit",
                        ));
                    }
                }
            }

            if first_err.is_none()
                && let Err(err) = crate::local::fan_out::write_level_marker(
                    &group_path,
                    active_buckets,
                    sync_data,
                    Some(&group.writes),
                )
                .await
                .map_err(|e| {
                    LocalImmutableStoreError::internal_with_context(
                        e,
                        "Failed to write level marker",
                    )
                })
            {
                first_err = Some(err);
            }

            if first_err.is_none()
                && let Err(err) = crate::local::fan_out::delete_level_pending(&group_path)
                    .await
                    .map_err(|e| {
                        LocalImmutableStoreError::internal_with_context(
                            e,
                            "Failed to delete level.pending",
                        )
                    })
            {
                first_err = Some(err);
            }

            if first_err.is_none() {
                group
                    .committed_level
                    .store(active_buckets, atomic::Ordering::Relaxed);
                group.pending_level.store(0, atomic::Ordering::Relaxed);
                commit.complete();
            }
        }
    } else if first_err.is_none() {
        // Regular flush at unchanged level: per-file .tmp + atomic rename for flagged buckets only, access stamps included. No marker write — marker already reflects the current level.
        for bucket_index in 0..active_buckets {
            if !group.needs_flush(bucket_index) {
                continue;
            }
            let Some(bucket) = group.try_bucket(bucket_index).cloned() else {
                continue;
            };
            let bucket = bucket.read_owned().await;
            let res = ImmutableStoreBucket::serialize(
                bucket,
                group.clone(),
                path.as_ref(),
                group_index,
                bucket_index,
                sync_data,
            )
            .await;
            if let Err(err) = res
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
    }

    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Inspect dirty buckets in `group` and, if any exceeds the per-store fan-out threshold and the
/// group is not yet at max level, atomically redistribute entries to the next ladder level. Same
/// shape as `maybe_fan_out_mutable_group` but operating on `ImmutableStoreEntry` (which references
/// pack data; pack references survive the move untouched since the pack layout is per-group).
async fn maybe_fan_out_immutable_group(
    group: &Arc<ImmutableStoreGroup>,
    path: &Path,
    group_index: usize,
) -> Result<(), LocalImmutableStoreError> {
    let n = group.bucket_count.load(atomic::Ordering::Relaxed);
    if n >= crate::local::fan_out::FAN_OUT_LEVEL_MAX {
        return Ok(());
    }
    let mut b_max = 0usize;
    for bucket_index in 0..n {
        if !group.dirty[bucket_index].load(atomic::Ordering::Relaxed) {
            continue;
        }
        let bucket = group.bucket(bucket_index).read().await;
        b_max = b_max.max(bucket.entry.len());
    }
    if b_max <= group.fan_out_threshold {
        return Ok(());
    }
    let target = crate::local::fan_out::level_for(n, b_max, group.fan_out_threshold);
    if target <= n {
        return Ok(());
    }

    let mut guards: Vec<OwnedRwLockWriteGuard<ImmutableStoreBucket>> = Vec::with_capacity(target);
    for i in 0..target {
        guards.push(group.bucket(i).clone().write_owned().await);
    }

    // Force-deserialize any [0..n] bucket whose entries are still on disk only. Without this, on-disk-only buckets contribute zero entries to the redistribute and their data is lost when serialize overwrites their files with empty buckets at the new layout.
    for (bucket_index, guard) in guards.iter_mut().take(n).enumerate() {
        if !guard.deserialized {
            Box::pin(guard.deserialize(group, path, group_index, bucket_index, None)).await?;
        }
    }

    let mut entries_per_new_bucket: Vec<Vec<ImmutableStoreEntry>> =
        (0..target).map(|_| Vec::new()).collect();
    for guard in guards.iter_mut().take(n) {
        let old = std::mem::take(&mut guard.entry);
        for entry in old.iter() {
            let new_idx = crate::local::fan_out::bucket_index_for(&entry.address.hash, target);
            entries_per_new_bucket[new_idx].push(*entry);
        }
        guard.sorted_index = lore_base::allocator::GrowVec::new();
    }

    for (new_idx, entries) in entries_per_new_bucket.into_iter().enumerate() {
        let count = entries.len();
        let bucket = &mut guards[new_idx];
        bucket.entry = lore_base::allocator::GrowVec::new();
        bucket.sorted_index = lore_base::allocator::GrowVec::new();
        for entry in entries {
            let (_match_slot, insert_slot, _match_made) = LocalImmutableStore::lookup(
                bucket,
                entry.partition,
                entry.address,
                StoreMatch::MatchFull,
            );
            let entry_index = bucket.entry.len();
            bucket.sorted_index.insert(insert_slot, entry_index as u32);
            bucket.entry.push(entry);
        }
        // The redistribute leaves every `[0..target]` bucket holding exactly the entries it
        // should, while the layout on disk is still the pre-fan-out one until the flush commits.
        // A lazy deserialize of any of them would therefore replace live entries with a stale
        // file, or with nothing for a slot the old layout never wrote.
        bucket.deserialized = true;
        if count > 0 {
            group.dirty[new_idx].store(true, atomic::Ordering::Relaxed);
        }
    }

    group.bucket_count.store(target, atomic::Ordering::Relaxed);
    drop(guards);
    Ok(())
}

/// Create a local immutable store.
///
/// Background eviction/compaction tasks are NOT spawned here — the store itself
/// is unaware of any GC event sink. Spawn them separately with
/// [`crate::maintenance::spawn_gc`], passing the GC config and an optional
/// [`crate::gc_event::GcEventSink`] to receive progress. `options` is accepted
/// for call-site compatibility and is consumed by `spawn_gc`, not here.
pub fn create(
    path: Option<impl AsRef<Path>>,
    options: ImmutableStoreCreateOptions,
    deserialize_buckets: bool,
    settings: ImmutableStoreSettings,
) -> impl Future<Output = Result<Arc<dyn crate::immutable_store::ImmutableStore>, StoreError>> {
    ReleasingOpeningClaim(create_claimed(path, options, deserialize_buckets, settings))
}

/// Creates a local immutable store as [`create`] does, and returns with it the claim it was
/// opened under, as [`LocalImmutableStore::new_claimed`] describes. The bucket load
/// `deserialize_buckets` asks for runs under that claim.
///
/// # Errors
///
/// [`StoreError`] if the store cannot be opened.
pub async fn create_claimed(
    path: Option<impl AsRef<Path>>,
    options: ImmutableStoreCreateOptions,
    deserialize_buckets: bool,
    settings: ImmutableStoreSettings,
) -> Result<
    (
        Arc<dyn crate::immutable_store::ImmutableStore>,
        Option<StoreGuard>,
    ),
    StoreError,
> {
    let path = path.as_ref();
    let (store, opening) =
        LocalImmutableStore::new_claimed(path.map(|path| path.as_ref().to_path_buf()), settings)
            .await
            .forward::<StoreError>("Failed to create data store for repository.")?;

    // Set before the bucket load below so a `deserialize_all` over-cap store can trigger.
    store.set_gc_caps(
        options.max_size.unwrap_or(0),
        options.max_capacity.unwrap_or(0),
        false,
    );

    if deserialize_buckets {
        let _ = store.deserialize_all_buckets().await;
    }

    Ok((store, opening))
}
