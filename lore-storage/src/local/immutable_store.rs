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
use std::backtrace::Backtrace;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
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
use crate::immutable_store::StoreError;
use crate::immutable_store::sanitise_fragment_behavior_flags;
use crate::local::fan_out::GroupLevel;
use crate::local::immutable_store::info::ImmutableStoreInfo;
use crate::local::immutable_store::info::get_or_init_disk_info;
use crate::local::immutable_store::info::info_path_for_store_root;
use crate::local::immutable_store::info::read_info_file;
use crate::local::immutable_store::info::write_info_file;
use crate::local::store_lock::Intent;
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
}

const VALIDATE_COMPACTION: bool = false;

pub const GROUP_COUNT: usize = 256;
pub const BUCKET_COUNT: usize = 256;

pub const DEFAULT_FLUSH_DELAY_SECONDS: u64 = 5;

const DOT_COMPACT: &str = "compact";

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
            | (self.flags & FragmentFlags::PayloadStored);
        if self.pack_file != 0 {
            self.flags |= FragmentFlags::PayloadStoredLocal.bits();
        } else {
            self.flags &= !FragmentFlags::PayloadStoredLocal.bits();
        }
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
    /// reaches disk without announcing a change — and `LocalImmutableStore::note_flushed`
    /// ignores it, so stamps alone never hold the flock and never keep a store claimed
    /// after the work is done.
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
    /// Forces the bucket-file writes for this group to be serial.
    ///
    /// `flush_all` holds it for a whole group flush so that the fan-out check, the
    /// `committed_level` read that selects the commit path, and the writes are one
    /// atomic unit. Every other writer (the delayed flush, the evictor, the
    /// compactor, the packfile upgrade) takes it only around its own bucket write.
    ///
    /// Without it, an overlapping flush can observe a half-finished level transition
    /// and take the regular in-place path while a two-phase commit is still pending.
    /// The commit's later `rename` of `index_<bb>.new` then publishes its older
    /// snapshot over the newer in-place write, discarding it. The losing write still
    /// returns `Ok`, and the clobbered file inherits the `.new` file's older mtime.
    /// Locking the rename alone would not help: the snapshot it publishes is taken
    /// before the rename, so the two paths must not interleave at all.
    ///
    /// Scope is deliberately narrow outside `flush_all` because
    /// `compact_group_packfiles` calls `evict_group_sized`, and a `tokio::sync::Mutex`
    /// is not reentrant - a per-function guard would self-deadlock.
    pub flush_lock: Arc<Mutex<()>>,
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
    /// flags are cleared for every slot, a pass over `BUCKET_COUNT` atomics.
    ///
    /// Does no I/O and cannot fail. The packstore is only *forgotten*; `resume()`
    /// re-reads it on next use, as it does for a store that has just been opened.
    ///
    /// # Requirements
    ///
    /// The caller must hold the store's flock with nothing else in flight — that is,
    /// on the transition into the first operation. Concurrent readers would otherwise
    /// see buckets emptied under them.
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

#[cfg(feature = "failure_generator")]
pub struct LocalImmutableStoreFailureGenerator {
    retry_rate: f32,
    miss_fragment_writes: HashSet<Hash>,
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
    /// a store kept only by the keep-alive cache holds none. See
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
    /// A read holds a read claim, which never marks the store dirty, so a stamp that
    /// counted as a change would leave a bucket dirty with nothing recording that the
    /// store has unflushed state. The flock would then be released over it and a later
    /// reload would drop it — or, if a flush had already passed that bucket,
    /// `note_flushed` would refuse to clear and a read-heavy process would hold the
    /// store against every other one for as long as it lived. So the stamp sets the
    /// bucket's soft flag, which a flush writes under a read claim.
    Read,
    /// The caller is writing and the lookup is part of that write.
    ///
    /// A write already holds a write claim and has already advanced the epoch, so
    /// keeping the stamp costs nothing — and a `put` of content the store already has
    /// is still a touch of that content, which is exactly what the ranking is for.
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
#[derive(Default, IntoBytes, FromBytes, Immutable)]
struct ImmutableStoreHeader {
    version: u32,
    _unused: u32,
    count: u32,
    _unused_two: u32,
    // Following the index store is
    // Sorted index of entries
    // sorted_index: [u32; count]
    // All entries
    // entry[IndexStoreEntry; count]
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
            if !name_str.starts_with(crate::local::fan_out::BUCKET_FILENAME_PREFIX)
                || name_str.ends_with(crate::local::fan_out::BUCKET_NEW_SUFFIX)
            {
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

/// Gather segments for a bucket write: the serialized header plus the
/// bucket's sorted-index and entry chunks, written with one vectored
/// operation and no staging copy. Owning the bucket's read guard keeps the
/// chunk memory alive and unmodified for the operation's whole kernel
/// flight, which is the stability contract vectored writes require.
struct ImmutableBucketWriteSegments {
    /// The serialized header, in a fixed-size allocation rather than a `Vec`: its length is a
    /// compile-time constant. It stays behind a pointer because [`lore_io::StableBufList`]
    /// requires a segment to keep its address when the value moves, and the ring backend moves
    /// the segment list into its operation entry after taking the pointers.
    header: Box<[u8; size_of::<ImmutableStoreHeader>()]>,
    bucket: OwnedRwLockReadGuard<ImmutableStoreBucket, ImmutableStoreBucket>,
}

impl lore_io::StableBufList for ImmutableBucketWriteSegments {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.header.as_ref().as_slice())
            .chain(self.bucket.sorted_index.byte_segments())
            .chain(self.bucket.entry.byte_segments())
    }
}

/// Re-arms a bucket's flag, and removes a synced write's staging file, when a serialize
/// ends without [`Self::success`].
///
/// The flag is cleared before the write begins, so a failed write that left it clear would
/// never be retried, and a flush scan would find the store clean and release its flock over
/// state that is only in memory.
pub struct SerializeFailureGuard<'a> {
    success: bool,
    dirty: &'a AtomicBool,
    /// The staging file a synced write goes through, or `None` for a write straight to the
    /// bucket's own file.
    path: Option<&'a Path>,
}

impl<'a> SerializeFailureGuard<'a> {
    pub fn new(dirty: &'a AtomicBool, path: Option<&'a Path>) -> Self {
        Self {
            success: false,
            dirty,
            path,
        }
    }

    pub fn success(&mut self) {
        self.success = true;
    }
}

impl<'a> Drop for SerializeFailureGuard<'a> {
    fn drop(&mut self) {
        if !self.success {
            // Important to reset flag while lock is still held
            self.dirty.store(true, atomic::Ordering::Relaxed);

            if let Some(path) = self.path
                && let Err(err) = std::fs::remove_file(path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                lore_base::lore_warn!(
                    "Failed to remove temporary file {}: {err:?}",
                    path.display()
                );
            }
        }
    }
}

impl ImmutableStoreBucket {
    #[lore_macro::test_pub]
    async fn deserialize_files(
        path: PathBuf,
    ) -> Result<
        (
            GrowVec<u32, CHUNK_SIZE_U32>,
            GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
            bool,
            bool,
        ),
        LocalImmutableStoreError,
    > {
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
                    return Ok((GrowVec::new(), GrowVec::new(), false, false));
                }
                return Err(LocalImmutableStoreError::internal_with_context(
                    err,
                    "Failed to deserialize storage bucket",
                ));
            }
        };

        // Recover from corruption (crash mid-flush leaves a half-written file) by
        // resetting the bucket. Future-version sentinels propagate untouched — the file
        // was written by a newer binary and deleting it would destroy newer-format data.
        match Self::deserialize_file_content(&path, &file, metadata.len() as usize, &head).await {
            Ok(result) => Ok(result),
            Err(DeserializeFileError::FutureVersion(version)) => {
                Err(LocalImmutableStoreError::internal_with_context(
                    io::Error::other(format!(
                        "Incompatible store version {version} encountered, please update your client to the latest version"
                    )),
                    "Failed to deserialize storage bucket",
                ))
            }
            Err(DeserializeFileError::Corrupt(reason)) => {
                Self::recover_corrupt_bucket(&path, reason).await
            }
        }
    }

    /// Buckets that fit inside the composite open's head bytes — the
    /// common case — parse straight from the head with no further
    /// dispatch. Larger buckets do one vectored read scattering the
    /// sorted index and entries straight into their final chunk
    /// allocations. Legacy layouts (before `LastAccessInEntry`) parse the
    /// remainder with entry conversion.
    async fn deserialize_file_content(
        path: &Path,
        file: &lore_io::IoFile,
        file_size: usize,
        head: &[u8],
    ) -> Result<
        (
            GrowVec<u32, CHUNK_SIZE_U32>,
            GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
            bool,
            bool,
        ),
        DeserializeFileError,
    > {
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
            return Ok((GrowVec::new(), GrowVec::new(), false, false));
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
            if file_size <= head.len() {
                let mut reader = &head[header_size..];
                let sorted_index =
                    GrowVec::read_from(&mut reader, expected_count).map_err(|err| {
                        DeserializeFileError::Corrupt(format!("read sorted index: {err}"))
                    })?;
                let entry = GrowVec::read_from(&mut reader, expected_count)
                    .map_err(|err| DeserializeFileError::Corrupt(format!("read entries: {err}")))?;
                return Ok((sorted_index, entry, upgrade_packfile, false));
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
                .map_err(|err| DeserializeFileError::Corrupt(format!("read bucket data: {err}")))?;
            return Ok((
                segments.sorted_index,
                segments.entry,
                upgrade_packfile,
                false,
            ));
        }

        let remainder;
        let mut reader: &[u8] = if file_size <= head.len() {
            &head[header_size..]
        } else {
            remainder = file
                .read_exact_at(file_size - header_size, header_size as u64)
                .await
                .map_err(|err| DeserializeFileError::Corrupt(format!("read bucket data: {err}")))?;
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

        Ok((sorted_index, entry, upgrade_packfile, true))
    }

    /// Drop the corrupt file and return an empty bucket. Pack file payloads survive
    /// until compaction reclaims the now-orphaned ranges.
    async fn recover_corrupt_bucket(
        path: &Path,
        reason: String,
    ) -> Result<
        (
            GrowVec<u32, CHUNK_SIZE_U32>,
            GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
            bool,
            bool,
        ),
        LocalImmutableStoreError,
    > {
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
        Ok((GrowVec::new(), GrowVec::new(), false, false))
    }

    async fn serialize_files(
        bucket: OwnedRwLockReadGuard<ImmutableStoreBucket, ImmutableStoreBucket>,
        group: Arc<ImmutableStoreGroup>,
        bucket_index: usize,
        path: PathBuf,
        changed: bool,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        // Append `.tmp` rather than replacing the extension, so a fan-out-commit path like `index_<bb>.new` becomes `index_<bb>.new.tmp`. set_extension would clobber `.new` to `.tmp`, colliding with the regular flush path's tmp file.
        let temporary_path = sync_data.then(|| {
            let mut p = path.as_os_str().to_owned();
            p.push(".tmp");
            PathBuf::from(p)
        });
        // A failed write re-arms the flag it claimed, synced or not — the flag was cleared
        // before the write either way — so a bucket flushed only for its access stamps
        // stays a stamp rather than becoming a change that keeps the store claimed.
        let rearm = if changed {
            &group.dirty[bucket_index]
        } else {
            &group.soft_dirty[bucket_index]
        };
        let mut failure_guard = SerializeFailureGuard::new(rearm, temporary_path.as_deref());

        if let Some(parent_path) = path.parent()
            && !parent_path.exists()
        {
            let _ = lore_io::IoDriver::global()
                .create_dir_all(parent_path)
                .await;
        }

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

        let segments = ImmutableBucketWriteSegments {
            header: {
                let mut bytes = Box::new([0u8; size_of::<ImmutableStoreHeader>()]);
                bytes.copy_from_slice(header.as_bytes());
                bytes
            },
            bucket,
        };

        let file_options = lore_io::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true);
        if let Some(temporary_path) = &temporary_path {
            lore_io::IoDriver::global()
                .write_file_segments_atomic(temporary_path, &path, &file_options, segments)
                .await
                .internal("Failed to serialize storage bucket")?;
        } else {
            lore_io::IoDriver::global()
                .write_file_segments(&path, &file_options, segments, false)
                .await
                .internal("Failed to serialize storage bucket")?;
        }
        failure_guard.success();

        Ok(())
    }

    async fn deserialize(
        &mut self,
        dirty: &AtomicBool,
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

        let (sorted_index, entry, upgrade_packfile, mark_dirty) =
            Self::deserialize_files(path).await?;

        self.sorted_index = sorted_index;
        self.entry = entry;
        self.upgrade_packfile = upgrade_packfile;
        self.deserialized = true;

        if let Some(gc) = gc_counters {
            gc.add_loaded_fragments(self.entry.len());
        }

        if mark_dirty {
            dirty.store(true, atomic::Ordering::Relaxed);
        }

        atomic::fence(atomic::Ordering::Release);

        Ok(())
    }

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

impl ImmutableStoreGroup {
    async fn flush_packstore(&self, sync_data: bool) {
        self.packstore.flush_all(sync_data).await;
    }

    #[lore_macro::test_pub]
    async fn flush_delayed(weak_ref: Weak<LocalImmutableStore>, group_index: usize, delay: u64) {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        if let Some(store) = weak_ref.upgrade()
            && let Some(path) = store.path.as_ref()
        {
            let group = store.group[group_index].clone();
            // Cleared before a single flag is read: a bucket flagged from here on schedules
            // the next sweep, and one flagged before this is seen by this one.
            group.scheduled.store(false, atomic::Ordering::Relaxed);
            // **Claimed before anything is written, and only as far as the work needs.**
            // This wakes long after the operation that scheduled it, by which time the
            // claim that operation held may be gone — the flock is released once
            // nothing has the store open and nothing is dirty, and an explicit flush in
            // between does exactly that. Writing then would put bucket files and a level
            // marker on disk with no claim and no epoch advance, over whatever another
            // process has since written, and tell nobody.
            //
            // Looked at before claiming, because a sweep that finds nothing — the usual
            // case once a command's own flush has run — must not claim to write: that
            // advances the epoch and tells every other process its state is worthless.
            // The flags are read again under the claim below, and a claim that finds
            // the store stale reloads first, which empties the buckets and leaves
            // nothing to write. That is the right answer: state already invalidated must
            // not reach disk.
            let level_pending = group.committed_level.load(atomic::Ordering::Relaxed) == 0
                && group.bucket_count.load(atomic::Ordering::Relaxed)
                    != crate::local::fan_out::FAN_OUT_LEVEL_MAX;
            let intent = match (level_pending, group.pending_flush()) {
                (true, _) | (false, PendingFlush::Changes) => Intent::Write,
                (false, PendingFlush::Stamps) => Intent::Read,
                (false, PendingFlush::Nothing) => {
                    store.note_flushed();
                    return;
                }
            };
            let Ok(claim) = store.hold(intent).await else {
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
                let _flush_guard = group.flush_lock.clone().lock_owned().await;

                // Re-check under the lock: a flush that ran while we waited may already
                // have written this bucket. The read above happened before the lock, so
                // it is stale.
                if !group.needs_flush(bucket_index) {
                    continue;
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

            let flush_guard = group.flush_lock.clone().lock_owned().await;
            crate::local::fan_out::commit_if_initial_level(
                &flush_guard,
                &group.committed_level,
                &group.bucket_count,
                path,
                group_index,
                false,
            )
            .await;
            // Dropped before declaring the store clean, for the reason `flush` gives.
            drop(_claim);
            store.note_flushed();
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
        let Some(lock) = self.lock.as_ref() else {
            return;
        };
        // Taken before scanning, so a write that ends while the scan runs refuses the
        // clear — and a store with nothing to clear is not scanned at all.
        let Some(scan) = lock.clean_scan_start() else {
            return;
        };
        let dirty = self.group.iter().any(|group| {
            group
                .dirty
                .iter()
                .any(|bucket| bucket.load(atomic::Ordering::Relaxed))
        });
        if !dirty {
            lock.clear_dirty(scan);
        }
    }

    /// Claims this store's cross-process lock for an operation, reloading first if
    /// another process changed the store while this one held nothing.
    ///
    /// Joins the span claim when one is held, which is the common case. Methods that
    /// touch the store's files hold a guard from this for as long as that work runs,
    /// including work handed to other tasks. `None` is not a failure: an in-memory
    /// store has no flock and nothing on disk for anyone to change.
    ///
    /// # Errors
    ///
    /// [`crate::immutable_store::StoreError`] if the lock cannot be taken, or if the
    /// reload a stale acquisition demands cannot read the store.
    pub async fn hold(
        &self,
        intent: Intent,
    ) -> Result<Option<StoreGuard>, crate::immutable_store::StoreError> {
        let Some(lock) = self.lock.as_ref() else {
            return Ok(None);
        };
        let guard = lock.acquire(intent).await.map_err(|err| {
            crate::immutable_store::StoreError::internal(format!(
                "failed to acquire store lock: {err}"
            ))
        })?;
        self.settle_claim(lock, guard, intent).await.map(Some)
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
        let Some(guard) = lock.try_acquire(intent).await.map_err(|err| {
            crate::immutable_store::StoreError::internal(format!(
                "failed to acquire store lock: {err}"
            ))
        })?
        else {
            return Ok(BackgroundClaim::Busy);
        };
        self.settle_claim(lock, guard, intent)
            .await
            .map(BackgroundClaim::Held)
    }

    /// Reloads the store if `guard` reports it stale, then marks a write claim dirty.
    ///
    /// # Errors
    ///
    /// [`crate::immutable_store::StoreError`] if the reload cannot read the store.
    async fn settle_claim(
        &self,
        lock: &StoreLock,
        mut guard: StoreGuard,
        intent: Intent,
    ) -> Result<StoreGuard, crate::immutable_store::StoreError> {
        if guard.is_stale() {
            // Boxed: a reload is rare, and carried inline its state would be part of every
            // operation's future.
            Box::pin(self.refresh()).await.map_err(|err| {
                crate::immutable_store::StoreError::internal(format!(
                    "failed to reload a store another process changed: {err}"
                ))
            })?;
            // Only now is the epoch this store's own. Returning early above leaves it
            // behind, so the next acquisition reports stale and retries the reload —
            // and until then no other operation joins, because a stale guard holds
            // the directory's gate.
            guard.note_refreshed();
        }
        if intent == Intent::Write {
            // Conservative on purpose: a write hold marks the store as possibly
            // modified before the caller writes, so the flock outlives the
            // operation and no other process can read a store this one has
            // changed and not yet flushed. A flush that leaves nothing dirty
            // clears it again.
            //
            // Reached only once the store is loaded, which is why it sits below the
            // refresh. `dirty` is what stops `release_if_idle` from dropping the
            // flock and only a flush clears it, so it must be set solely for a store
            // that has state worth protecting: a store that could not be read holds
            // none, and marking it would pin the flock against every other process
            // with nothing left to come and release it.
            lock.mark_dirty();
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
        let index_existed_on_disk = immutable_path.is_some_and(|p| p.join("index").exists());
        // Every group is read at once. Each group is one recovery check and one marker read, and
        // awaiting them in turn puts a whole store open behind `GROUP_COUNT` round trips to the
        // I/O engine — a cost every process that opens a store pays before it does any work. The
        // groups are independent directories, so the reads overlap and the engine's thread budget
        // is what paces them. Completions arrive in whatever order the reads finish, so each task
        // carries the group it answers for.
        let mut group_levels = vec![GroupLevel::Unwritten; GROUP_COUNT];
        if let Some(path) = immutable_path {
            let index_path = path.as_path().join("index");
            let mut tasks = JoinSet::new();
            for group_index in 0..GROUP_COUNT {
                let group_path = crate::local::fan_out::group_dir_path(&index_path, group_index);
                lore_base::lore_spawn!(tasks, async move {
                    if !group_path.exists() {
                        return (group_index, Ok(GroupLevel::Unwritten));
                    }
                    // Roll forward any pending fan-out commit before reading the marker. After this returns the marker reflects the post-recovery state.
                    if let Err(err) =
                        crate::local::fan_out::recover_level_transition(&group_path, false).await
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

        // Determine serialize_version per Decision 8. Fresh stores, stores with markers, and
        // existing stores with bucket files at any older version (v1-v3) all go to LazyFanOut.
        // Existing stores at the current pre-fan-out version (v4 LastAccessInEntry) with no
        // markers stay at v4 for backward compatibility (old binaries can still read them).
        let any_older_bucket_seen = if index_existed_on_disk
            && !any_marker_seen
            && let Some(path) = immutable_path
        {
            let mut idx = path.as_path().to_path_buf();
            idx.push("index");
            detect_any_older_immutable_bucket(&idx)
        } else {
            false
        };
        let serialize_version: u32 =
            if !index_existed_on_disk || any_marker_seen || any_older_bucket_seen {
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
                flush_lock: Arc::new(Mutex::new(())),
                packstore: crate::PackStore::new(
                    packpath,
                    MIN_PACKFILE_COUNT,
                    Some(gc_counters.clone()),
                ),
                flush: Mutex::new(JoinSet::new()),
                scheduled: AtomicBool::new(false),
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

    pub async fn new(
        path: Option<PathBuf>,
        settings: ImmutableStoreSettings,
    ) -> Result<Arc<Self>, LocalImmutableStoreError> {
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
            miss_fragment_writes: std::env::var("LORE_MISS_FRAGMENT_WRITES")
                .map(|val| {
                    val.split(",")
                        .filter_map(|hash| Hash::from_str(hash.trim()).ok())
                        .collect()
                })
                .unwrap_or_default(),
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
                StoreLock::new(path.as_path())
                    .internal("Failed to open the store's lock directory")?,
            )
        } else {
            None
        };
        // Held across the survey and the info read below, and released once both are
        // done: they read from disk, so they have to read under the flock, and the
        // epoch observed here is what a later acquisition compares against.
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
        drop(opening);
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

        // The `Arc` only exists now (not when the groups were built above), so back-fill
        // the weak self-ref the load hooks need to fire a pass.
        let dyn_store: Arc<dyn crate::immutable_store::ImmutableStore> = store.clone();
        store.gc_counters.set_store(&dyn_store);
        if let Some(path) = immutable_path.as_deref() {
            let mut old_packpath = path.clone();
            old_packpath.push("pack");
            if let Ok(metadata) = std::fs::metadata(old_packpath.as_path())
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
                if std::fs::metadata(old_packpath.as_path()).is_ok_and(|metadata| metadata.is_dir())
                {
                    store
                        .clone()
                        .upgrade_global_packfiles(path, old_packpath.as_path())
                        .await?;
                }
                // Released before declaring the store clean, for the reason `flush`
                // gives, so a store that has just been opened is not left claimed by a
                // one-time path.
                drop(_claim);
                store.note_flushed();
            }
        }

        Ok(store)
    }

    pub async fn run_migrations(self: &Arc<Self>) -> Result<(), LocalImmutableStoreError> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let migrated = self.run_migrations_held(path).await;
        // Released before declaring the store clean, for the reason `flush` gives, and on
        // every outcome: a write claim marked the store dirty as it was taken, so a
        // migration that returned early without this would keep the store claimed.
        self.note_flushed();
        migrated
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
        let claim = self.hold(Intent::Write).await.map_err(|err| {
            LocalImmutableStoreError::internal_with_context(
                err,
                "Failed to claim the store to write its info file",
            )
        })?;
        let written = self.write_store_info(accessor).await;
        // Released before declaring the store clean, for the reason `flush` gives. The
        // file is written in full before this returns, so nothing of it is left to flush.
        drop(claim);
        self.note_flushed();
        written
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

                    let _flush_guard = group.flush_lock.clone().lock_owned().await;
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
                final_result = final_result.and(
                    group
                        .packstore
                        .resume()
                        .await
                        .forward("Failed to deserialize storage bucket"),
                );
                if final_result.is_err() {
                    break;
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
                                    &group.dirty[bucket_index],
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

        self.deserialized_all.store(true, atomic::Ordering::Relaxed);

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

    // Assumes that payload has been validated to match the given hash prior to
    // calling this function to store the content payload - no hash validation done
    pub async fn store(
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

        let group = &self.group[group_index];
        let (bucket_index, mut bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&address.hash, n);
            let lock = group.bucket(idx).clone().write_owned().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) == n {
                break (idx, lock);
            }
            drop(lock);
        };

        if !bucket.deserialized && self.path.is_some() {
            Box::pin(bucket.deserialize(
                &group.dirty[bucket_index],
                self.path.clone().unwrap().as_ref(),
                group_index,
                bucket_index,
                Some(&self.gc_counters),
            ))
            .await?;
        }

        let (match_slot, insert_slot, match_made) =
            Self::lookup(&bucket, partition, address, StoreMatch::MatchFull);

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
                if fragment_flags != data.flags {
                    // Update stored upstream/local flags
                    if (fragment_flags & FragmentFlags::PayloadStored)
                        != (current_flags & FragmentFlags::PayloadStored)
                    {
                        let previous_flags = current_flags;
                        {
                            let entry = &mut bucket.entry[entry_index];
                            entry.data.flags &= !FragmentFlags::PayloadStored;
                            entry.data.flags |= fragment_flags & FragmentFlags::PayloadStored;
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
                }
                // If we have a previous payload or if no payload was given there is
                // no update to do and we can early out - unless force write payload
                if (!force && data.pack_file != 0) || payload.is_none() {
                    return Ok(());
                }
            }

            // Deduplicate payload if it already exist and not forced overwrite
            if !force || payload.is_none() {
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
        // If there was no existing data and payload given, then store the data
        if !matched_partition || (pack_file == 0 && payload.is_some()) {
            if let Some(payload) = payload {
                if pack_file == 0 {
                    if payload.len() < fragment.size_payload as usize {
                        lore_base::lore_error!(
                            "Failed storing immutable data, payload length {} does not match fragment payload size {} for {}",
                            payload.len(),
                            fragment.size_payload,
                            address
                        );
                        return Err(LocalImmutableStoreError::internal("Invalid payload"));
                    }

                    let packref = group
                        .packstore
                        .store(payload.slice(..fragment.size_payload as usize))
                        .await
                        .forward::<LocalImmutableStoreError>(
                            "Failed storing immutable data, packstore write failed",
                        )?;
                    pack_file = packref.id;
                    pack_offset = packref.offset;
                }
            } else {
                lore_base::lore_trace!("Storing partial fragment {address}");
            }
        }

        let last_access = Self::last_access();

        let data = ImmutableData {
            flags: fragment_flags,
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
                address,
                partition,
                data,
            });
        }

        // Ensure all other instances of this hash has the same payload associated - if storing
        // a payload for an already existing hash that had no payload we should upgrade those
        if data.pack_file != 0 {
            let last_slot = bucket.sorted_index.len() - 1;

            let mut update_slot = |slot| {
                let entry_index = bucket.sorted_index[slot] as usize;
                if bucket.entry[entry_index].address.hash != address.hash {
                    return false;
                }
                let entry = &mut bucket.entry[entry_index];
                if entry.data.pack_file != data.pack_file
                    || entry.data.pack_offset != data.pack_offset
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
        drop(bucket);

        {
            let stored_durable = fragment_flags & FragmentFlags::PayloadStoredDurable
                == FragmentFlags::PayloadStoredDurable;
            // Through the group's flag, set after the bucket's, so a sweep that has already
            // cleared it sees the bucket — see [`ImmutableStoreGroup::scheduled`].
            if (!stored_durable || self.settings.flush_background)
                && !group.scheduled.swap(true, atomic::Ordering::Relaxed)
            {
                let mut flush = group.flush.lock().await;
                // Clears a finished sweep, which the set would otherwise keep.
                let _ = flush.try_join_next();
                let weak_self = Arc::downgrade(&self);
                lore_base::lore_spawn!(
                    flush,
                    ImmutableStoreGroup::flush_delayed(
                        weak_self,
                        group_index,
                        self.settings.flush_delay_seconds,
                    )
                );
            }
        }

        if self.settings.verify_write
            && pack_file != 0
            && fragment_flags & FragmentFlags::PayloadObliterated == 0
        {
            let find = self
                .clone()
                .find(partition, address, Lookup::Write)
                .await
                .inspect_err(|err| {
                    lore_base::lore_warn!(
                        "Store write verify failed: {err}\n{}",
                        Backtrace::force_capture()
                    );
                })
                .forward::<LocalImmutableStoreError>("Failed to verify written data")?;

            let data = Self::load(&self.group[find.group].packstore, find.data)
                .await
                .inspect_err(|err| {
                    lore_base::lore_warn!(
                        "Store write verify failed: {err}\n{}",
                        Backtrace::force_capture()
                    );
                })
                .forward::<LocalImmutableStoreError>("Failed to verify written data")?;

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
        }

        Ok(())
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
                let dirty = &group.dirty[idx];
                let gc_counters = self.gc_counters.clone();
                Box::pin(async move {
                    let mut bucket = bucket_ref_for_write.write_owned().await;
                    if group_for_check.bucket_count.load(atomic::Ordering::Relaxed) != n
                        || bucket.deserialized
                    {
                        return Ok::<_, LocalImmutableStoreError>(());
                    }
                    bucket
                        .deserialize(dirty, path.as_ref(), group_index, idx, Some(&gc_counters))
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
        let (bucket_index, mut bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&address.hash, n);
            let lock = group.bucket(idx).clone().write_owned().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) == n {
                break (idx, lock);
            }
            drop(lock);
        };

        if !bucket.deserialized && self.path.is_some() {
            Box::pin(bucket.deserialize(
                &group.dirty[bucket_index],
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
            return Ok(());
        }

        if is_last_fragment && entry.data.pack_file != 0 {
            lore_base::lore_debug!(
                "Fragment payload has no other references, obliterating from packstore"
            );

            stats.num_payloads.fetch_add(1, atomic::Ordering::Relaxed);

            group
                .packstore
                .obliterate(
                    entry.data.pack_file,
                    entry.data.pack_offset,
                    entry.data.size_payload,
                )
                .await
                .forward::<StoreError>("Failed to obliterate payload from pack store.")?;
        }

        stats.num_fragments.fetch_add(1, atomic::Ordering::Relaxed);

        bucket.entry[index].data = ImmutableData {
            flags: FragmentFlags::PayloadObliterated.bits(),
            size_payload: 0,
            size_content: 0,
            pack_file: 0,
            pack_offset: 0,
            last_access: 0,
        };

        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        drop(bucket);

        // Scheduled through the group's flag, set after the bucket's, as `put` does.
        if !group.scheduled.swap(true, atomic::Ordering::Relaxed) {
            let mut flush = group.flush.lock().await;
            // Clears a finished sweep, which the set would otherwise keep.
            let _ = flush.try_join_next();
            let weak_self = Arc::downgrade(&self);
            lore_base::lore_spawn!(
                flush,
                ImmutableStoreGroup::flush_delayed(
                    weak_self,
                    group_index,
                    self.settings.flush_delay_seconds,
                )
            );
        }

        Ok(())
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

    async fn evict_group_sized(
        self: Arc<Self>,
        group_index: usize,
        target_size: usize,
        path: Option<Arc<PathBuf>>,
        protect_local_fragment: bool,
        sync_data: bool,
    ) -> (usize, usize) {
        let mut total_stored_count = 0;
        let mut total_stored_size = 0usize;

        let group = &self.group[group_index];
        let bucket_count = group.bucket_count.load(atomic::Ordering::Relaxed);
        let mut bucket_stored_size = Vec::with_capacity(bucket_count);
        for bucket_index in 0..bucket_count {
            if self.gc_stop_requested() {
                return (0, 0);
            }
            // Uninit slot is empty: push 0 to keep `bucket_stored_size` indexed by
            // bucket_index for the second pass below.
            let Some(bucket_ref) = group.try_bucket(bucket_index) else {
                bucket_stored_size.push(0);
                continue;
            };
            let bucket = bucket_ref.read().await;
            let bucket_size = bucket.entry.len();

            let mut payloads = HashSet::with_capacity(bucket_size / 4);
            let mut stored_size = 0;
            let mut stored_count = 0;
            for index in 0..bucket_size {
                let entry = &bucket.entry[index];
                if entry.data.pack_file == 0 {
                    continue;
                }

                if protect_local_fragment
                    && entry.data.flags & FragmentFlags::PayloadStoredDurable == 0
                {
                    continue;
                }

                let key = (entry.data.pack_file, entry.data.pack_offset);
                if !payloads.contains(&key) {
                    stored_size += entry.data.size_payload as usize;
                    stored_count += 1;

                    payloads.insert(key);
                }
            }

            bucket_stored_size.push(stored_size);
            total_stored_size += stored_size;
            total_stored_count += stored_count;
        }

        if total_stored_size < target_size {
            lore_base::lore_debug!(
                "Size eviction for group {group_index} skipped, currently {total_stored_size} bytes, target {target_size}"
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
        let bucket_count = group.bucket_count.load(atomic::Ordering::Relaxed);
        for bucket_index in 0..bucket_count {
            while serialize_tasks.try_join_next().is_some() {}

            if self.gc_stop_requested() {
                lore_base::lore_debug!(
                    "Size eviction for group {group_index} stopping at bucket {bucket_index}"
                );
                break;
            }

            let Some(bucket) = group.try_bucket(bucket_index).cloned() else {
                continue;
            };
            let bucket_stored_size = bucket_stored_size[bucket_index];
            let mut entry: Vec<((u32, u32), (u32, u64))> = {
                let bucket = bucket.read().await;

                // Grab only the newest timestamp for each hash with payload
                let mut stored_payloads = HashMap::with_capacity(bucket.sorted_index.len());
                for entry in bucket.entry.iter() {
                    if entry.data.pack_file == 0 {
                        continue;
                    }

                    // Clients cannot evict fragments only stored locally
                    if protect_local_fragment {
                        let stored_durable =
                            entry.data.flags & FragmentFlags::PayloadStoredDurable != 0;
                        if !stored_durable {
                            continue;
                        }
                    }

                    let key = (entry.data.pack_file, entry.data.pack_offset);
                    let last_access = Self::load_last_access(&entry.data);
                    stored_payloads
                        .entry(key)
                        .and_modify(|item: &mut (u32, u64)| item.1 = last_access)
                        .or_insert((entry.data.size_payload, last_access));
                }
                stored_payloads.drain().collect()
            };

            entry.sort_unstable_by_key(|left| left.1);

            let mut evicted_payloads = HashSet::with_capacity(entry.len() / 4);
            let mut estimated_evicted_size = 0;
            let mut cutoff_point = u64::MAX;
            for ((pack_file, pack_offset), (size_payload, last_access)) in entry.iter() {
                let key = (*pack_file, *pack_offset);
                if evicted_payloads.contains(&key) {
                    continue;
                }

                evicted_payloads.insert(key);

                estimated_evicted_size += *size_payload as usize;
                if bucket_stored_size.saturating_sub(estimated_evicted_size) < bucket_target_size {
                    cutoff_point = *last_access;
                    break;
                }
            }

            drop(entry);

            evicted_payloads.clear();

            let mut evicted_count = 0;
            let mut evicted_payload_count = 0;
            let mut evicted_payload_size = 0;
            {
                let mut bucket_lock = bucket.write().await;
                let bucket = &mut *bucket_lock;
                let (entry, sorted_index) = (&mut bucket.entry, &bucket.sorted_index);
                for index in sorted_index.iter() {
                    let index = *index as usize;
                    let entry = &mut entry[index];

                    if entry.data.pack_file == 0 {
                        continue;
                    }
                    // Protected fragments are absent from the candidate set, so the
                    // cutoff (which stays u64::MAX for an all-non-durable bucket) must
                    // not be applied to them.
                    if protect_local_fragment
                        && entry.data.flags & FragmentFlags::PayloadStoredDurable == 0
                    {
                        continue;
                    }
                    if entry.data.last_access > cutoff_point {
                        continue;
                    }

                    let key = (entry.data.pack_file, entry.data.pack_offset);

                    entry.data.pack_file = 0;
                    entry.data.pack_offset = 0;
                    evicted_count += 1;

                    if !evicted_payloads.contains(&key) {
                        evicted_payload_count += 1;
                        evicted_payload_size += entry.data.size_payload as usize;
                        evicted_payloads.insert(key);
                    }
                }

                group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
            }

            lore_base::lore_trace!(
                "Size eviction for group {group_index} evicted {evicted_payload_count} payloads from {evicted_count} of {bucket_count} entries"
            );

            total_evicted_size += evicted_payload_size;
            total_evicted_count += evicted_payload_count;

            if let Some(path) = path.as_ref() {
                let path = path.clone();
                let store = self.clone();
                lore_base::lore_spawn!(serialize_tasks, async move {
                    let group = store.group[group_index].clone();
                    let bucket = group.bucket(bucket_index).clone();
                    let _flush_guard = group.flush_lock.clone().lock_owned().await;
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

        if let Some(path) = path.as_ref() {
            let flush_guard = group.flush_lock.clone().lock_owned().await;
            crate::local::fan_out::commit_if_initial_level(
                &flush_guard,
                &group.committed_level,
                &group.bucket_count,
                path.as_ref(),
                group_index,
                sync_data,
            )
            .await;
        }

        if total_evicted_count > 0 {
            lore_base::lore_debug!(
                "Size evicted for group {group_index} with {total_evicted_count} of {total_stored_count} payloads, {total_evicted_size} of {total_stored_size} bytes orphaned in pack store"
            );
        } else {
            lore_base::lore_debug!("Size eviction for group {group_index} did not evict anything");
        }

        (total_evicted_count, total_evicted_size)
    }

    pub async fn evict_oldest(
        &self,
        max_capacity: usize,
        sink: Option<&crate::gc_event::GcEventSinkRef>,
    ) -> usize {
        let mut evict_count = 0;
        let mut began = false;
        // Held from the first eviction to the end of the pass, so the epoch advances
        // once rather than per bucket, and not at all when there is nothing to evict.
        let mut writing = None;

        if self.gc_stop_requested() {
            return 0;
        }
        let Ok(_permit) = self.eviction.acquire().await else {
            lore_base::lore_warn!("Evict oldest failed to get permit");
            return 0;
        };
        if self.gc_stop_requested() {
            return 0;
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
                    let entry_count = {
                        let bucket = bucket_ref.read().await;
                        if protect_local_fragment {
                            bucket
                                .entry
                                .iter()
                                .filter(|entry| {
                                    entry.data.flags & FragmentFlags::PayloadStoredDurable != 0
                                })
                                .count()
                        } else {
                            bucket.entry.len()
                        }
                    };
                    total_count += entry_count;
                    if entry_count > target_capacity {
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
                        return evict_count;
                    }
                    Err(err) => {
                        lore_base::lore_warn!("Eviction could not claim the store: {err}");
                        return evict_count;
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

        // Released with the pass rather than at the first eviction: one claim covers
        // the whole run, so the epoch advances once however many buckets it touches.
        drop(writing);
        evict_count
    }

    pub async fn evict_oldest_bucket(
        bucket: Arc<RwLock<ImmutableStoreBucket>>,
        dirty: &AtomicBool,
        target_capacity: usize,
        protect_local_fragment: bool,
    ) -> usize {
        let cutoff_point = {
            let bucket = bucket.read().await;

            // Non-durable fragments are protected: excluded from the count and never evicted.
            let evictable_count = if protect_local_fragment {
                bucket
                    .entry
                    .iter()
                    .filter(|entry| entry.data.flags & FragmentFlags::PayloadStoredDurable != 0)
                    .count()
            } else {
                bucket.entry.len()
            };
            if evictable_count <= target_capacity {
                return 0;
            }
            let to_evict = evictable_count - target_capacity;

            let mut heap: BinaryHeap<u64> = BinaryHeap::with_capacity(to_evict);
            for entry in bucket.entry.iter() {
                if protect_local_fragment
                    && entry.data.flags & FragmentFlags::PayloadStoredDurable == 0
                {
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

            *heap.peek().unwrap()
        };

        // Build new arrays with the remaining items
        let mut sorted_index = GrowVec::new();
        let mut entry = GrowVec::new();

        // Accessing in sorted order means we don't have to resort
        let mut bucket = bucket.write().await;
        for index in bucket.sorted_index.iter() {
            let index = *index as usize;

            let data = &bucket.entry[index].data;
            let protected =
                protect_local_fragment && data.flags & FragmentFlags::PayloadStoredDurable == 0;
            if !protected && data.last_access <= cutoff_point {
                continue;
            }

            let new_index = entry.len() as u32;
            sorted_index.push(new_index);
            entry.push(bucket.entry[index]);
        }

        let evict_count = bucket.entry.len() - entry.len();
        // Nothing left to evict once the bucket is locked to write — another pass or a
        // reload got there first — leaves it as it is and unmarked: a mark with no change
        // would rewrite the bucket under a write claim, announcing a change that never
        // happened.
        if evict_count > 0 {
            bucket.sorted_index = sorted_index;
            bucket.entry = entry;
            dirty.store(true, atomic::Ordering::Relaxed);
        }

        evict_count
    }

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
        if starting_pass {
            let total_size = self.clone().packstore_total_size().await;
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

        // Loading reads the store's files, so it is claimed; to read, because a pass
        // may still stop or lose the permit below without changing anything. The size
        // above came from what this store held without a claim, so a claim that reloaded
        // the store decides again from what is on disk now.
        let reading = match self.try_hold(Intent::Read).await? {
            BackgroundClaim::Held(claim) => Some(claim),
            BackgroundClaim::Unlocked => None,
            // Left for another pass rather than queued behind a store in use. `None` has
            // the compactor wait out its interval, and a resumed pass reads its resume point
            // back from disk.
            BackgroundClaim::Busy => return Ok(None),
        };
        if starting_pass
            && reading.as_ref().is_some_and(StoreGuard::is_stale)
            && self.packstore_total_size().await < max_size
        {
            return Ok(None);
        }
        let _reading = reading;
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
        // anything.
        //
        // **A write claim that reloaded the store ends the pass.** The buckets loaded above
        // are gone with the reload, and rewriting packfiles against buckets that no longer
        // describe them would truncate payloads still referenced on disk. The resume point
        // stays where it was, so the next pass starts from state loaded under its own claim.
        let writing = self.hold(Intent::Write).await?;
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

        // A group that stopped still has packfiles to rewrite, so `group_index` is where a
        // later pass picks up rather than what this step reached.
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
            let total_size = self.packstore_total_size().await;
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

    /// Rewrite the group's packfiles into fewer, denser ones, one packfile at a time.
    ///
    /// Returns whether the group was left complete. `false` means a stop request ended
    /// the pass with packfiles still to rewrite, and the caller must hold the compaction
    /// resume point so a later pass repeats this group.
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

        let mut packfile = 1;
        let mut group_reclaimed: u64 = 0;
        let mut completed = true;
        loop {
            // A packfile is the unit of compaction work. Its bucket sweep must reach the
            // truncate below: buckets already rewritten point at the new packfile while
            // the rest still point at this one, so abandoning a sweep part way would
            // leave payloads that are still referenced in a packfile about to be dropped.
            if self.gc_stop_requested() {
                lore_base::lore_debug!(
                    "Packstore compactor stopping group {group_index} before packfile {packfile}"
                );
                completed = false;
                break;
            }

            let group = &self.group[group_index];
            if let Ok(current_size) = group.packstore.total_size().await
                && current_size < target_size
            {
                lore_base::lore_debug!(
                    "Packstore compactor complete group {group_index}, current size {current_size} below target size {target_size}"
                );
                break;
            }
            let Ok(original_size) = group.packstore.stop_write(packfile).await else {
                lore_base::lore_debug!(
                    "Packstore compactor complete group {group_index}, no more packfiles"
                );
                break;
            };

            lore_base::lore_debug!(
                "Packstore compactor running on group {group_index} packfile {packfile} size {original_size}"
            );

            let mut compacted_size = 0;
            let active_buckets = self.group[group_index]
                .bucket_count
                .load(atomic::Ordering::Relaxed);
            for bucket_index in 0..active_buckets {
                lore_base::lore_trace!(
                    "Packstore compactor running on group {group_index} bucket {bucket_index} packfile {packfile} size {original_size}"
                );
                compacted_size += self
                    .clone()
                    .compact_bucket_packfile(group_index, bucket_index, packfile, sync_data)
                    .await;

                if let Some(path) = path.as_ref() {
                    lore_base::lore_trace!(
                        "Packstore compactor serializing group {group_index} bucket {bucket_index}"
                    );
                    let bucket = group.bucket(bucket_index).clone();
                    // Narrow scope on purpose: this fn already called
                    // evict_group_sized above, which takes the same lock.
                    let _flush_guard = group.flush_lock.clone().lock_owned().await;
                    let bucket = bucket.read_owned().await;
                    let _ = ImmutableStoreBucket::serialize(
                        bucket,
                        group.clone(),
                        path.as_ref(),
                        group_index,
                        bucket_index,
                        sync_data,
                    )
                    .await;
                }

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

            if let Some(path) = path.as_ref() {
                let flush_guard = group.flush_lock.clone().lock_owned().await;
                crate::local::fan_out::commit_if_initial_level(
                    &flush_guard,
                    &group.committed_level,
                    &group.bucket_count,
                    path.as_ref(),
                    group_index,
                    sync_data,
                )
                .await;
            }

            lore_base::lore_debug!(
                "Packstore compactor group {group_index} truncating packfile {packfile}"
            );
            let _ = group.packstore.truncate(packfile).await;

            lore_base::lore_debug!(
                "Packstore compactor finished group {group_index} packfile {packfile}, {original_size} -> {compacted_size} bytes"
            );
            instruments
                .group_final_total_size
                .record(compacted_size as u64, &labels);

            group_reclaimed += (original_size as u64).saturating_sub(compacted_size as u64);
            packfile += 1;
        }

        if group_reclaimed > 0 {
            if let Some(sink) = &sink {
                sink.compaction_progress(group_reclaimed);
            }
            self.compaction_reclaimed
                .fetch_add(group_reclaimed, atomic::Ordering::Relaxed);
        }

        Ok(completed)
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

    pub async fn compact_bucket_packfile(
        self: Arc<Self>,
        group_index: usize,
        bucket_index: usize,
        packfile: u32,
        sync_data: bool,
    ) -> usize {
        let group = &self.group[group_index];
        Self::compact_bucket_packfile_impl(group, group_index, bucket_index, packfile, sync_data)
            .await
    }

    pub async fn compact_bucket_packfile_impl(
        group: &ImmutableStoreGroup,
        group_index: usize,
        bucket_index: usize,
        packfile: u32,
        sync_data: bool,
    ) -> usize {
        let mut packfiles_to_flush = vec![];

        let (entry, sorted_index) = {
            let lock = group.bucket(bucket_index).read().await;
            lock.clone_for_compaction()
        };

        let mut compacted_size = 0;
        let mut last_hash = Hash::default();
        let mut rewritten = Vec::with_capacity(entry.len());

        for index in sorted_index.iter() {
            let mut entry = entry[*index as usize];
            if entry.data.pack_file != packfile {
                continue;
            }

            if entry.address.hash == last_hash {
                lore_base::lore_trace!("Entry {entry:?} reuse rewritten data for hash");
                continue;
            }

            lore_base::lore_trace!("Entry {entry:?} repacking");

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
                    lore_base::lore_trace!("Entry loaded {} bytes payload", payload.len());

                    match group.packstore.store(payload).await {
                        Ok(packref) => {
                            debug_assert!(
                                packref.id != packfile,
                                "Compaction wrote data to same packfile being repacked"
                            );
                            lore_base::lore_trace!("Entry stored in new packref {packref:?}");

                            entry.data.pack_file = packref.id;
                            entry.data.pack_offset = packref.offset;

                            last_hash = entry.address.hash;

                            rewritten.push(entry);

                            compacted_size += entry.data.size_payload as usize;

                            if !packfiles_to_flush.contains(&packref.id) {
                                packfiles_to_flush.push(packref.id);
                            }
                        }
                        Err(err) => {
                            debug_assert!(false, "Failed to store data for compaction: {err}");
                            lore_base::lore_warn!("Failed to store data for compaction: {err}");
                        }
                    }
                }
                Err(err) => {
                    debug_assert!(
                        false,
                        "Failed to load data for compaction: packfile {} offset {} payload size {}: {err}",
                        entry.data.pack_file, entry.data.pack_offset, entry.data.size_payload
                    );
                    lore_base::lore_warn!(
                        "Failed to load data for compaction: packfile {} offset {} payload size {}: {err}",
                        entry.data.pack_file,
                        entry.data.pack_offset,
                        entry.data.size_payload
                    );
                }
            }
        }

        lore_base::lore_trace!(
            "Packstore compactor rewrote {} of {} fragments from group {} bucket {} packfile {} into {} packfiles",
            rewritten.len(),
            sorted_index.len(),
            group_index,
            bucket_index,
            packfile,
            packfiles_to_flush.len()
        );

        for packfile in packfiles_to_flush {
            lore_base::lore_trace!(
                "Packstore compactor group {group_index} bucket {bucket_index} flushing packfile {packfile}"
            );
            let _ = group.packstore.flush(packfile, sync_data).await;
        }

        // Now write back the updated entries
        lore_base::lore_trace!(
            "Packstore compactor group {group_index} bucket {bucket_index} reinserting rewritten entries"
        );
        if !rewritten.is_empty() {
            let mut bucket_lock = group.bucket(bucket_index).write().await;
            let bucket = &mut *bucket_lock;
            let (entry, sorted_index) = (&mut bucket.entry, &bucket.sorted_index);

            let mut match_index = 0;
            for rewritten in rewritten.iter() {
                while match_index < sorted_index.len() {
                    let entry = &mut entry[sorted_index[match_index] as usize];
                    match entry.address.hash.cmp(&rewritten.address.hash) {
                        Ordering::Less => {
                            if entry.data.pack_file == packfile {
                                entry.data.pack_file = 0;
                                entry.data.pack_offset = 0;
                            }
                            match_index += 1;
                        }
                        Ordering::Equal => {
                            entry.data.assign_deduplicated_payload(rewritten.data);
                            match_index += 1;
                        }
                        Ordering::Greater => {
                            break;
                        }
                    }
                }
            }

            debug_assert!(
                !bucket
                    .entry
                    .iter()
                    .any(|entry| entry.data.pack_file == packfile),
                "Entry remains in group {group_index} bucket {bucket_index} referencing packfile {packfile}"
            );

            group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
        }

        lore_base::lore_trace!(
            "Packstore compactor group {group_index} bucket {bucket_index} packfile {packfile} rewrite complete, {compacted_size} bytes rewritten"
        );

        compacted_size
    }

    pub async fn packstore_total_size(&self) -> usize {
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

    /// Immediate flush of all dirty buckets. Parallel across groups, sequential within a group.
    async fn flush_all(
        self: Arc<Self>,
        path: Option<Arc<PathBuf>>,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        let Some(path) = path else {
            return Ok(());
        };

        let mut tasks = JoinSet::new();

        for group_index in 0..self.group.len() {
            let group = self.group[group_index].clone();

            // Lock-free scan: a group with no dirty bucket has taken no write since the last
            // flush, so it has nothing to serialize and owes no marker. A marker-less group
            // reloads at bucket_count = 256, correct for an empty group at any level, and its
            // first write fires the two-phase commit. Empty groups cost ~nothing per flush.
            let any_dirty = (0..group.dirty.len()).any(|bucket| group.needs_flush(bucket));
            if !any_dirty {
                continue;
            }

            let path = path.clone();
            lore_base::lore_spawn!(tasks, async move {
                // One flusher per group at a time, held for the whole group flush so an
                // overlapping flush cannot observe a half-finished level transition and
                // take the other commit path. See `ImmutableStoreGroup::flush_lock`.
                let _flush_guard = group.flush_lock.clone().lock_owned().await;

                flush_locked_group(group, group_index, path, sync_data).await
            });
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
                    entry.data.flags |=
                        FragmentFlags::PayloadStoredDurable | FragmentFlags::PayloadStoredLocal;
                }
            }
        }
    }

    // For test purposes, mark a single fragment as NOT durably stored
    pub async fn mark_as_not_durably_stored(&self, partition: Partition, address: Address) {
        let group_index = address.hash.data()[0] as usize;
        let group = &self.group[group_index];
        let (bucket_index, mut bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&address.hash, n);
            let lock = group.bucket(idx).clone().write_owned().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) == n {
                break (idx, lock);
            }
            drop(lock);
        };
        if !bucket.deserialized && self.path.is_some() {
            let _ = bucket
                .deserialize(
                    &group.dirty[bucket_index],
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
        // Read intent: this hold is for ordering, not for permission. An operation
        // that writes takes its own write hold, which joins this one and advances
        // the epoch then.
        self.hold(Intent::Read).await
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
    /// entry and there is nothing further to fetch.
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

        let mut local_flags = 0;
        if find.data.pack_file != 0 {
            local_flags |= FragmentFlags::PayloadStoredLocal.bits();
        }
        if self.settings.implicit_durable_stored {
            local_flags |= FragmentFlags::PayloadStoredDurable.bits();
        }

        Ok(StoreGetData::metadata(
            Fragment {
                flags: find.data.flags | local_flags,
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

        let mut local_flags = 0;
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
        let payload = Self::load(&self.group[find.group].packstore, find.data)
            .await
            .forward::<StoreError>("Failed to load payload from local storage.")?;
        crate::validate_fragment_payload(&fragment, payload.len())?;

        Ok(StoreGetData {
            fragment,
            match_made: find.matching,
            partition: find.partition,
            payload: Some(payload),
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

        let mut local_flags = 0;
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

        if let Some(payload) = payload.as_ref() {
            crate::validate_fragment_payload(&fragment, payload.len())?;
        } else if fragment.size_payload as usize > crate::FRAGMENT_SIZE_THRESHOLD {
            return Err(StoreError::from(crate::errors::Oversized {
                context: format!(
                    "fragment size_payload {} exceeds FRAGMENT_SIZE_THRESHOLD {} on put",
                    fragment.size_payload,
                    crate::FRAGMENT_SIZE_THRESHOLD
                ),
            }));
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
                let found = self
                    .find(partition, address, Lookup::Write)
                    .await
                    .forward::<StoreError>("Failed to deserialize store data.")?;

                lore_base::lore_debug!("Lookup match for {address}: {:?}", found.matching);

                if found.matching != StoreMatch::MatchFull {
                    return Err(StoreError::from(AddressNotFound::from(address)));
                }
                let data = found.data;

                if (data.flags & FragmentFlags::PayloadFragmented) != 0 {
                    lore_base::lore_debug!("Payload fragmented, obliterating subfragments");

                    let group = &self.group[address.hash.data()[0] as usize];
                    if let Ok(payload) = Self::load(&group.packstore, data).await.inspect_err(|e| {
                        lore_base::lore_warn!(
                            "Failed to load fragment while obliterating address {address}: {e:?}"
                        );
                    }) {
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
        let evicted = timed!(
            self.instruments.operation_latency,
            &self.instruments.get_labels_for_operation_context("evict"),
            Ok(self.evict_oldest(max_capacity, sink.as_ref()).await)
        );
        // **What a pass evicts, the pass writes.** Eviction empties buckets in memory
        // and nothing else is scheduled to write them, so left dirty they would keep the
        // flock for as long as this process kept the store — for a timer running in the
        // background of a process that has finished with the store, indefinitely.
        if let Ok(count) = &evicted.result
            && *count > 0
        {
            let _ = crate::immutable_store::ImmutableStore::flush(self.clone(), sync_data).await;
        }
        // A pass that evicted nothing wrote nothing, but a write claim taken on the way
        // still marked the store dirty; this is what releases it.
        self.note_flushed();
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
        // Released when nothing was compacted, for the reason `evict` gives.
        self.note_flushed();
        compacted.into()
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

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        let hold = self
            .hold(if heal { Intent::Write } else { Intent::Read })
            .await?;
        // Held for the whole walk: eviction and compaction rewrite the very entries and
        // packfiles being read, so a pass running alongside reports failures that are only
        // the store moving underneath it.
        let evict_permit = self.eviction.acquire().await;
        let compact_permit = self.compaction.acquire().await;

        let _ = self.deserialize_all_buckets().await;

        let mut failed = vec![];

        let mut fragment_count = 0;
        let mut verified_count = 0;
        for (group_index, group) in self.group.iter().enumerate() {
            let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
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
                        failed.push((group_index, bucket_index, entry.address.hash, entry.data));
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
                            failed.push((group_index, bucket_index, entry.address.hash, entry.data));
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
                            failed.push((
                                group_index,
                                bucket_index,
                                entry.address.hash,
                                entry.data,
                            ));
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
                            failed.push((
                                group_index,
                                bucket_index,
                                entry.address.hash,
                                entry.data,
                            ));
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
                            failed.push((
                                group_index,
                                bucket_index,
                                entry.address.hash,
                                entry.data,
                            ));
                            continue;
                        }
                    }

                    verified_count += 1;
                }
            }
        }

        lore_base::lore_debug!(
            "Verified {verified_count} fragments with payloads of {fragment_count} total fragments"
        );
        if !failed.is_empty() {
            lore_base::lore_debug!("{} invalid fragments", failed.len());

            for (group_index, failed_bucket_index, failed_hash, failed_data) in failed.iter() {
                let group = &self.group[*group_index];
                let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
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

            if heal {
                drop(evict_permit);
                drop(compact_permit);
                let _ = crate::immutable_store::ImmutableStore::flush(self.clone(), false).await;

                lore_base::lore_debug!("Store healing complete");
            }
        }
        lore_base::lore_debug!("Store verification complete");

        // The flush above ran inside this pass's own write claim, which counts as a
        // writer in flight, so it could not release the store. Dropped first, the claim
        // is released with nothing left to hold it.
        drop(hold);
        self.note_flushed();
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
        let mut pending = PendingFlush::Nothing;
        for group in &self.group {
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
                // A write claim that wrote nothing still marked the store dirty; this is
                // what releases it.
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
        // Dropped before declaring the store clean. `clear_dirty` refuses while any
        // write claim is in flight — a writer that has announced itself but not yet
        // marked its bucket would otherwise be declared clean on its behalf — and this
        // flush's own claim is one of those until it lets go. A flush that failed
        // declares nothing: what it could not write keeps the store claimed.
        drop(hold);
        if flushed.is_ok() {
            self.note_flushed();
        }
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
        durable: bool,
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
        let (bucket_index, mut bucket) = loop {
            let n = group.bucket_count.load(atomic::Ordering::Relaxed);
            let idx = crate::local::fan_out::bucket_index_for(&source_address.hash, n);
            let lock = group.bucket(idx).clone().write_owned().await;
            if group.bucket_count.load(atomic::Ordering::Relaxed) == n {
                break (idx, lock);
            }
            drop(lock);
        };

        if !bucket.deserialized && self.path.is_some() {
            bucket
                .deserialize(
                    &group.dirty[bucket_index],
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
            entry.data.merge_from_copy_source(source_data, durable);
            if entry.data != before {
                entry.data.last_access = Self::last_access();
                group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
            }
            return Ok(());
        }

        let mut data = ImmutableData::default();
        data.merge_from_copy_source(source_data, durable);
        data.last_access = Self::last_access();

        let count = bucket.entry.len();
        bucket.sorted_index.insert(insert_slot, count as u32);
        bucket.entry.push(ImmutableStoreEntry {
            address: destination_address,
            partition: destination_partition,
            data,
        });
        group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);

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
        let hold = self
            .hold(if heal { Intent::Write } else { Intent::Read })
            .await?;
        let verified = self
            .clone()
            .verify_fragment_held(path, address, partition, match_requested, heal)
            .await;
        // Released before declaring the store clean, for the reason `verify` gives, and on
        // every outcome: a write claim marked the store dirty as it was taken, so a verify
        // that returned early without this would keep the store claimed.
        drop(hold);
        self.note_flushed();
        verified
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
                                &group.dirty[idx],
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

    // Re-check under the lock: another flusher may have drained this group
    // while we waited. The scan that got us here is lock-free and stale by
    // now, so skip the redundant fan-out check, path selection and - in the
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
            group.flush_packstore(sync_data).await;
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

    // Always flush the packstore once per group when sync_data is set, regardless of which serialize path runs below.
    if sync_data {
        group.flush_packstore(sync_data).await;
    }

    if needs_two_phase_commit && first_err.is_none() {
        // T10 two-phase commit. Every [0..active_buckets] bucket gets a .new file (skipping empties at index >= committed_level since no old file exists there to overwrite). After all .new files are durable, write level.pending as the commit point. Then rename .new -> final, write the level marker, delete level.pending. Recovery on the next store open rolls forward from any pending state.
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

        let mut wrote_new: Vec<usize> = Vec::new();
        if first_err.is_none() {
            for bucket_index in 0..active_buckets {
                // Fast path: skip the bucket entirely (no lock acquire) when it's neither dirty nor an old-level slot we need to overwrite. The dirty flag is the cheap proxy for "this bucket has data to flush"; combined with the index < committed_level check (which forces an empty .new to overwrite stale level-N files), this avoids 256× read-lock acquires per group on the common server-fresh-store first flush where most buckets are empty and committed_level == 0.
                let must_overwrite_old = bucket_index < committed_level;
                let dirty = group.needs_flush(bucket_index);
                if !must_overwrite_old && !dirty {
                    continue;
                }
                let bucket = group.bucket(bucket_index).clone().read_owned().await;
                // Re-check after lock acquire — concurrent paths may have just dirtied or undirtied this bucket.
                if bucket.entry.is_empty() && !must_overwrite_old {
                    // Nothing to write and no older file to overwrite, so the flags
                    // are settled here: left set, they would keep the store claimed
                    // after a flush with nothing more to do. Cleared under the read
                    // lock, which every writer is excluded by.
                    group.dirty[bucket_index].store(false, atomic::Ordering::Relaxed);
                    group.soft_dirty[bucket_index].store(false, atomic::Ordering::Relaxed);
                    continue;
                }
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
                    Ok(()) => wrote_new.push(bucket_index),
                    Err(err) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                        }
                    }
                }
            }
        }

        if wrote_new.is_empty() {
            // No .new files written for this group — skip the level.pending sentinel entirely. The sentinel exists to drive roll-forward recovery of a partially-completed transition; with no .new files there is no in-progress state to recover, so a direct marker write is sufficient. Restores ~3x throughput on fresh-store-first-flush-with-sync_data when most groups are empty (the common shape on `lore repository create`).
            if first_err.is_none()
                && let Err(err) = crate::local::fan_out::write_level_marker(
                    &group_path,
                    active_buckets,
                    sync_data,
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
                && let Err(err) = crate::local::fan_out::write_level_pending(
                    &group_path,
                    active_buckets,
                    sync_data,
                )
                .await
                .map_err(|e| {
                    LocalImmutableStoreError::internal_with_context(
                        e,
                        "Failed to write level.pending",
                    )
                })
            {
                first_err = Some(err);
            }

            if first_err.is_none() {
                for &bucket_index in &wrote_new {
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

    let mut guards: Vec<tokio::sync::OwnedRwLockWriteGuard<ImmutableStoreBucket>> =
        Vec::with_capacity(target);
    for i in 0..target {
        guards.push(group.bucket(i).clone().write_owned().await);
    }

    // Force-deserialize any [0..n] bucket whose entries are still on disk only. Without this, on-disk-only buckets contribute zero entries to the redistribute and their data is lost when serialize overwrites their files with empty buckets at the new layout.
    for (bucket_index, guard) in guards.iter_mut().take(n).enumerate() {
        if !guard.deserialized {
            Box::pin(guard.deserialize(
                &group.dirty[bucket_index],
                path,
                group_index,
                bucket_index,
                None,
            ))
            .await?;
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
pub async fn create(
    path: Option<impl AsRef<Path>>,
    options: ImmutableStoreCreateOptions,
    deserialize_buckets: bool,
    settings: ImmutableStoreSettings,
) -> Result<Arc<dyn crate::immutable_store::ImmutableStore>, StoreError> {
    let path = path.as_ref();
    let store = LocalImmutableStore::new(path.map(|path| path.as_ref().to_path_buf()), settings)
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

    Ok(store)
}
