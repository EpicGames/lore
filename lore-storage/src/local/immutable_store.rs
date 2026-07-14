// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::backtrace::Backtrace;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
#[cfg(target_family = "windows")]
use std::os::windows::fs::OpenOptionsExt;
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
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::allocator::GrowVec;
use lore_base::fs::lock::FSLock;
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
use crate::store_types::StoreMatch;
use crate::store_types::StoreObliterateStats;
use crate::store_types::StoreQueryResult;

#[error_set]
pub enum LocalImmutableStoreError {
    NotSupported,
}

const VALIDATE_COMPACTION: bool = false;

pub const GROUP_COUNT: usize = 256;
pub const BUCKET_COUNT: usize = 256;

pub const DEFAULT_FLUSH_DELAY_SECONDS: u64 = 5;

const DOT_COMPACT: &str = "compact";

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

#[derive(Default)]
pub struct ImmutableStoreBucket {
    pub entry: GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
    pub sorted_index: GrowVec<u32, CHUNK_SIZE_U32>,
    deserialized: bool,
    upgrade_packfile: bool,
    serialize_lock: Arc<Mutex<()>>,
}

impl ImmutableStoreBucket {
    fn clone_for_compaction(&self) -> (Vec<ImmutableStoreEntry>, Vec<u32>) {
        (self.entry.to_vec(), self.sorted_index.to_vec())
    }
}

pub struct ImmutableStoreGroup {
    /// Per-slot lazily-initialized bucket. Empty `OnceLock` at construction; first
    /// `bucket()` call materializes the `Arc<RwLock<ImmutableStoreBucket>>`. Use
    /// `try_bucket()` for paths that must be a no-op when the slot has never been
    /// touched (flush of a clean slot, dirty-only scans, diagnostic iteration).
    pub bucket: [OnceLock<Arc<RwLock<ImmutableStoreBucket>>>; BUCKET_COUNT],
    /// Dirty flag per bucket, kept outside the bucket's `RwLock` so `flush_all`
    /// can scan for work with lock-free atomic loads.
    pub dirty: [AtomicBool; BUCKET_COUNT],
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
    pub packstore: crate::PackStore,
    pub flush: Mutex<JoinSet<()>>,
}

impl ImmutableStoreGroup {
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

#[cfg(feature = "failure_generator")]
pub struct LocalImmutableStoreFailureGenerator {
    retry_rate: f32,
    miss_fragment_writes: HashSet<Hash>,
}

pub struct LocalImmutableStore {
    path: Option<Arc<PathBuf>>,
    pub group: Vec<Arc<ImmutableStoreGroup>>,
    eviction: Semaphore,
    compaction: Semaphore,
    stop_gc: AtomicBool,
    /// Bytes reclaimed by the current compaction pass, accumulated across its
    /// stepped `compact` calls and reset at the start of each pass. Reported in
    /// the compaction-end progress callback.
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

    // This field must be dropped last so it must be declared last
    #[allow(dead_code)]
    lock: Option<FSLock>,
}

pub struct ImmutableStoreSettings {
    /// Allow partial fragments (true for clients, false for server)
    pub allow_partial_fragment: bool,
    /// Protect local fragments during eviction/compaction (true for clients, false for server)
    pub protect_local_fragment: bool,
    /// Consider all fragments durably stored (false for clients, generally true for server)
    pub implicit_durable_stored: bool,
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
    /// Update last access timestamps on reads
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
            allow_partial_fragment: true,
            protect_local_fragment: true,
            implicit_durable_stored: false,
            flush_background: false,
            flush_delay_seconds: DEFAULT_FLUSH_DELAY_SECONDS,
            target_capacity_percentage: 70,
            target_size_percentage: 70,
            compaction_parallel_groups: 8,
            verify_write: false,
            atime: false,
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
    use std::io;
    use std::io::Write;
    let mut path = path.to_path_buf();
    path.reserve(20);
    path.push("index");
    let mut hexstring: [u8; 8] = Default::default();
    {
        let mut cursor = io::Cursor::new(&mut hexstring[..]);
        let _ = write!(&mut cursor, "{:02x}", group_index as u8);
        path.push(std::str::from_utf8(&hexstring[..2]).unwrap_or_default());
    }
    {
        let mut cursor = io::Cursor::new(&mut hexstring[..]);
        let _ = write!(&mut cursor, "index_{:02x}", bucket_index as u8);
        path.push(std::str::from_utf8(&hexstring).unwrap_or_default());
    }
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
            if !name_str.starts_with("index_") || name_str.ends_with(".new") {
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

pub struct SerializeFailureGuard<'a> {
    success: bool,
    dirty: &'a AtomicBool,
    path: &'a Path,
}

impl<'a> SerializeFailureGuard<'a> {
    pub fn new(dirty: &'a AtomicBool, path: &'a Path) -> Self {
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

            if let Err(err) = std::fs::remove_file(self.path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                lore_base::lore_warn!(
                    "Failed to remove temporary file {}: {err:?}",
                    self.path.display()
                );
            }
        }
    }
}

impl ImmutableStoreBucket {
    fn deserialize_files(
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
        let mut file = match std::fs::File::options()
            .read(true)
            .write(false)
            .create(false)
            .open(&path)
        {
            Ok(file) => file,
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
        match Self::deserialize_files_parse(&path, &mut file) {
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
                Self::recover_corrupt_bucket(&path, reason)
            }
        }
    }

    fn deserialize_files_parse(
        path: &Path,
        file: &mut std::fs::File,
    ) -> Result<
        (
            GrowVec<u32, CHUNK_SIZE_U32>,
            GrowVec<ImmutableStoreEntry, CHUNK_SIZE_ENTRY>,
            bool,
            bool,
        ),
        DeserializeFileError,
    > {
        let mut header = ImmutableStoreHeader::new_zeroed();
        file.read_exact(header.as_mut_bytes())
            .map_err(|err| DeserializeFileError::Corrupt(format!("read header: {err}")))?;

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

        let file_size = file
            .metadata()
            .map_err(|err| DeserializeFileError::Corrupt(format!("read metadata: {err}")))?
            .len() as usize;
        let header_size = size_of::<ImmutableStoreHeader>();
        if file_size < header_size {
            return Err(DeserializeFileError::Corrupt(format!(
                "file size {file_size} smaller than header size {header_size}"
            )));
        }
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
        let mut mark_dirty = false;

        let sorted_index = GrowVec::read_from_file(file, expected_count)
            .map_err(|err| DeserializeFileError::Corrupt(format!("read sorted index: {err}")))?;

        // LazyFanOut keeps the LastAccessInEntry layout, so any version ≥ LastAccessInEntry uses the new entry layout.
        let entry = if header.version >= ImmutableStoreVersion::LastAccessInEntry as u32 {
            GrowVec::read_from_file(file, expected_count)
                .map_err(|err| DeserializeFileError::Corrupt(format!("read entries: {err}")))?
        } else {
            let entry_old: GrowVec<ImmutableStoreEntryBeforeLastAccess, CHUNK_SIZE_ENTRY> =
                GrowVec::read_from_file(file, expected_count).map_err(|err| {
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

            mark_dirty = true;

            entry
        };

        Ok((sorted_index, entry, upgrade_packfile, mark_dirty))
    }

    /// Drop the corrupt file and return an empty bucket. Pack file payloads survive
    /// until compaction reclaims the now-orphaned ranges.
    fn recover_corrupt_bucket(
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
        if let Err(err) = std::fs::remove_file(path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(LocalImmutableStoreError::internal_with_context(
                err,
                "Failed to remove corrupt storage bucket",
            ));
        }
        Ok((GrowVec::new(), GrowVec::new(), false, false))
    }

    fn serialize_files(
        bucket: OwnedRwLockReadGuard<ImmutableStoreBucket, ImmutableStoreBucket>,
        group: Arc<ImmutableStoreGroup>,
        bucket_index: usize,
        path: PathBuf,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        // Append `.tmp` rather than replacing the extension, so a fan-out-commit path like `index_<bb>.new` becomes `index_<bb>.new.tmp`. set_extension would clobber `.new` to `.tmp`, colliding with the regular flush path's tmp file.
        let temporary_path = if sync_data {
            let mut p = path.as_os_str().to_owned();
            p.push(".tmp");
            PathBuf::from(p)
        } else {
            path.clone()
        };
        let mut temporary_guard = if sync_data {
            Some(SerializeFailureGuard::new(
                &group.dirty[bucket_index],
                &temporary_path,
            ))
        } else {
            None
        };

        if let Some(parent_path) = temporary_path.parent()
            && !parent_path.exists()
        {
            let _ = std::fs::create_dir_all(parent_path);
        }

        let mut file_options = std::fs::File::options();
        file_options
            .read(false)
            .write(true)
            .create(true)
            .truncate(true);
        #[cfg(target_family = "windows")]
        {
            // Prevent any other process from writing the file
            file_options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
        }
        let mut file = file_options
            .open(&temporary_path)
            .internal("Failed to serialize storage bucket")?;

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

        file.write_all(header.as_bytes())
            .internal("Failed to serialize storage bucket")?;
        if count > 0 {
            bucket
                .sorted_index
                .write_to_file(&mut file)
                .internal("Failed to serialize storage bucket")?;

            bucket
                .entry
                .write_to_file(&mut file)
                .internal("Failed to serialize storage bucket")?;
        }

        if sync_data {
            file.sync_all()
                .internal("Failed to serialize storage bucket")?;
        }

        drop(file);

        if let Some(mut guard) = temporary_guard.take() {
            fs_util::rename_file(temporary_path.as_path(), path.as_path())
                .internal("Failed to serialize storage bucket")?;

            guard.success();
        }

        if sync_data
            && let Some(parent_path) = temporary_path.parent()
            && let Err(err) = fs_util::sync_dir(parent_path)
        {
            lore_base::lore_debug!("Failed to flush and sync immutable index directory: {err}");
        }

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
            lore_base::lore_spawn_blocking!(move || Self::deserialize_files(path))
                .await
                .internal("Task failed")??;

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
        let count = bucket.entry.len();
        if count == 0 {
            return Ok(());
        }

        // Ensure only one serialization/deserialization of this bucket is happening at any given time
        let _lock = bucket.serialize_lock.clone().lock_owned().await;

        // Atomically flip dirty from true to false; if it was already false another flush
        // task has already claimed this bucket.
        if !group.dirty[bucket_index].swap(false, atomic::Ordering::Relaxed) {
            return Ok(());
        }

        lore_base::lore_trace!(
            "Serialize immutable store group {group_index} bucket {bucket_index}"
        );

        let path = format_bucket_path(path, group_index, bucket_index);

        lore_base::lore_spawn_blocking!(move || {
            Self::serialize_files(bucket, group, bucket_index, path, sync_data)
        })
        .await
        .internal("Task failed")?
    }

    /// Serialize the bucket to its `.new` twin during a fan-out commit. Differs from the regular
    /// `serialize` path in two ways: (1) bypasses the `count == 0` early-exit and the
    /// `dirty.swap(false) → skip-if-was-false` short-circuit, because every `[0..committed_level]`
    /// bucket must be rewritten at the new layout to overwrite stale level-N files even if it's
    /// empty post-redistribute; (2) always clears dirty after claiming ownership. The clear is
    /// safe because the caller holds the bucket's read lock — no concurrent writer can set
    /// dirty=true while we hold it, so any post-release write will correctly re-set dirty and
    /// be picked up by the next flush, matching the regular `serialize` path's semantics.
    pub async fn serialize_to_new(
        bucket: OwnedRwLockReadGuard<ImmutableStoreBucket, ImmutableStoreBucket>,
        group: Arc<ImmutableStoreGroup>,
        path: &Path,
        group_index: usize,
        bucket_index: usize,
        sync_data: bool,
    ) -> Result<(), LocalImmutableStoreError> {
        let _lock = bucket.serialize_lock.clone().lock_owned().await;

        // Claim ownership of the bucket's current content. We hold the bucket's read lock so no concurrent writer can have set dirty between the time we decided to serialize and now.
        group.dirty[bucket_index].swap(false, atomic::Ordering::Relaxed);

        let final_path = format_bucket_path(path, group_index, bucket_index);
        let new_path = {
            let mut p = final_path.into_os_string();
            p.push(crate::local::fan_out::BUCKET_NEW_SUFFIX);
            PathBuf::from(p)
        };

        lore_base::lore_spawn_blocking!(move || {
            Self::serialize_files(bucket, group, bucket_index, new_path, sync_data)
        })
        .await
        .internal("Task failed")?
    }
}

impl ImmutableStoreGroup {
    async fn flush_packstore(&self, sync_data: bool) {
        self.packstore.flush_all(sync_data).await;
    }

    async fn flush_delayed(weak_ref: Weak<LocalImmutableStore>, group_index: usize, delay: u64) {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        if let Some(store) = weak_ref.upgrade()
            && let Some(path) = store.path.as_ref()
        {
            let group = store.group[group_index].clone();

            for bucket_index in 0..group.bucket.len() {
                // Atomic pre-check avoids acquiring the bucket RwLock for clean buckets.
                if !group.dirty[bucket_index].load(atomic::Ordering::Relaxed) {
                    continue;
                }
                let Some(bucket) = group.try_bucket(bucket_index).cloned() else {
                    continue;
                };
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
            let path = path.clone();
            let lock = lore_base::lore_spawn_blocking!(|| {
                if !path.exists() {
                    let _ = std::fs::create_dir_all(path.as_path());
                }
                FSLock::acquire_directory_lock(path)
            })
            .await
            .map_err(|err| io::Error::other(format!("Store lock task failed: {err}")))
            .flatten()
            .internal("Failed to acquire store lock")?;
            Some(lock)
        } else {
            None
        };

        let mut store = LocalImmutableStore {
            path: immutable_path.clone(),
            lock,
            group: Vec::with_capacity(GROUP_COUNT),
            settings,
            eviction: Semaphore::new(1),
            compaction: Semaphore::new(1),
            stop_gc: AtomicBool::new(false),
            compaction_reclaimed: AtomicU64::new(0),
            deserialize_all: Semaphore::new(1),
            deserialized_all: AtomicBool::new(false),
            instruments: StoreInstruments::default(),
            gc_counters: Arc::new(crate::maintenance::GcCounters::new()),
            #[cfg(feature = "failure_generator")]
            failure_generator,
        };

        // With per group packstores the minimum number of packfiles per group
        // can be set to 1 - and will then grow dynamically as needed
        const MIN_PACKFILE_COUNT: usize = 1;

        // Per-group level marker detection. For each group dir (if present on disk), first run
        // T10 recovery to roll forward any interrupted fan-out commit, then read the marker; if
        // the marker is missing, fall back to `settings.initial_fan_out_level` for fresh stores
        // or 256 for existing legacy stores (the pre-fan-out 256-bucket layout). `committed_level`
        // tracks the on-disk marker value (0 if absent) for the flush path's two-phase decision.
        let index_existed_on_disk = immutable_path
            .as_ref()
            .is_some_and(|p| p.join("index").exists());
        let mut bucket_counts: Vec<usize> = Vec::with_capacity(GROUP_COUNT);
        let mut committed_levels: Vec<usize> = Vec::with_capacity(GROUP_COUNT);
        let mut any_marker_seen = false;
        for group_index in 0..GROUP_COUNT {
            let (initial, committed) = if let Some(path) = immutable_path.as_deref() {
                let mut group_path: PathBuf = path.as_path().to_path_buf();
                group_path.push("index");
                let group_hex = format!("{:02x}", group_index as u8);
                group_path.push(&group_hex);

                // Roll forward any pending fan-out commit before reading the marker. After this returns the marker reflects the post-recovery state.
                if group_path.exists()
                    && let Err(err) =
                        crate::local::fan_out::recover_level_transition(&group_path, false)
                {
                    return Err(LocalImmutableStoreError::internal_with_context(
                        err,
                        "Failed to recover pending level transition for group",
                    ));
                }

                match crate::local::fan_out::read_level_marker(&group_path) {
                    Ok(Some(level)) => {
                        any_marker_seen = true;
                        (level, level)
                    }
                    Ok(None) => {
                        if index_existed_on_disk {
                            (BUCKET_COUNT, 0)
                        } else {
                            (store.settings.initial_fan_out_level, 0)
                        }
                    }
                    Err(err) => {
                        return Err(LocalImmutableStoreError::internal_with_context(
                            err,
                            "Failed to read level marker for group",
                        ));
                    }
                }
            } else {
                (store.settings.initial_fan_out_level, 0)
            };
            bucket_counts.push(initial);
            committed_levels.push(committed);
        }

        // Determine serialize_version per Decision 8. Fresh stores, stores with markers, and
        // existing stores with bucket files at any older version (v1-v3) all go to LazyFanOut.
        // Existing stores at the current pre-fan-out version (v4 LastAccessInEntry) with no
        // markers stay at v4 for backward compatibility (old binaries can still read them).
        let any_older_bucket_seen = if index_existed_on_disk
            && !any_marker_seen
            && let Some(path) = immutable_path.as_deref()
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

        for (group_index, &count) in bucket_counts.iter().enumerate() {
            let packpath = immutable_path.as_deref().map(|path| {
                let mut path = path.clone();
                path.reserve(16);
                path.push("index");
                {
                    use std::io::Write;
                    let mut hexstring: [u8; 2] = Default::default();
                    let mut cursor = io::Cursor::new(&mut hexstring[..]);
                    let _ = write!(&mut cursor, "{:02x}", group_index as u8);
                    path.push(std::str::from_utf8(&hexstring).unwrap_or_default());
                }
                path
            });
            store.group.push(Arc::new(ImmutableStoreGroup {
                bucket: [const { OnceLock::new() }; BUCKET_COUNT],
                dirty: std::array::from_fn(|_| AtomicBool::new(false)),
                bucket_count: std::sync::atomic::AtomicUsize::new(count),
                serialize_version: std::sync::atomic::AtomicU32::new(serialize_version),
                fan_out_threshold: store.settings.fan_out_threshold,
                committed_level: std::sync::atomic::AtomicUsize::new(committed_levels[group_index]),
                packstore: crate::PackStore::new(
                    packpath,
                    MIN_PACKFILE_COUNT,
                    Some(store.gc_counters.clone()),
                ),
                flush: Mutex::new(JoinSet::new()),
            }));
        }

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
                store
                    .clone()
                    .upgrade_global_packfiles(path, old_packpath.as_path())
                    .await?;
            }
        }

        Ok(store)
    }

    pub fn packstore(&self, group_index: usize) -> &crate::PackStore {
        &self.group[group_index].packstore
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
                if !self.settings.allow_partial_fragment {
                    lore_base::lore_error!(
                        "Partial deduplication not allowed without payload proof for {address}"
                    );
                    return Err(LocalImmutableStoreError::internal("Payload is required"));
                }
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
            let mut flush = group.flush.lock().await;
            let _ = flush.try_join_next();

            let stored_durable = fragment_flags & FragmentFlags::PayloadStoredDurable
                == FragmentFlags::PayloadStoredDurable;
            if (!stored_durable || self.settings.flush_background) && flush.is_empty() {
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
                .find(partition, address, StoreMatch::MatchFull)
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

    pub async fn find(
        &self,
        partition: Partition,
        address: Address,
        match_request: StoreMatch,
    ) -> Result<ImmutableStoreFindResult, LocalImmutableStoreError> {
        if match_request == StoreMatch::MatchNone {
            return Err(LocalImmutableStoreError::internal("Invalid query"));
        }

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
        let (match_slot, _, match_made) = Self::lookup(&bucket, partition, address, match_request);

        if match_made == StoreMatch::MatchNone {
            Ok(ImmutableStoreFindResult {
                group: group_index,
                ..Default::default()
            })
        } else {
            let index = bucket.sorted_index[match_slot] as usize;
            let data = &bucket.entry[index].data;

            let data = if data.flags & FragmentFlags::PayloadObliterated
                == FragmentFlags::PayloadObliterated
            {
                ImmutableData {
                    flags: FragmentFlags::PayloadObliterated.bits(),
                    ..Default::default()
                }
            } else {
                // Record the last access timestamp unless disabled
                if self.settings.atime {
                    // SAFETY: Treat the u64 as an atomic, it is guaranteed to exist from the read lock
                    // and stomping the value is safe and expected (contention is irrelevant)
                    unsafe {
                        AtomicU64::from_ptr(&data.last_access as *const u64 as *mut u64)
                            .store(Self::last_access(), atomic::Ordering::Release);
                    }
                    group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
                }

                *data
            };

            Ok(ImmutableStoreFindResult {
                group: group_index,
                data,
                matching: match_made,
            })
        }
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
                    stored_payloads
                        .entry(key)
                        .and_modify(|item: &mut (u32, u64)| item.1 = entry.data.last_access)
                        .or_insert((entry.data.size_payload, entry.data.last_access));
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
                let path = Arc::new(path.as_ref().clone());
                let store = self.clone();
                lore_base::lore_spawn!(serialize_tasks, async move {
                    let group = store.group[group_index].clone();
                    let bucket = group.bucket(bucket_index).clone();
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

        if self.stop_gc.load(atomic::Ordering::Relaxed) {
            return 0;
        }
        let Ok(_permit) = self.eviction.acquire().await else {
            lore_base::lore_warn!("Evict oldest failed to get permit");
            return 0;
        };
        if self.stop_gc.load(atomic::Ordering::Relaxed) {
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
                let key = entry.data.last_access;
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

        bucket.sorted_index = sorted_index;
        bucket.entry = entry;

        dirty.store(true, atomic::Ordering::Relaxed);

        evict_count
    }

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
        if group_index >= GROUP_COUNT {
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
            self.compaction_reclaimed
                .store(0, atomic::Ordering::Relaxed);
            if let Some(sink) = &sink {
                sink.compaction_begin(max_size as u64);
            }
        }

        let _ = self.deserialize_all_buckets().await;

        if self.stop_gc.load(atomic::Ordering::Relaxed) {
            return Ok(None);
        }
        let Ok(_permit) = self.compaction.acquire().await else {
            lore_base::lore_warn!("Compact packfiles failed to get permit");
            return Ok(None);
        };

        if group_index >= GROUP_COUNT {
            lore_base::lore_debug!("Packstore compactor starting fresh");

            group_index = 0;
        } else {
            lore_base::lore_debug!(
                "Packstore compactor continuing, current in progress is group {group_index}"
            );
        }

        if self.stop_gc.load(atomic::Ordering::Relaxed) {
            return Ok(None);
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
        while let Some(result) = tasks.join_next().await {
            final_result = final_result.and(
                result
                    .map_err(|_err| Internal::msg("Task failure"))
                    .map_err(StoreError::from)
                    .flatten(),
            );
        }

        group_index += parallel_group_count;

        if let Some(path) = self.path.as_ref() {
            let path = path.join(DOT_COMPACT);
            if group_index < GROUP_COUNT {
                let _ = std::fs::write(path, group_index.to_ne_bytes());
            } else {
                let _ = std::fs::remove_file(path);
            }
        }

        // Error out if any operation failed
        final_result?;

        if group_index < GROUP_COUNT {
            let total_size = self.packstore_total_size().await;
            lore_base::lore_debug!("Packstore compaction complete, new total size {total_size}");
            self.instruments
                .compaction
                .final_total_size
                .record(total_size as u64, &[]);
            Ok(Some(group_index))
        } else {
            if let Some(sink) = &sink {
                sink.compaction_end(self.compaction_reclaimed.load(atomic::Ordering::Relaxed));
            }
            Ok(None)
        }
    }

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
    ) -> Result<(), StoreError> {
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
        loop {
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
                    let path = Arc::new(path.as_ref().clone());
                    let bucket = group.bucket(bucket_index).clone();
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

        Ok(())
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

    fn last_access() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
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

            // Lock-free scan: skip the group entirely when no bucket is dirty. No dirty bucket means no put/store operation has touched this group since the last flush, so the packstore has no new content to flush AND no marker write is needed (an empty group with no marker defaults to bucket_count = 256 on reload, which is fine — empty groups have no entries to interpret at any level, and the first write to such a group will fire the two-phase commit at that point). This restores ~zero per-group overhead for empty groups in fresh-store flushes, matching pre-fan-out behaviour.
            let any_dirty = group
                .dirty
                .iter()
                .any(|flag| flag.load(atomic::Ordering::Relaxed));
            if !any_dirty {
                continue;
            }

            let path = path.clone();
            lore_base::lore_spawn!(tasks, async move {
                let mut first_err: Option<LocalImmutableStoreError> = None;

                // Fan-out trigger: if any dirty bucket exceeds threshold and we're below max level, redistribute entries before serializing.
                if let Err(err) =
                    maybe_fan_out_immutable_group(&group, path.as_ref(), group_index).await
                {
                    first_err = Some(err);
                }

                let active_buckets = group.bucket_count.load(atomic::Ordering::Relaxed);
                let committed_level = group.committed_level.load(atomic::Ordering::Relaxed);
                let group_path = {
                    let mut p = path.as_path().to_path_buf();
                    p.push("index");
                    p.push(format!("{:02x}", group_index as u8));
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
                    if let Err(e) = std::fs::create_dir_all(&group_path).map_err(|e| {
                        LocalImmutableStoreError::internal_with_context(
                            e,
                            "Failed to create group directory for fan-out commit",
                        )
                    }) {
                        first_err = Some(e);
                    }

                    let mut wrote_new: Vec<usize> = Vec::new();
                    if first_err.is_none() {
                        for bucket_index in 0..active_buckets {
                            // Fast path: skip the bucket entirely (no lock acquire) when it's neither dirty nor an old-level slot we need to overwrite. The dirty flag is the cheap proxy for "this bucket has data to flush"; combined with the index < committed_level check (which forces an empty .new to overwrite stale level-N files), this avoids 256× read-lock acquires per group on the common server-fresh-store first flush where most buckets are empty and committed_level == 0.
                            let must_overwrite_old = bucket_index < committed_level;
                            let dirty = group.dirty[bucket_index].load(atomic::Ordering::Relaxed);
                            if !must_overwrite_old && !dirty {
                                continue;
                            }
                            let bucket = group.bucket(bucket_index).clone().read_owned().await;
                            // Re-check after lock acquire — concurrent paths may have just dirtied or undirtied this bucket.
                            if bucket.entry.is_empty() && !must_overwrite_old {
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
                                let new_path = crate::local::fan_out::bucket_new_path(
                                    &group_path,
                                    bucket_index,
                                );
                                let final_path =
                                    crate::local::fan_out::bucket_path(&group_path, bucket_index);
                                if let Err(err) = std::fs::rename(&new_path, &final_path)
                                    && first_err.is_none()
                                {
                                    first_err = Some(
                                        LocalImmutableStoreError::internal_with_context(
                                            err,
                                            "Failed to rename .new bucket file during fan-out commit",
                                        ),
                                    );
                                }
                            }
                        }

                        if first_err.is_none()
                            && let Err(err) = crate::local::fan_out::write_level_marker(
                                &group_path,
                                active_buckets,
                                sync_data,
                            )
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
                            && let Err(err) = crate::local::fan_out::delete_level_pending(
                                &group_path,
                            )
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
                    // Regular flush at unchanged level: per-file .tmp + atomic rename for dirty buckets only. No marker write — marker already reflects the current level.
                    for bucket_index in 0..active_buckets {
                        if !group.dirty[bucket_index].load(atomic::Ordering::Relaxed) {
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
                    let last_access = bucket.entry[*index as usize].data.last_access;
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

    async fn exist(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        Ok(self
            .find(partition, address, match_requested)
            .await
            .forward_with::<StoreError, _>(|| {
                format!("Failed to query immutable store {}.", address.hash)
            })?
            .matching)
    }

    async fn exist_batch(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let mut output = vec![];

        for address in addresses {
            output.push(
                self.clone()
                    .exist(partition, *address, match_requested)
                    .await?,
            );
        }

        Ok(output)
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        let find = self
            .find(partition, address, match_requested)
            .await
            .forward_with::<StoreError, _>(|| {
                format!("Failed to query immutable store {}.", address.hash)
            })?;

        let mut local_flags = 0;
        if find.data.pack_file != 0 {
            local_flags |= FragmentFlags::PayloadStoredLocal.bits();
        }
        if self.settings.implicit_durable_stored {
            local_flags |= FragmentFlags::PayloadStoredDurable.bits();
        }

        Ok(StoreQueryResult {
            fragment: Fragment {
                flags: find.data.flags | local_flags,
                size_payload: find.data.size_payload,
                size_content: find.data.size_content,
            },
            match_made: find.matching,
        })
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        #[cfg(feature = "failure_generator")]
        if self.failure_generator.retry_rate > 0.0
            && rand::random::<f32>() < self.failure_generator.retry_rate
        {
            return Err(StoreError::from(SlowDown));
        }

        let find = self
            .find(partition, address, match_required)
            .await
            .forward_with::<StoreError, _>(|| {
                format!("Failed to query immutable store for get {}.", address.hash)
            })?;

        if find.matching == match_required {
            let mut local_flags = 0;
            if self.settings.implicit_durable_stored {
                local_flags |= FragmentFlags::PayloadStoredDurable.bits();
            }

            let fragment = Fragment {
                flags: find.data.flags | local_flags,
                size_payload: find.data.size_payload,
                size_content: find.data.size_content,
            };
            if find.data.pack_file != 0 {
                crate::validate_fragment_payload(&fragment, find.data.size_payload as usize)?;
                let payload = Self::load(&self.group[find.group].packstore, find.data)
                    .await
                    .forward::<StoreError>("Failed to load payload from local storage.")?;
                crate::validate_fragment_payload(&fragment, payload.len())?;
                Ok((fragment, payload))
            } else {
                Err(StoreError::from(PayloadNotFound::from(address.hash)))
            }
        } else {
            Err(StoreError::from(AddressNotFound::from(address)))
        }
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        mut fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
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
            .find(partition, address, StoreMatch::MatchFull)
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
        timed!(
            self.instruments.operation_latency,
            &self.instruments.get_labels_for_operation_context("obliterate"),
            {
                let group_index = address.hash.data()[0] as usize;
                lore_base::lore_debug!("Obliterating address {address}");

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
                    Box::pin(bucket
                        .deserialize(
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

                lore_base::lore_debug!("Lookup match for {address}: {match_made:?}");

                if match_made != StoreMatch::MatchFull {
                    return Err(StoreError::from(AddressNotFound::from(address)));
                }

                let index = bucket.sorted_index[match_slot] as usize;
                let entry = &bucket.entry[index];

                if (entry.data.flags & FragmentFlags::PayloadFragmented) != 0 {
                    lore_base::lore_debug!("Payload fragmented, obliterating subfragments");

                    if let Ok(payload) = Self::load(&group.packstore, entry.data).await.inspect_err(|e| {
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

                if entry.data.flags & FragmentFlags::PayloadObliterated.bits() == FragmentFlags::PayloadObliterated {
                    lore_base::lore_warn!("Address {address} already obliterated");
                    return Ok(());
                }

                if is_last_fragment && entry.data.pack_file != 0 {
                    lore_base::lore_debug!(
                        "Fragment payload has no other references, obliterating from packstore"
                    );

                    stats
                        .num_payloads
                        .fetch_add(1, atomic::Ordering::Relaxed);

                    group.packstore
                        .obliterate(
                            entry.data.pack_file,
                            entry.data.pack_offset,
                            entry.data.size_payload,
                        )
                        .await
                        .forward::<StoreError>(
                            "Failed to obliterate payload from pack store.",
                        )?;
                }

                stats
                    .num_fragments
                    .fetch_add(1, atomic::Ordering::Relaxed);

                bucket.entry[index].data = ImmutableData {
                    flags: FragmentFlags::PayloadObliterated.bits(),
                    size_payload: 0,
                    size_content: 0,
                    pack_file: 0,
                    pack_offset: 0,
                    last_access: 0
                };

                group.dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
                drop(bucket);

                let mut flush = group.flush.lock().await;
                let _ = flush.try_join_next();

                if flush.is_empty() {
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
        ).into()
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        _sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        timed!(
            self.instruments.operation_latency,
            &self.instruments.get_labels_for_operation_context("evict"),
            Ok(self.evict_oldest(max_capacity, sink.as_ref()).await)
        )
        .into()
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<crate::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        timed!(
            self.instruments.operation_latency,
            &self.instruments.get_labels_for_operation_context("compact"),
            {
                self.clone()
                    .compact_packfiles(max_size, at, sync_data, sink)
                    .await
            }
        )
        .into()
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        if let Some(path) = self.path.as_deref() {
            tokio::fs::read(path.join(DOT_COMPACT))
                .await
                .ok()
                .and_then(|bytes| {
                    lore_base::lore_debug!("Reading compactor resume point");
                    bytes.try_into().ok().map(usize::from_ne_bytes)
                })
        } else {
            None
        }
    }

    async fn compact_stop(self: Arc<Self>) {
        self.stop_gc.store(true, atomic::Ordering::Relaxed);
        {
            let _evict = self.eviction.acquire().await;
        }
        {
            let _compact = self.compaction.acquire().await;
        }
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
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
                let _ = crate::immutable_store::ImmutableStore::flush(self, false).await;

                lore_base::lore_debug!("Store healing complete");
            }
        }
        lore_base::lore_debug!("Store verification complete");

        Ok(())
    }

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
        if let Some(path) = self.path.as_ref() {
            self.clone()
                .flush_all(Some(path.clone()), sync_data)
                .await
                .forward("Failed to flush store to disk")
        } else {
            Ok(())
        }
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn fragment_count(self: Arc<Self>) -> Option<usize> {
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
        // Hash is preserved across the copy; the destination address only differs in context.
        // Same hash → same bucket, so source and destination always live in one bucket — including
        // the same-partition different-context case used for in-partition payload dedup.
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

        let (source_slot, _, source_match) = Self::lookup(
            &bucket,
            source_partition,
            source_address,
            StoreMatch::MatchFull,
        );

        if source_match != StoreMatch::MatchFull {
            return Err(StoreError::from(AddressNotFound::from(source_address)));
        }

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

        for data in entries {
            if data.pack_file == 0 {
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

        if heal && !failed_data.is_empty() {
            let mut bucket = bucket_ref.write().await;

            for entry in bucket.entry.iter_mut() {
                if entry.address.hash == address.hash
                    && failed_data.iter().any(|f| {
                        entry.data.pack_file == f.pack_file
                            && entry.data.pack_offset == f.pack_offset
                    })
                {
                    entry.data.pack_file = 0;
                    entry.data.pack_offset = 0;
                }
            }

            self.group[group_index].dirty[bucket_index].store(true, atomic::Ordering::Relaxed);
            drop(bucket);

            let _ = crate::immutable_store::ImmutableStore::flush(self, false).await;
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
        if count > 0 {
            group.dirty[new_idx].store(true, atomic::Ordering::Relaxed);
            // Mark deserialized so subsequent operations don't try to re-read from disk.
            // Note: ImmutableStoreBucket has private `deserialized` and `upgrade_packfile` fields;
            // the redistribute mutates entry/sorted_index directly while leaving them at their
            // previous values, which is safe since we hold the write lock.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bucket_file(path: &Path, version: u32) {
        let entry = ImmutableStoreEntry::default();
        let mut header = ImmutableStoreHeader::new_zeroed();
        header.version = version;
        header.count = 1;
        let mut bytes = Vec::with_capacity(
            size_of::<ImmutableStoreHeader>() + 4 + size_of::<ImmutableStoreEntry>(),
        );
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(entry.as_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn lazy_fan_out_version_is_five() {
        assert_eq!(ImmutableStoreVersion::LazyFanOut as u32, 5);
    }

    #[test]
    fn deserialize_accepts_last_access_in_entry_v4() {
        let dir = crate::test_util::TempDir::new("is_v4_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, ImmutableStoreVersion::LastAccessInEntry as u32);
        let result = ImmutableStoreBucket::deserialize_files(path);
        assert!(
            result.is_ok(),
            "v4 (LastAccessInEntry) bucket should deserialize"
        );
    }

    #[test]
    fn deserialize_accepts_lazy_fan_out_v5() {
        let dir = crate::test_util::TempDir::new("is_v5_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, ImmutableStoreVersion::LazyFanOut as u32);
        let result = ImmutableStoreBucket::deserialize_files(path);
        assert!(result.is_ok(), "v5 (LazyFanOut) bucket should deserialize");
    }

    #[test]
    fn deserialize_rejects_unknown_future_version() {
        let dir = crate::test_util::TempDir::new("is_v100_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, 100);
        let result = ImmutableStoreBucket::deserialize_files(path.clone());
        assert!(result.is_err(), "v100 bucket should be rejected as too new");
        // Future-version files MUST be preserved on disk — recovery would clobber data
        // written by a newer binary.
        assert!(
            path.exists(),
            "future-version bucket file must be preserved, not deleted"
        );
    }

    /// Write a v5 bucket file whose header claims `header_count` entries but contains
    /// only `actual_entries_on_disk` entry slots — the crash-mid-flush shape.
    fn write_bucket_file_with_count_mismatch(
        path: &Path,
        header_count: u32,
        actual_entries_on_disk: u32,
    ) {
        let mut header = ImmutableStoreHeader::new_zeroed();
        header.version = ImmutableStoreVersion::LazyFanOut as u32;
        header.count = header_count;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(header.as_bytes());
        for i in 0..actual_entries_on_disk {
            bytes.extend_from_slice(&i.to_le_bytes());
        }
        for _ in 0..actual_entries_on_disk {
            let entry = ImmutableStoreEntry::default();
            bytes.extend_from_slice(entry.as_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn deserialize_recovers_from_bad_count_header() {
        // Mirrors the production server log shape (header.count > what file fits).
        let dir = crate::test_util::TempDir::new("is_badcount_");
        let path = dir.path().join("bucket");
        write_bucket_file_with_count_mismatch(&path, 670, 518);
        let result = ImmutableStoreBucket::deserialize_files(path.clone());
        let (sorted_index, entry, _, mark_dirty) =
            result.expect("count-mismatch corruption must recover");
        assert!(sorted_index.is_empty());
        assert!(entry.is_empty());
        assert!(!mark_dirty);
        assert!(!path.exists(), "corrupt bucket file must be removed");
    }

    #[test]
    fn deserialize_recovers_from_invalid_version() {
        // 0xFFFF is above the future-version sentinel range, so it's corruption.
        let dir = crate::test_util::TempDir::new("is_badver_");
        let path = dir.path().join("bucket");
        write_bucket_file(&path, 0xFFFF);
        let result = ImmutableStoreBucket::deserialize_files(path.clone());
        let (sorted_index, entry, _, _) = result.expect("invalid-version corruption must recover");
        assert!(sorted_index.is_empty());
        assert!(entry.is_empty());
        assert!(!path.exists(), "corrupt bucket file must be removed");
    }

    #[test]
    fn deserialize_recovers_from_truncated_entries() {
        let dir = crate::test_util::TempDir::new("is_trunc_");
        let path = dir.path().join("bucket");
        let mut header = ImmutableStoreHeader::new_zeroed();
        header.version = ImmutableStoreVersion::LazyFanOut as u32;
        header.count = 3;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(header.as_bytes());
        for i in 0u32..3 {
            bytes.extend_from_slice(&i.to_le_bytes());
        }
        let entry = ImmutableStoreEntry::default();
        bytes.extend_from_slice(entry.as_bytes());
        bytes.extend_from_slice(&entry.as_bytes()[..size_of::<ImmutableStoreEntry>() / 2]);
        std::fs::write(&path, bytes).unwrap();

        let result = ImmutableStoreBucket::deserialize_files(path.clone());
        let (sorted_index, entry, _, _) =
            result.expect("truncated-entries corruption must recover");
        assert!(sorted_index.is_empty());
        assert!(entry.is_empty());
        assert!(!path.exists(), "corrupt bucket file must be removed");
    }

    #[test]
    fn deserialize_recovers_from_short_header() {
        // File too small to even hold the header.
        let dir = crate::test_util::TempDir::new("is_shorthdr_");
        let path = dir.path().join("bucket");
        std::fs::write(&path, [0u8; 4]).unwrap();
        let result = ImmutableStoreBucket::deserialize_files(path.clone());
        let (sorted_index, entry, _, _) = result.expect("short-header corruption must recover");
        assert!(sorted_index.is_empty());
        assert!(entry.is_empty());
        assert!(!path.exists(), "corrupt bucket file must be removed");
    }

    #[tokio::test]
    async fn store_recovers_from_corrupt_bucket_and_remains_usable() {
        // End-to-end: corrupt a bucket file and verify the store is still usable for
        // writes and reads on that bucket. Original content is lost (expected).
        use crate::options::ReadOptions;
        use crate::options::WriteOptions;
        use crate::read::read;
        use crate::write::write_content;

        let dir = crate::test_util::TempDir::new("is_e2e_recover_");
        let store_path = dir.path().to_path_buf();
        let partition = Partition::from([0x42u8; 16]);
        let context = Context::from([0x07u8; 16]);
        let payload = Bytes::from(vec![0xCDu8; 256]);

        let address = {
            let store: Arc<dyn crate::immutable_store::ImmutableStore> = create(
                Some(&store_path),
                ImmutableStoreCreateOptions::none(),
                false,
                ImmutableStoreSettings {
                    initial_fan_out_level: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

            let (address, _) = write_content(
                store.clone(),
                partition,
                context,
                payload.clone(),
                WriteOptions::default(),
                None,
                None,
            )
            .await
            .unwrap();

            store.clone().flush(true).await.unwrap();
            address
        };

        // initial_fan_out_level=1 → bucket index is always 0; group is hash[0].
        let group_index = address.hash.data()[0] as usize;
        let bucket_path = store_path
            .join("immutable")
            .join("index")
            .join(format!("{group_index:02x}"))
            .join("index_00");
        assert!(
            bucket_path.exists(),
            "bucket file should exist after flush at {bucket_path:?}"
        );

        // Crash-mid-flush shape: header claims N entries, body is short.
        let mut header = ImmutableStoreHeader::new_zeroed();
        header.version = ImmutableStoreVersion::LazyFanOut as u32;
        header.count = 4096;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(&[0u8; size_of::<u32>() * 4]);
        std::fs::write(&bucket_path, bytes).unwrap();

        let store: Arc<dyn crate::immutable_store::ImmutableStore> = create(
            Some(&store_path),
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Original content is gone (data lost), but the bucket is operational.
        let read_result = read(
            store.clone(),
            partition,
            address,
            None,
            ReadOptions::default(),
            None,
        )
        .await;
        assert!(
            read_result.is_err(),
            "originally stored content must be reported missing after recovery"
        );

        let (new_address, _) = write_content(
            store.clone(),
            partition,
            context,
            payload.clone(),
            WriteOptions::default(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(new_address, address);

        let bytes = read(
            store.clone(),
            partition,
            new_address,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(bytes.as_ref(), payload.as_ref());
    }

    #[test]
    fn immutable_store_settings_default_includes_fan_out_fields() {
        let s = ImmutableStoreSettings::default();
        assert_eq!(s.initial_fan_out_level, 1);
        assert_eq!(
            s.fan_out_threshold,
            crate::local::fan_out::FAN_OUT_THRESHOLD_DEFAULT
        );
    }

    #[tokio::test]
    async fn store_initializes_group_bucket_count_from_settings_level_1() {
        use std::sync::atomic::Ordering;
        let store = LocalImmutableStore::new(
            None,
            ImmutableStoreSettings {
                initial_fan_out_level: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for group in &store.group {
            assert_eq!(group.bucket_count.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn store_initializes_group_bucket_count_from_settings_level_256() {
        use std::sync::atomic::Ordering;
        let store = LocalImmutableStore::new(
            None,
            ImmutableStoreSettings {
                initial_fan_out_level: crate::local::fan_out::FAN_OUT_LEVEL_MAX,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for group in &store.group {
            assert_eq!(
                group.bucket_count.load(Ordering::Relaxed),
                crate::local::fan_out::FAN_OUT_LEVEL_MAX
            );
        }
    }

    fn payload_data(pack_file: u32, encoding: u32, storage: u32) -> ImmutableData {
        ImmutableData {
            flags: encoding | storage,
            size_payload: if pack_file == 0 { 0 } else { 100 },
            size_content: 256,
            pack_offset: if pack_file == 0 { 0 } else { 200 },
            pack_file,
            last_access: 0,
        }
    }

    #[test]
    fn merge_from_copy_source_adopts_payload_and_encoding() {
        // Target had its own uncompressed payload. Source has the same content stored
        // compressed in a different pack file. After merge, target adopts source's pack
        // pointer and the encoding flag that describes those bytes — keeping target's
        // pre-existing flags would mis-describe the new payload.
        let mut target = payload_data(1, 0, 0);
        let source = payload_data(2, FragmentFlags::PayloadCompressedZstd.bits(), 0);

        target.merge_from_copy_source(source, false);

        assert_eq!(target.pack_file, 2, "target adopts source's pack_file");
        assert_eq!(target.pack_offset, 200);
        assert_eq!(target.size_payload, 100);
        assert_ne!(
            target.flags & FragmentFlags::PayloadCompressedZstd.bits(),
            0,
            "encoding flag must follow the adopted payload",
        );
        assert_ne!(
            target.flags & FragmentFlags::PayloadStoredLocal.bits(),
            0,
            "adopted bytes are locally available",
        );
    }

    #[test]
    fn merge_from_copy_source_preserves_target_durable() {
        // Target had PayloadStoredDurable from a prior remote round-trip on the destination
        // tuple. A subsequent local copy must not unset that bit.
        let mut target = payload_data(0, 0, FragmentFlags::PayloadStoredDurable.bits());
        let source = payload_data(2, 0, FragmentFlags::PayloadStoredDurable.bits());

        target.merge_from_copy_source(source, false);

        assert_ne!(
            target.flags & FragmentFlags::PayloadStoredDurable.bits(),
            0,
            "target's prior Durable must be preserved",
        );
    }

    #[test]
    fn merge_from_copy_source_durable_only_from_caller() {
        // Source carries Durable; target had none. With `durable=false`, source's Durable
        // must NOT propagate. With `durable=true`, the caller's intent sets the bit.
        let source = payload_data(2, 0, FragmentFlags::PayloadStoredDurable.bits());

        let mut local_only = payload_data(0, 0, 0);
        local_only.merge_from_copy_source(source, false);
        assert_eq!(
            local_only.flags & FragmentFlags::PayloadStoredDurable.bits(),
            0,
            "Durable must not propagate from source on a local-only copy",
        );

        let mut remote_confirmed = payload_data(0, 0, 0);
        remote_confirmed.merge_from_copy_source(source, true);
        assert_ne!(
            remote_confirmed.flags & FragmentFlags::PayloadStoredDurable.bits(),
            0,
            "caller's `durable=true` sets the bit",
        );
    }

    #[tokio::test]
    async fn copy_adopts_source_payload_and_decompresses_through_target_partition() {
        // Prime target with uncompressed payload at one address, prime source with the same
        // hash but compressed payload, then copy source → target. The target entry must
        // adopt source's payload pointer along with the matching encoding flag so a read
        // against the target partition decompresses correctly and returns the original bytes.
        use std::sync::atomic::Ordering;

        use crate::compress::COMPRESSION_MODE;
        use crate::compress::CompressionMode;
        use crate::options::ReadOptions;
        use crate::options::WriteOptions;
        use crate::read::read;
        use crate::write::write_content;

        let store: Arc<dyn crate::immutable_store::ImmutableStore> = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .unwrap();

        let target_partition = Partition::from([0x01u8; 16]);
        let source_partition = Partition::from([0x02u8; 16]);
        let context = Context::from([0x03u8; 16]);
        // Highly compressible content so compression actually triggers when enabled.
        let payload: Vec<u8> = vec![0xABu8; 4096];

        // Prime target (uncompressed).
        let prev_mode =
            COMPRESSION_MODE.swap(CompressionMode::NoCompression as u32, Ordering::AcqRel);
        let (target_address, _target_fragment) = write_content(
            store.clone(),
            target_partition,
            context,
            Bytes::from(payload.clone()),
            WriteOptions::default(),
            None,
            None,
        )
        .await
        .unwrap();

        // Prime source (compressed).
        COMPRESSION_MODE.store(CompressionMode::Zstd as u32, Ordering::Release);
        let (source_address, _source_fragment) = write_content(
            store.clone(),
            source_partition,
            context,
            Bytes::from(payload.clone()),
            WriteOptions::default(),
            None,
            None,
        )
        .await
        .unwrap();
        // Restore mode for any other tests sharing this process.
        COMPRESSION_MODE.store(prev_mode, Ordering::Release);

        assert_eq!(target_address, source_address, "same content → same hash");

        // Copy source → target with durable=false (pure local). Pass the source context as the
        // destination context so the address tuple is preserved (cross-partition copy with the
        // hash + context invariant the original test relied on).
        store
            .clone()
            .copy(
                source_partition,
                source_address,
                target_partition,
                source_address.context,
                false,
            )
            .await
            .unwrap();

        // Read from target partition: payload bytes must round-trip identically. The helper
        // must have adopted source's pack pointer AND encoding flag together — if encoding
        // and bytes were ever desynchronized, decompression in `read` would fail or return
        // garbage.
        let bytes = read(
            store.clone(),
            target_partition,
            target_address,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(bytes.as_ref(), payload.as_slice());
    }

    #[tokio::test]
    async fn copy_same_partition_new_context_adopts_payload_without_transfer() {
        // Same partition, different context — the in-partition deduplication path. The destination
        // entry must adopt the source's payload pointer (no payload transfer) and the read against
        // the new `(partition, hash, target_context)` tuple must return the same bytes that were
        // originally written under the source context.
        use crate::options::ReadOptions;
        use crate::options::WriteOptions;
        use crate::read::read;
        use crate::write::write_content;

        let store: Arc<dyn crate::immutable_store::ImmutableStore> = create(
            None::<&Path>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings::default(),
        )
        .await
        .unwrap();

        let partition = Partition::from([0xA1u8; 16]);
        let source_context = Context::from([0xB1u8; 16]);
        let target_context = Context::from([0xB2u8; 16]);
        let payload: Vec<u8> = b"in-partition new-context dedup payload".to_vec();

        // Seed the source tuple `(partition, hash, source_context)`.
        let (source_address, _) = write_content(
            store.clone(),
            partition,
            source_context,
            Bytes::from(payload.clone()),
            WriteOptions::default(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(source_address.context, source_context);

        // Copy within the same partition, retagging the destination with `target_context`. The
        // store's `copy` is the only call we make — there must be no payload transfer; the
        // destination tuple gets its own entry that points at the source's payload data.
        store
            .clone()
            .copy(partition, source_address, partition, target_context, false)
            .await
            .unwrap();

        // The destination address shares the source's hash but takes the target context.
        let destination_address = Address {
            hash: source_address.hash,
            context: target_context,
        };

        let bytes = read(
            store.clone(),
            partition,
            destination_address,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(bytes.as_ref(), payload.as_slice());

        // Source tuple must remain readable independently — copy creates a new entry, it does
        // not consume or repoint the source.
        let bytes = read(
            store.clone(),
            partition,
            source_address,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(bytes.as_ref(), payload.as_slice());
    }
}
